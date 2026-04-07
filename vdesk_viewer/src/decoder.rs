//! JPEG 프레임 디코더 — image 크레이트 사용

use anyhow::Result;

/// JPEG 바이트 → BGRA 픽셀 버퍼 (softbuffer용 XRGB로 변환)
pub fn decode_jpeg(jpeg_bytes: &[u8]) -> Result<(u32, u32, Vec<u32>)> {
    let img = image::load_from_memory_with_format(jpeg_bytes, image::ImageFormat::Jpeg)?;
    let rgb = img.to_rgb8();
    let (width, height) = rgb.dimensions();

    // RGB → XRGB (0x00RRGGBB) — softbuffer 포맷
    let pixels: Vec<u32> = rgb
        .pixels()
        .map(|p| {
            let r = p[0] as u32;
            let g = p[1] as u32;
            let b = p[2] as u32;
            (r << 16) | (g << 8) | b
        })
        .collect();

    Ok((width, height, pixels))
}
