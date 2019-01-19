//! Image representations for ffi.
//!
//! # Usage
//!
//! Imagine you want to offer a very simple ffi interface: The caller provides an image buffer and
//! your program creates a thumbnail from it and dumps that image as `png`. This module is designed
//! to help you transition from raw memory data to Rust representation.
//! 
//! ```no_run
//! use std::ptr;
//! use std::slice;
//! use image::Rgb;
//! use image::flat::{FlatSamples, SampleLayout};
//! use image::imageops::thumbnail;
//!
//! #[no_mangle]
//! pub extern "C" fn store_rgb8_compressed(
//!     data: *const u8, len: usize,
//!     format: *const SampleLayout
//! )
//!     -> bool
//! {
//!     let samples = unsafe { slice::from_raw_parts(data, len) };
//!     let format = unsafe { ptr::read(format) };
//!
//!     let buffer = FlatSamples {
//!         samples,
//!         format,
//!         color_hint: None,
//!     };
//!
//!     let view = match buffer.as_view::<Rgb<u8>>() {
//!         Err(_) => return false, // Invalid format.
//!         Ok(view) => view,
//!     };
//!
//!     thumbnail(&view, 64, 64)
//!         .save("output.png")
//!         .map(|_| true)
//!         .unwrap_or_else(|_| false)
//! }
//! ```
//! 
use std::cmp;
use std::ops::{Deref, Index, IndexMut};
use std::marker::PhantomData;

use num_traits::Zero;

use buffer::{ImageBuffer, Pixel};
use color::ColorType;
use image::{GenericImage, GenericImageView, ImageError};

/// A flat buffer over a (multi channel) image.
///
/// In contrast to `ImageBuffer`, this representation of a sample collection is much more lenient
/// in the layout thereof. In particular, it also allows grouping by color planes instead of by
/// pixel, at least for the purpose of a `GenericImageView`.
///
/// Note that the strides need not conform to the assumption that constructed indices actually
/// refer inside the underlying buffer but return values of library functions will always guarantee
/// this. To manually make this check use `check_index_validities` and maybe put that inside an
/// assert.
#[derive(Clone, Debug)]
pub struct FlatSamples<Buffer> {
    /// Underlying linear container holding sample values.
    pub samples: Buffer,

    /// A `repr(C)` description of the buffer format.
    pub format: SampleLayout,

    /// Supplementary color information.
    ///
    /// You may keep this as `None` in most cases. This is NOT checked in `View` or other
    /// converters. It is intended mainly as a way for types that convert to this buffer type to
    /// attach their otherwise static color information. A dynamic image representation could
    /// however use this to resolve representational ambiguities such as the order of RGB channels.
    pub color_hint: Option<ColorType>,
}

/// A ffi compatible description of a sample buffer.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SampleLayout {
    /// The number of channels in the color representation of the image.
    pub channels: u8,

    /// Add this to an index to get to the sample in the next channel.
    pub channel_stride: usize,

    /// The width of the represented image.
    pub width: u32,

    /// Add this to an index to get to the next sample in x-direction.
    pub width_stride: usize,

    /// The height of the represented image.
    pub height: u32,

    /// Add this to an index to get to the next sample in y-direction.
    pub height_stride: usize,
}

/// Helper struct for an unnamed (stride, length) pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Dim(usize, usize);

impl SampleLayout {
    /// Get the strides for indexing matrix-like [(c, w, h)].
    ///
    /// For a row-major layout with grouped samples, this tuple is strictly
    /// increasing.
    pub fn strides_cwh(&self) -> (usize, usize, usize) {
        (self.channel_stride, self.width_stride, self.height_stride)
    }

    /// Get the dimensions (channels, width, height).
    ///
    /// The interface is optimized for use with `strides_cwh` instead. The channel extent will be
    /// before width and height.
    pub fn extents(&self) -> (usize, usize, usize) {
        (self.channels as usize, self.width as usize, self.height as usize)
    }

    /// Tuple of bounds in the order of coordinate inputs.
    ///
    /// This function should be used whenever working with image coordinates opposed to buffer
    /// coordinates.
    pub fn bounds(&self) -> (u8, u32, u32) {
        (self.channels, self.width, self.height)
    }

    /// Get the minimum length of a buffer such that all in-bounds samples have valid indices.
    /// 
    /// This method will allow zero strides, allowing compact representations of monochrome images.
    /// To check that no aliasing occurs, try `check_alias_invariants`. For compact images (no
    /// aliasing and no unindexed samples) this is `width*height*channels`. But for both of the
    /// other cases, the reasoning is slightly more involved.
    ///
    /// # Explanation
    ///
    /// Note that there is a difference between `min_length` and the index of the sample
    /// 'one-past-the-end`. This is due to strides that may be larger than the dimension below.
    ///
    /// ## Example with holes
    ///
    /// Let's look at an example of a grayscale image with 
    /// * `width_stride = 1`
    /// * `width = 2`
    /// * `height_stride = 3`
    /// * `height = 2`
    ///
    /// ```text
    /// | x x   | x x m | $
    ///  min_length m ^
    ///                   ^ one-past-the-end $
    /// ```
    ///
    /// The difference is also extreme for empty images with large strides. The one-past-the-end
    /// sample index is still as large as the largest of these strides while `min_length = 0`.
    ///
    /// ## Example with aliasing
    ///
    /// The concept gets even more important when you allow samples to alias each other. Here we
    /// have the buffer of a small grayscale image where this is the case, this time we will first
    /// show the buffer and then the individual rows below.
    ///
    /// * `width_stride = 1`
    /// * `width = 3`
    /// * `height_stride = 2`
    /// * `height = 2`
    ///
    /// ```text
    ///  1 2 3 4 5 m
    /// |1 2 3| row one
    ///     |3 4 5| row two
    ///            ^ m min_length
    ///          ^ ??? one-past-the-end
    /// ```
    ///
    /// This time 'one-past-the-end' is not even simply the largest stride times the extent of its
    /// dimension. That still points inside the image because `height*height_stride = 4` but also
    /// `index_of(1, 2) = 4`.
    pub fn min_length(&self) -> Option<usize> {
        if self.width == 0 || self.height == 0 || self.channels == 0 {
            return Some(0)
        }

        self.index(self.channels - 1, self.width - 1, self.height - 1)
            .and_then(|idx| idx.checked_add(1))
    }

