use image::RgbaImage;
use objc2_core_foundation::CGRect;
use objc2_core_graphics::{
    CGDataProvider, CGImage, CGWindowID, CGWindowImageOption, CGWindowListCreateImage,
    CGWindowListOption,
};

use crate::error::{XCapError, XCapResult};

pub fn capture(
    cg_rect: CGRect,
    list_option: CGWindowListOption,
    window_id: CGWindowID,
    image_option: CGWindowImageOption,
) -> XCapResult<RgbaImage> {
    let cg_image = CGWindowListCreateImage(cg_rect, list_option, window_id, image_option);

    let width = CGImage::width(cg_image.as_deref());
    let height = CGImage::height(cg_image.as_deref());
    let bytes_per_row = CGImage::bytes_per_row(cg_image.as_deref());
    let data_provider = CGImage::data_provider(cg_image.as_deref());

    let data = CGDataProvider::data(data_provider.as_deref())
        .ok_or_else(|| XCapError::new("Failed to copy data"))?;

    let len = width * height * 4;
    let mut buffer: Vec<u8> = Vec::with_capacity(len);
    // SAFETY: `buffer` has capacity for `len` bytes, and every byte is
    // initialized by the conversion loop below before `set_len` is called.
    let dst = unsafe { std::slice::from_raw_parts_mut(buffer.as_mut_ptr(), len) };

    // SAFETY: the returned CFData is immutable for the duration of this read,
    // and all indexing stays within its length.
    let src = unsafe { data.as_bytes_unchecked() };

    // Some platforms e.g. MacOS can have extra bytes at the end of each row.
    // See
    // https://github.com/nashaofu/xcap/issues/29
    // https://github.com/nashaofu/xcap/issues/38
    //
    // Strip row padding and convert BGRA to RGBA in a single pass.
    if bytes_per_row == width * 4 {
        bgra_to_rgba(&src[..len], dst);
    } else {
        for (src_row, dst_row) in src
            .chunks_exact(bytes_per_row)
            .zip(dst.chunks_exact_mut(width * 4))
        {
            bgra_to_rgba(&src_row[..width * 4], dst_row);
        }
    }

    // SAFETY: all `len` bytes of `buffer` were written above.
    unsafe { buffer.set_len(len) };

    RgbaImage::from_raw(width as u32, height as u32, buffer)
        .ok_or_else(|| XCapError::new("RgbaImage::from_raw failed"))
}

/// Convert BGRA pixels to RGBA. Written with u32 lane ops so LLVM can
/// auto-vectorize it into SIMD byte shuffles.
#[inline]
fn bgra_to_rgba(src: &[u8], dst: &mut [u8]) {
    for (s, d) in src.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
        let px = u32::from_ne_bytes([s[0], s[1], s[2], s[3]]);
        let px = (px & 0xff00_ff00) | ((px & 0x00ff_0000) >> 16) | ((px & 0x0000_00ff) << 16);
        d.copy_from_slice(&px.to_ne_bytes());
    }
}
