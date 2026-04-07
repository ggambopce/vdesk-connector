//! JPEG 프레임 디코더 — image 크레이트 사용

use anyhow::Result;

/// JPEG 바이트 → XRGB 픽셀 버퍼 (softbuffer용 0x00RRGGBB)
pub fn decode_jpeg(jpeg_bytes: &[u8]) -> Result<(u32, u32, Vec<u32>)> {
    let img = image::load_from_memory_with_format(jpeg_bytes, image::ImageFormat::Jpeg)?;
    // into_rgb8(): 이미 RGB면 copy 없이 소비 (to_rgb8은 항상 clone)
    let rgb = img.into_rgb8();
    let (width, height) = rgb.dimensions();
    let raw = rgb.as_raw();

    // RGB → XRGB (0x00RRGGBB) — chunks_exact으로 bounds check 제거
    let mut pixels = Vec::with_capacity((width * height) as usize);
    for p in raw.chunks_exact(3) {
        pixels.push(((p[0] as u32) << 16) | ((p[1] as u32) << 8) | (p[2] as u32));
    }

    Ok((width, height, pixels))
}