    /// Check if the buffer is large enough.
    pub fn fits(&self, len: usize) -> bool {
        self.min_length().map(|min| len >= min).unwrap_or(false)
    }

    /// The extents of this array, in order of increasing strides.
    fn increasing_stride_dims(&self) -> [Dim; 3] {
        // Order extents by strides, then check that each is less equal than the next stride.
        let mut grouped: [Dim; 3] = [
            Dim(self.channel_stride, self.channels as usize),
            Dim(self.width_stride, self.width as usize),
            Dim(self.height_stride, self.height as usize)];

        grouped.sort();

        let (min_dim, mid_dim, max_dim) = (grouped[0], grouped[1], grouped[2]);
        assert!(min_dim.stride() <= mid_dim.stride() && mid_dim.stride() <= max_dim.stride());
        
        grouped
    }

    /// If there are any samples aliasing each other.
    ///
    /// If this is not the case, it would always be safe to allow mutable access to two different
    /// samples at the same time. Otherwise, this operation would need additional checks. When one
    /// dimension overflows `usize` with its stride we also consider this aliasing.
    pub fn has_aliased_samples(&self) -> bool {
        let grouped = self.increasing_stride_dims();
        let (min_dim, mid_dim, max_dim) = (grouped[0], grouped[1], grouped[2]);

        let min_size = match min_dim.checked_len() {
            None => return true,
            Some(size) => size,
        };

        let mid_size = match mid_dim.checked_len() {
            None => return true,
            Some(size) => size,
        };

        let _max_size = match max_dim.checked_len() {
            None => return true,
            Some(_) => (), // Only want to know this didn't overflow.
        };

        // Each higher dimension must walk over all of one lower dimension.
        min_size > mid_dim.stride() || mid_size > max_dim.stride()
    }

    /// Check if a buffer fulfills the requirements of a normal form.
    ///
    /// Certain conversions have preconditions on the structure of the sample buffer that are not
    /// captured (by design) by the type system. These are then checked before the conversion. Such
    /// checks can all be done in constant time and will not inspect the buffer content. You can
    /// perform these checks yourself when the conversion is not required at this moment but maybe
    /// still performed later.
    pub fn is_normal(&self, form: NormalForm) -> bool {
        if self.has_aliased_samples() {
            return false;
        }

        if form >= NormalForm::PixelPacked && self.channel_stride != 1 {
            return false;
        }

        if form >= NormalForm::ImagePacked {
            // has aliased already checked for overflows.
            let grouped = self.increasing_stride_dims();
            let (min_dim, mid_dim, max_dim) = (grouped[0], grouped[1], grouped[2]);

            if 1 != min_dim.stride() {
                return false;
            }

            if min_dim.len() != mid_dim.stride() {
                return false;
            }

            if  mid_dim.len() != max_dim.stride() {
                return false;
            }
        }

        if form >= NormalForm::RowMajorPacked {
            if self.width_stride != self.channels as usize {
                return false;
            }

            if self.width as usize*self.width_stride != self.height_stride {
                return false;
            }
        }

        if form >= NormalForm::ColumnMajorPacked {
            if self.height_stride != self.channels as usize {
                return false;
            }
            
            if self.height as usize*self.height_stride != self.width_stride {
                return false;
            }
        }

        return true;
    }

    /// Check that the pixel and the channel index are in bounds.
    pub fn in_bounds(&self, channel: u8, x: u32, y: u32) -> bool {
        return channel < self.channels && x < self.width && y < self.height
    }

    /// Resolve the index of a particular sample.
    ///
    /// `None` if the index is outside the bounds or does not fit into a `usize`.
    pub fn index(&self, channel: u8, x: u32, y: u32) -> Option<usize> {
        if !self.in_bounds(channel, x, y) {
            return None
        }

        self.index_ignoring_bounds(channel as usize, x as usize, y as usize)
    }

    /// Get the theoretical position of sample (channel, x, y).
    ///
    /// The 'check' is for overflow during index calculation, not that it is contained in the
    /// image.
    pub fn index_ignoring_bounds(&self, channel: usize, x: usize, y: usize) -> Option<usize> {
        let idx_c = (channel as usize).checked_mul(self.channel_stride);
        let idx_x = (x as usize).checked_mul(self.width_stride);
        let idx_y = (y as usize).checked_mul(self.height_stride);

        let (idx_c, idx_x, idx_y) = match (idx_c, idx_x, idx_y) {
            (Some(idx_c), Some(idx_x), Some(idx_y)) => (idx_c, idx_x, idx_y),
            _ => return None,
        };

        Some(0usize)
            .and_then(|b| b.checked_add(idx_c))
            .and_then(|b| b.checked_add(idx_x))
            .and_then(|b| b.checked_add(idx_y))
    }

    /// Get an index provided it is inbouds.
    ///
    /// Assumes that the image is backed by some sufficiently large buffer. Then computation can
    /// not overflow as we could represent the maximum coordinate. Since overflow is defined either
    /// way, this method can not be unsafe.
    pub fn in_bounds_index(&self, c: u8, x: u32, y: u32) -> usize {
        let (c_stride, x_stride, y_stride) = self.strides_cwh();
        (y as usize * y_stride) + (x as usize * x_stride) + (c as usize * c_stride)
    }


    /// Shrink the image to the minimum of current and given extents.
    ///
    /// This does not modify the strides, so that the resulting sample buffer may have holes
    /// created by the shrinking operation. Shrinking could also lead to an non-aliasing image when
    /// samples had aliased each other before.
    pub fn shrink_to(&mut self, channels: u8, width: u32, height: u32) {
        self.channels = self.channels.min(channels);
        self.width = self.width.min(width);
        self.height = self.height.min(height);
    }
}

impl Dim {
    fn stride(self) -> usize {
        self.0
    }

    /// Length of this dimension in memory.
    fn checked_len(self) -> Option<usize> {
        self.0.checked_mul(self.1)
    }

    fn len(self) -> usize {
        self.0*self.1
    }
}

impl<Buffer> FlatSamples<Buffer> {
    /// Get the strides for indexing matrix-like [(c, w, h)].
    ///
    /// For a row-major layout with grouped samples, this tuple is strictly
    /// increasing.
    pub fn strides_cwh(&self) -> (usize, usize, usize) {
        self.format.strides_cwh()
    }

    /// Get the dimensions (channels, width, height).
    ///
    /// The interface is optimized for use with `strides_cwh` instead. The channel extent will be
    /// before width and height.
    pub fn extents(&self) -> (usize, usize, usize) {
        self.format.extents()
    }

    /// Tuple of bounds in the order of coordinate inputs.
    ///
    /// This function should be used whenever working with image coordinates opposed to buffer
    /// coordinates.
    pub fn bounds(&self) -> (u8, u32, u32) {
        self.format.bounds()
    }

    /// Get a reference based version.
    pub fn as_ref<T>(&self) -> FlatSamples<&[T]> where Buffer: AsRef<[T]> {
        FlatSamples {
            samples: self.samples.as_ref(),
            format: self.format,
            color_hint: self.color_hint,
        }
    }

    /// Get a mutable reference based version.
    pub fn as_mut<T>(&mut self) -> FlatSamples<&mut [T]> where Buffer: AsMut<[T]> {
        FlatSamples {
            samples: self.samples.as_mut(),
            format: self.format,
            color_hint: self.color_hint,
        }
    }

    /// Copy the data into an owned vector.
    pub fn to_vec<T>(&self) -> FlatSamples<Vec<T>> 
        where T: Clone, Buffer: AsRef<[T]> 
    {
        FlatSamples {
            samples: self.samples.as_ref().to_vec(),
            format: self.format,
            color_hint: self.color_hint,
        }
    }

    /// View this buffer as an image over some type of pixel.
    ///
    /// This first ensures that all in-bounds coordinates refer to valid indices in the sample
    /// buffer. It also checks that the specified pixel format expects the same number of channels
    /// that are present in this buffer. Neither are larger nor a smaller number will be accepted.
    /// There is no automatic conversion.
    pub fn as_view<P>(&self) -> Result<View<&[P::Subpixel], P>, Error> 
        where P: Pixel, Buffer: AsRef<[P::Subpixel]>,
    {
        if self.format.channels != P::channel_count() {
            return Err(Error::WrongColor(P::color_type()))
        }

        let as_ref = self.samples.as_ref();
        if !self.format.fits(as_ref.len()) {
            return Err(Error::TooLarge)
        }

        Ok(View {
            inner: FlatSamples {
                samples: as_ref,
                format: self.format,
                color_hint: self.color_hint,
            },
            phantom: PhantomData,
        })
    }

    /// Interpret this buffer as a mutable image.
    ///
    /// To succeed, the pixels in this buffer may not alias each other and the samples of each
    /// pixel must be packed (i.e. `channel_stride` is `1`). The number of channels must be
    /// consistent with the channel count expected by the pixel format.
    ///
    /// This is similar to an `ImageBuffer` except it is a temporary view that is not normalized as
    /// strongly. To get an owning version, consider copying the data into an `ImageBuffer`. This
    /// provides many more operations, is possibly faster (if not you may want to open an issue) is
    /// generally polished. You can also try to convert this buffer inline, see
    /// `ImageBuffer::from_raw`.
    pub fn as_view_mut<P>(&mut self) -> Result<ViewMut<&mut [P::Subpixel], P>, Error>
        where P: Pixel, Buffer: AsMut<[P::Subpixel]>,
    {
        if !self.format.is_normal(NormalForm::PixelPacked) {
            return Err(Error::NormalFormRequired(NormalForm::PixelPacked))
        }

        if self.format.channels != P::channel_count() {
            return Err(Error::WrongColor(P::color_type()))
        }

        let as_mut = self.samples.as_mut();
        if !self.format.fits(as_mut.len()) {
            return Err(Error::TooLarge)
        }

        Ok(ViewMut {
            inner: FlatSamples {
                samples: as_mut,
                format: self.format,
                color_hint: self.color_hint,
            },
            phantom: PhantomData,
        })
    }

    /// View the samples as a slice.
    ///
    /// The slice is not limited to the region of the image and not all sample indices are valid
    /// indices into this buffer. See `image_mut_slice` as an alternative.
    pub fn as_slice<T>(&self) -> &[T] where Buffer: AsRef<[T]> {
        self.samples.as_ref()
    }

    /// View the samples as a slice.
    ///
    /// The slice is not limited to the region of the image and not all sample indices are valid
    /// indices into this buffer. See `image_mut_slice` as an alternative.
    pub fn as_mut_slice<T>(&mut self) -> &mut [T] where Buffer: AsMut<[T]> {
        self.samples.as_mut()
    }

    /// Return the portion of the buffer that holds sample values.
    ///
    /// This may fail when the coordinates in this image are either out-of-bounds of the underlying
    /// buffer or can not be represented. Note that the slice may have holes that do not correspond
    /// to any sample in the image represented by it.
    pub fn image_slice<T>(&self) -> Option<&[T]> where Buffer: AsRef<[T]> {
        let min_length = match self.min_length() {
            None => return None,
            Some(index) => index,
        };

        let slice = self.samples.as_ref();
        if slice.len() < min_length {
            return None
        }

        Some(&slice[..min_length])
    }

    /// Mutable portion of the buffer that holds sample values.
    pub fn image_mut_slice<T>(&mut self) -> Option<&mut [T]> where Buffer: AsMut<[T]> {
        let min_length = match self.min_length() {
            None => return None,
            Some(index) => index,
        };

        let slice = self.samples.as_mut();
        if slice.len() < min_length {
            return None
        }

        Some(&mut slice[..min_length])
    }

    /// Move the data into an image buffer.
    ///
    /// This does **not** convert the image format. The buffer needs to be in packed row-major form
    /// before calling this function. In case of an error, returns the buffer again so that it does
    /// not release any allocation.
    pub fn try_into_buffer<P>(self) -> Result<ImageBuffer<P, Buffer>, (Error, Self)> 
    where 
        P: Pixel + 'static,
        P::Subpixel: 'static,
        Buffer: Deref<Target=[P::Subpixel]>,
    {
        if !self.is_normal(NormalForm::RowMajorPacked) {
            return Err((Error::NormalFormRequired(NormalForm::RowMajorPacked), self))
        }

        if self.format.channels != P::channel_count() {
            return Err((Error::WrongColor(P::color_type()), self))
        }

        if !self.fits(self.samples.deref().len()) {
            return Err((Error::TooLarge, self))
        }


        Ok(ImageBuffer::from_raw(self.format.width, self.format.height, self.samples).unwrap_or_else(
            || panic!("Preconditions should have been ensured before conversion")))
    }

    /// Get the minimum length of a buffer such that all in-bounds samples have valid indices.
    /// 
    /// This method will allow zero strides, allowing compact representations of monochrome images.
    /// To check that no aliasing occurs, try `check_alias_invariants`. For compact images (no
    /// aliasing and no unindexed samples) this is `width*height*channels`. But for both of the
    /// other cases, the reasoning is slightly more involved.
    ///
    /// # Explanation
    ///
    /// Note that there is a difference between `min_length` and the index of the sample
    /// 'one-past-the-end`. This is due to strides that may be larger than the dimension below.
    ///
    /// ## Example with holes
    ///
    /// Let's look at an example of a grayscale image with 
    /// * `width_stride = 1`
    /// * `width = 2`
    /// * `height_stride = 3`
    /// * `height = 2`
    ///
    /// ```text
    /// | x x   | x x m | $
    ///  min_length m ^
    ///                   ^ one-past-the-end $
    /// ```
    ///
    /// The difference is also extreme for empty images with large strides. The one-past-the-end
    /// sample index is still as large as the largest of these strides while `min_length = 0`.
    ///
    /// ## Example with aliasing
    ///
    /// The concept gets even more important when you allow samples to alias each other. Here we
    /// have the buffer of a small grayscale image where this is the case, this time we will first
    /// show the buffer and then the individual rows below.
    ///
    /// * `width_stride = 1`
    /// * `width = 3`
    /// * `height_stride = 2`
    /// * `height = 2`
    ///
    /// ```text
    ///  1 2 3 4 5 m
    /// |1 2 3| row one
    ///     |3 4 5| row two
    ///            ^ m min_length
    ///          ^ ??? one-past-the-end
    /// ```
    ///
    /// This time 'one-past-the-end' is not even simply the largest stride times the extent of its
    /// dimension. That still points inside the image because `height*height_stride = 4` but also
    /// `index_of(1, 2) = 4`.
    pub fn min_length(&self) -> Option<usize> {
        self.format.min_length()
    }

    /// Check if the buffer is large enough.
    pub fn fits(&self, len: usize) -> bool {
        self.format.fits(len)
    }

    /// If there are any samples aliasing each other.
    ///
    /// If this is not the case, it would always be safe to allow mutable access to two different
    /// samples at the same time. Otherwise, this operation would need additional checks. When one
    /// dimension overflows `usize` with its stride we also consider this aliasing.
    pub fn has_aliased_samples(&self) -> bool {
        self.format.has_aliased_samples()
    }

    /// Check if a buffer fulfills the requirements of a normal form.
    ///
    /// Certain conversions have preconditions on the structure of the sample buffer that are not
    /// captured (by design) by the type system. These are then checked before the conversion. Such
    /// checks can all be done in constant time and will not inspect the buffer content. You can
    /// perform these checks yourself when the conversion is not required at this moment but maybe
    /// still performed later.
    pub fn is_normal(&self, form: NormalForm) -> bool {
        self.format.is_normal(form)
    }

    /// Check that the pixel and the channel index are in bounds.
    pub fn in_bounds(&self, channel: u8, x: u32, y: u32) -> bool {
        self.format.in_bounds(channel, x, y)
    }

    /// Resolve the index of a particular sample.
    ///
    /// `None` if the index is outside the bounds or does not fit into a `usize`.
    pub fn index(&self, channel: u8, x: u32, y: u32) -> Option<usize> {
        self.format.index(channel, x, y)
    }

    /// Get the theoretical position of sample (x, y, channel).
    ///
    /// The 'check' is for overflow during index calculation, not that it is contained in the
    /// image.
    pub fn index_ignoring_bounds(&self, channel: usize, x: usize, y: usize) -> Option<usize> {
        self.format.index_ignoring_bounds(channel, x, y)
    }

    /// Get an index provided it is inbouds.
    ///
    /// Assumes that the image is backed by some sufficiently large buffer. Then computation can
    /// not overflow as we could represent the maximum coordinate. Since overflow is defined either
    /// way, this method can not be unsafe.
    pub fn in_bounds_index(&self, channel: u8, x: u32, y: u32) -> usize {
        self.format.in_bounds_index(channel, x, y)
    }

    /// Shrink the image to the minimum of current and given extents.
    ///
    /// This does not modify the strides, so that the resulting sample buffer may have holes
    /// created by the shrinking operation. Shrinking could also lead to an non-aliasing image when
    /// samples had aliased each other before.
    pub fn shrink_to(&mut self, channels: u8, width: u32, height: u32) {
        self.format.shrink_to(channels, width, height)
    }
}

/// A flat buffer that can be used as an image view.
///
/// This is a nearly trivial wrapper around a buffer but at least sanitizes by checking the buffer
/// length first and constraining the pixel type.
///
/// Note that this does not eliminate panics as the `AsRef<[T]` implementation of `Buffer` may be
/// unreliable, i.e. return different buffers at different times. This of course is a non-issue for
/// all common collections where the bounds check once must be enough.
#[derive(Clone, Debug)]
pub struct View<Buffer, P: Pixel> 
where 
    Buffer: AsRef<[P::Subpixel]> 
{
    inner: FlatSamples<Buffer>,
    phantom: PhantomData<P>,
}

/// A mutable owning version of a flat buffer.
///
/// While this wraps a buffer similar to `ImageBuffer`, this is mostly intended as a utility. The
/// library endorsed normalized representation is still `ImageBuffer`. Also, the implementation of
/// `AsMut<[P::Subpixel]>` must always yield the same buffer. Therefore there is no public way to
/// construct this with an owning buffer.
#[derive(Clone, Debug)]
pub struct ViewMut<Buffer, P: Pixel> 
where 
    Buffer: AsMut<[P::Subpixel]> 
{
    inner: FlatSamples<Buffer>,
    phantom: PhantomData<P>,
}

/// Denotes invalid flat sample buffers when trying to convert to stricter types.
///
/// The biggest use case being `ImageBuffer` which expects closely packed
/// samples in a row major matrix representation. But this error type may be
/// resused for other import functions. A more versatile user may also try to
/// correct the underlying representation depending on the error variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Error {
    /// The represented image was too large.
    ///
    /// The optional value denotes a possibly accepted maximal bound.
    TooLarge,

    /// The represented image can not use this representation.
    ///
    /// The normalized form that would be accepted.
    NormalFormRequired(NormalForm),

    /// The color format did not match the channel count.
    ///
    /// In some cases you might be able to fix this by lowering the reported pixel count of the
    /// buffer without touching the strides.
    ///
    /// In very special circumstances you *may* do the opposite. This is **VERY** dangerous but not
    /// directly memory unsafe although that will likely alias pixels. One scenario is when you
    /// want to construct an `Rgba` image but have only 3 bytes per pixel and for some reason don't
    /// care about the value of the alpha channel even though you need `Rgba`.
    WrongColor(ColorType),
}

/// Different normal forms of buffers.
///
/// A normal form is an unaliased buffer with some additional constraints.  The `ÌmageBuffer` uses
/// row major form with packed samples. 
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NormalForm {
    /// No pixel aliases another.
    ///
    /// Unaliased also guarantees that all index calculations in the image bounds using
    /// `dim_index*dim_stride` (such as `x*width_stride + y*height_stride`) do not overflow.
    Unaliased,

    /// At least pixels are packed.
    ///
    /// Images of these types can wrap `[T]`-slices into the standard color types. This is a
    /// precondition for `GenericImage` which requires by-reference access to pixels.
    PixelPacked,

    /// All samples are packed.
    ///
    /// This is orthogonal to `PixelPacked`. It requires that there are no holes in the image but
    /// it is not necessary that the pixel samples themselves are adjacent. An example of this
    /// behaviour is a planar image format.
    ImagePacked,

    /// The samples are in row-major form and all samples are packed.
    ///
    /// In addition to `PixelPacked` and `ImagePacked` this also asserts that the pixel matrix is
    /// in row-major form. 
    RowMajorPacked,

    /// The samples are in column-major form and all samples are packed.
    ///
    /// In addition to `PixelPacked` and `ImagePacked` this also asserts that the pixel matrix is
    /// in column-major form. 
    ColumnMajorPacked,
}

impl<Buffer, P: Pixel> View<Buffer, P>
where 
    Buffer: AsRef<[P::Subpixel]> 
{
    /// Take out the sample buffer.
    ///
    /// Gives up the normalization invariants on the buffer format.
    pub fn into_inner(self) -> FlatSamples<Buffer> {
        self.inner
    }

    /// Get a reference on the inner sample descriptor.
    ///
    /// There is no mutable counterpart as modifying the buffer format, including strides and
    /// lengths, could invalidate the accessibility invariants of the `View`. It is not specified
    /// if the inner buffer is the same as the buffer of the image from which this view was
    /// created. It might have been truncated as an optimization.
    pub fn flat(&self) -> &FlatSamples<Buffer> {
        &self.inner
    }

    /// Get a reference on the inner buffer.
    ///
    /// There is no mutable counter part since it is not intended to allow you to reassign the
    /// buffer or otherwise change its size or properties.
    pub fn samples(&self) -> &Buffer {
        &self.inner.samples
    }

    /// Get a reference to a selected subpixel if it is in-bounds.
    ///
    /// This method will return `None` when the sample is out-of-bounds. All errors that could
    /// occur due to overflow have been eliminated while construction the `View`.
    pub fn get_sample(&self, channel: u8, x: u32, y: u32) -> Option<&P::Subpixel> {
        if !self.inner.in_bounds(channel, x, y) {
            return None
        }

        let index = self.inner.in_bounds_index(channel, x, y);
        // Should always be `Some(_)` but checking is more costly.
        self.samples().as_ref().get(index)
    }

    /// Get the minimum length of a buffer such that all in-bounds samples have valid indices.
    ///
    /// See `FlatSamples::min_length`. This method will always succeed.
    pub fn min_length(&self) -> usize {
        self.inner.min_length().unwrap()
    }

    /// Return the portion of the buffer that holds sample values.
    ///
    /// While this can not fail–the validity of all coordinates has been validated during the
    /// conversion from `FlatSamples`–the resulting slice may still contain holes.
    pub fn image_slice(&self) -> &[P::Subpixel] {
        &self.samples().as_ref()[..self.min_length()]
    }

    /// Shrink the inner image.
    ///
    /// The new dimensions will be the minimum of the previous dimensions. Since the set of
    /// in-bounds pixels afterwards is a subset of the current ones, this is allowed on a `View`.
    pub fn shrink_to(&mut self, channels: u8, width: u32, height: u32) {
        self.inner.shrink_to(channels, width, height)
    }
}

impl<Buffer, P: Pixel> ViewMut<Buffer, P>
where 
    Buffer: AsMut<[P::Subpixel]>
{
    /// Take out the sample buffer.
    ///
    /// Gives up the normalization invariants on the buffer format.
    pub fn into_inner(self) -> FlatSamples<Buffer> {
        self.inner
    }

    /// Get a reference on the sample buffer descriptor.
    ///
    /// There is no mutable counterpart as modifying the buffer format, including strides and
    /// lengths, could invalidate the accessibility invariants of the `View`. It is not specified
    /// if the inner buffer is the same as the buffer of the image from which this view was
    /// created. It might have been truncated as an optimization.
    pub fn flat(&self) -> &FlatSamples<Buffer> {
        &self.inner
    }

    /// Get a reference on the inner buffer.
    ///
    /// There is no mutable counter part since it is not intended to allow you to reassign the
    /// buffer or otherwise change its size or properties. However, its contents can be accessed
    /// mutable through a slice with `image_mut_slice`.
    pub fn samples(&self) -> &Buffer {
        &self.inner.samples
    }

    /// Get the minimum length of a buffer such that all in-bounds samples have valid indices.
    ///
    /// See `FlatSamples::min_length`. This method will always succeed.
    pub fn min_length(&self) -> usize {
        self.inner.min_length().unwrap()
    }

    /// Get a reference to a selected subpixel.
    ///
    /// This method will return `None` when the sample is out-of-bounds. All errors that could
    /// occur due to overflow have been eliminated while construction the `View`.
    pub fn get_sample(&self, channel: u8, x: u32, y: u32) -> Option<&P::Subpixel>
        where Buffer: AsRef<[P::Subpixel]>
    {
        if !self.inner.in_bounds(channel, x, y) {
            return None
        }

        let index = self.inner.in_bounds_index(channel, x, y);
        // Should always be `Some(_)` but checking is more costly.
        self.samples().as_ref().get(index)
    }

    /// Get a mutable reference to a selected sample.
    ///
    /// This method will return `None` when the sample is out-of-bounds. All errors that could
    /// occur due to overflow have been eliminated while construction the `View`.
    pub fn get_mut_sample(&mut self, channel: u8, x: u32, y: u32) -> Option<&mut P::Subpixel> {
        if !self.inner.in_bounds(channel, x, y) {
            return None
        }

        let index = self.inner.in_bounds_index(channel, x, y);
        // Should always be `Some(_)` but checking is more costly.
        self.inner.samples.as_mut().get_mut(index)
    }

    /// Return the portion of the buffer that holds sample values.
    ///
    /// While this can not fail–the validity of all coordinates has been validated during the
    /// conversion from `FlatSamples`–the resulting slice may still contain holes.
    pub fn image_slice(&self) -> &[P::Subpixel] where Buffer: AsRef<[P::Subpixel]> {
        &self.inner.samples.as_ref()[..self.min_length()]
    }

    /// Return the mutable buffer that holds sample values.
    pub fn image_mut_slice(&mut self) -> &mut [P::Subpixel] {
        let length = self.min_length();
        &mut self.inner.samples.as_mut()[..length]
    }

    /// Shrink the inner image.
    ///
    /// The new dimensions will be the minimum of the previous dimensions. Since the set of
    /// in-bounds pixels afterwards is a subset of the current ones, this is allowed on a `View`.
    pub fn shrink_to(&mut self, channels: u8, width: u32, height: u32) {
        self.inner.shrink_to(channels, width, height)
    }
}


// The out-of-bounds panic for single sample access similar to `slice::index`.
#[inline(never)]
#[cold]
fn panic_cwh_out_of_bounds(
    (c, x, y): (u8, u32, u32),
    bounds: (u8, u32, u32),
    strides: (usize, usize, usize)) -> !
{
    panic!("Sample coordinates {:?} out of sample matrix bounds {:?} with strides {:?}", (c, x, y), bounds, strides)
}

// The out-of-bounds panic for pixel access similar to `slice::index`.
#[inline(never)]
#[cold]
fn panic_pixel_out_of_bounds(
    (x, y): (u32, u32),
    bounds: (u32, u32)) -> !
{
    panic!("Image index {:?} out of bounds {:?}", (x, y), bounds)
}

impl<Buffer> Index<(u8, u32, u32)> for FlatSamples<Buffer>
    where Buffer: Index<usize>
{
    type Output = Buffer::Output;

    /// Return a reference to a single sample at specified coordinates.
    ///
    /// # Panics
    ///
    /// When the coordinates are out of bounds or the index calculation fails.
    fn index(&self, (c, x, y): (u8, u32, u32)) -> &Self::Output {
        let bounds = self.bounds();
        let strides = self.strides_cwh();
        let index = self.index(c, x, y).unwrap_or_else(||
            panic_cwh_out_of_bounds((c, x, y), bounds, strides));
        &self.samples[index]
    }
}

impl<Buffer> IndexMut<(u8, u32, u32)> for FlatSamples<Buffer>
    where Buffer: IndexMut<usize>
{

    /// Return a mutable reference to a single sample at specified coordinates.
    ///
    /// # Panics
    ///
    /// When the coordinates are out of bounds or the index calculation fails.
    fn index_mut(&mut self, (c, x, y): (u8, u32, u32)) -> &mut Self::Output {
        let bounds = self.bounds();
        let strides = self.strides_cwh();
        let index = self.index(c, x, y).unwrap_or_else(||
            panic_cwh_out_of_bounds((c, x, y), bounds, strides));
        &mut self.samples[index]
    }
}

impl<Buffer, P: Pixel> GenericImageView for View<Buffer, P> 
    where Buffer: AsRef<[P::Subpixel]>
{
    type Pixel = P;

    // We don't proxy an inner image.
    type InnerImageView = Self;

    fn dimensions(&self) -> (u32, u32) {
        (self.inner.format.width, self.inner.format.height)
    }

    fn bounds(&self) -> (u32, u32, u32, u32) {
        let (w, h) = self.dimensions();
        (0, w, 0, h)
    }

    fn in_bounds(&self, x: u32, y: u32) -> bool {
        let (w, h) = self.dimensions();
        x < w && y < h
    }

    fn get_pixel(&self, x: u32, y: u32) -> Self::Pixel {
        if !self.inner.in_bounds(0, x, y) {
            panic_pixel_out_of_bounds((x, y), self.dimensions())
        }

        let image = self.inner.samples.as_ref();
        let base_index = self.inner.in_bounds_index(0, x, y);
        let channels = P::channel_count() as usize;

        let mut buffer = [Zero::zero(); 256];
        buffer.iter_mut().enumerate().take(channels).for_each(|(c, to)| {
            let index = base_index + c*self.inner.format.channel_stride;
            *to = image[index];
        });

        P::from_slice(&buffer[..channels]).clone()
    }

    fn inner(&self) -> &Self {
        self // There is no other inner image.
    }
}

impl<Buffer, P: Pixel> GenericImageView for ViewMut<Buffer, P> 
    where Buffer: AsMut<[P::Subpixel]> + AsRef<[P::Subpixel]>,
{
    type Pixel = P;

    // We don't proxy an inner image.
    type InnerImageView = Self;

    fn dimensions(&self) -> (u32, u32) {
        (self.inner.format.width, self.inner.format.height)
    }

    fn bounds(&self) -> (u32, u32, u32, u32) {
        let (w, h) = self.dimensions();
        (0, w, 0, h)
    }

    fn in_bounds(&self, x: u32, y: u32) -> bool {
        let (w, h) = self.dimensions();
        x < w && y < h
    }

    fn get_pixel(&self, x: u32, y: u32) -> Self::Pixel {
        if !self.inner.in_bounds(0, x, y) {
            panic_pixel_out_of_bounds((x, y), self.dimensions())
        }

        let image = self.inner.samples.as_ref();
        let base_index = self.inner.in_bounds_index(0, x, y);
        let channels = P::channel_count() as usize;

        let mut buffer = [Zero::zero(); 256];
        buffer.iter_mut().enumerate().take(channels).for_each(|(c, to)| {
            let index = base_index + c*self.inner.format.channel_stride;
            *to = image[index];
        });

        P::from_slice(&buffer[..channels]).clone()
    }

    fn inner(&self) -> &Self {
        self // There is no other inner image.
    }
}

impl<Buffer, P: Pixel> GenericImage for ViewMut<Buffer, P> 
    where Buffer: AsMut<[P::Subpixel]> + AsRef<[P::Subpixel]>,
{
    type InnerImage = Self;

    fn get_pixel_mut(&mut self, x: u32, y: u32) -> &mut Self::Pixel {
        if !self.inner.in_bounds(0, x, y) {
            panic_pixel_out_of_bounds((x, y), self.dimensions())
        }

        let base_index = self.inner.in_bounds_index(0, x, y);
        let channel_count = <P as Pixel>::channel_count() as usize;
        let pixel_range = base_index..base_index + channel_count;
        P::from_slice_mut(&mut self.inner.samples.as_mut()[pixel_range])
    }

    fn put_pixel(&mut self, x: u32, y: u32, pixel: Self::Pixel) {
        *self.get_pixel_mut(x, y) = pixel;
    }

    fn blend_pixel(&mut self, x: u32, y: u32, pixel: Self::Pixel) {
        self.get_pixel_mut(x, y).blend(&pixel);
    }

    fn inner_mut(&mut self) -> &mut Self {
        self
    }
}

impl From<Error> for ImageError {
    fn from(error: Error) -> ImageError {
        match error {
            Error::TooLarge => ImageError::DimensionError,
            Error::WrongColor(color) => ImageError::UnsupportedColor(color),
            Error::NormalFormRequired(form) => ImageError::FormatError(
                format!("Required sample buffer in normal form {:?}", form)),
        }
    }
}

impl PartialOrd for NormalForm {
    /// Compares the logical preconditions.
    ///
    /// `a < b` if the normal form `a` has less preconditions than `b`.
    fn partial_cmp(&self, other: &Self) -> Option<cmp::Ordering> {
        match (*self, *other) {
            (NormalForm::Unaliased, NormalForm::Unaliased) => Some(cmp::Ordering::Equal),
            (NormalForm::PixelPacked, NormalForm::PixelPacked) => Some(cmp::Ordering::Equal),
            (NormalForm::ImagePacked, NormalForm::ImagePacked) => Some(cmp::Ordering::Equal),
            (NormalForm::RowMajorPacked, NormalForm::RowMajorPacked) => Some(cmp::Ordering::Equal),
            (NormalForm::ColumnMajorPacked, NormalForm::ColumnMajorPacked) => Some(cmp::Ordering::Equal),

            (NormalForm::Unaliased, _) => Some(cmp::Ordering::Less),
            (_, NormalForm::Unaliased) => Some(cmp::Ordering::Greater),

            (NormalForm::PixelPacked, NormalForm::ColumnMajorPacked) => Some(cmp::Ordering::Less),
            (NormalForm::PixelPacked, NormalForm::RowMajorPacked) => Some(cmp::Ordering::Less),
            (NormalForm::RowMajorPacked, NormalForm::PixelPacked) => Some(cmp::Ordering::Greater),
            (NormalForm::ColumnMajorPacked, NormalForm::PixelPacked) => Some(cmp::Ordering::Greater),

            (NormalForm::ImagePacked, NormalForm::ColumnMajorPacked) => Some(cmp::Ordering::Less),
            (NormalForm::ImagePacked, NormalForm::RowMajorPacked) => Some(cmp::Ordering::Less),
            (NormalForm::RowMajorPacked, NormalForm::ImagePacked) => Some(cmp::Ordering::Greater),
            (NormalForm::ColumnMajorPacked, NormalForm::ImagePacked) => Some(cmp::Ordering::Greater),

            (NormalForm::ImagePacked, NormalForm::PixelPacked) => None,
            (NormalForm::PixelPacked, NormalForm::ImagePacked) => None,
            (NormalForm::RowMajorPacked, NormalForm::ColumnMajorPacked) => None,
            (NormalForm::ColumnMajorPacked, NormalForm::RowMajorPacked) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use buffer::GrayAlphaImage;
    use color::{LumaA, Rgb};

    #[test]
    fn aliasing_view() {
       let buffer = FlatSamples {
           samples: &[42],
           format: SampleLayout {
               channels: 3,
               channel_stride: 0,
               width: 100,
               width_stride: 0,
               height: 100,
               height_stride: 0,
           },
           color_hint: None,
       };

       let view = buffer.as_view::<Rgb<usize>>()
           .expect("This is a valid view");
       let pixel_count = view.pixels()
           .inspect(|pixel| assert!(pixel.2 == Rgb([42, 42, 42])))
           .count();
       assert_eq!(pixel_count, 100*100);
    }

    #[test]
    fn mutable_view() {
        let mut buffer = FlatSamples {
            samples: [0; 18],
            format: SampleLayout {
                channels: 2,
                channel_stride: 1,
                width: 3,
                width_stride: 2,
                height: 3,
                height_stride: 6,
            },
            color_hint: None,
        };

        {
            let mut view = buffer.as_view_mut::<LumaA<usize>>()
                .expect("This should be a valid mutable buffer");
            #[allow(deprecated)]
            let pixel_count = view.pixels_mut()
                .enumerate()
                .map(|(idx, (_, _, pixel))| *pixel = LumaA([2*idx, 2*idx + 1]))
                .count();
            assert_eq!(pixel_count, 9);
        }

        buffer.samples.iter()
            .enumerate()
            .for_each(|(idx, sample)| assert_eq!(idx, *sample));
    }

    #[test]
    fn normal_forms() {
        assert!(FlatSamples {
            samples: [0u8; 0],
            format: SampleLayout {
                channels: 2,
                channel_stride: 1,
                width: 3,
                width_stride: 9,
                height: 3,
                height_stride: 28,
            },
            color_hint: None,
        }.is_normal(NormalForm::PixelPacked));

        assert!(FlatSamples {
            samples: [0u8; 0],
            format: SampleLayout {
                channels: 2,
                channel_stride: 8,
                width: 4,
                width_stride: 1,
                height: 2,
                height_stride: 4,
            },
            color_hint: None,
        }.is_normal(NormalForm::ImagePacked));

        assert!(FlatSamples {
            samples: [0u8; 0],
            format: SampleLayout {
                channels: 2,
                channel_stride: 1,
                width: 4,
                width_stride: 2,
                height: 2,
                height_stride: 8,
            },
            color_hint: None,
        }.is_normal(NormalForm::RowMajorPacked));

        assert!(FlatSamples {
            samples: [0u8; 0],
            format: SampleLayout {
                channels: 2,
                channel_stride: 1,
                width: 4,
                width_stride: 4,
                height: 2,
                height_stride: 2,
            },
            color_hint: None,
        }.is_normal(NormalForm::ColumnMajorPacked));
    }

    #[test]
    fn image_buffer_conversion() {
        let buffer = FlatSamples {
            samples: vec![0u8; 16],
            format: SampleLayout {
                channels: 2,
                channel_stride: 1,
                width: 4,
                width_stride: 2,
                height: 2,
                height_stride: 8,
            },
            color_hint: None,
        };

        let _: GrayAlphaImage = buffer.try_into_buffer().unwrap_or_else(|(error, _)|
            panic!("Expected buffer to be convertible but {:?}", error));
    }
}
