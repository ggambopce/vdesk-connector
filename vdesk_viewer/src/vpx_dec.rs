//! VP9 디코더 — C 래퍼(vpx_wrap.c) 호출 + I420→XRGB 변환

use anyhow::{anyhow, Result};

// ── C 래퍼 FFI ───────────────────────────────────────────────────────────────

#[repr(C)]
struct VpxDecHandle(u8); // opaque

extern "C" {
    fn vpx_dec_create() -> *mut VpxDecHandle;
    fn vpx_dec_destroy(h: *mut VpxDecHandle);
    fn vpx_dec_decode(
        h:        *mut VpxDecHandle,
        data:     *const u8,
        len:      i32,
        out_w:    *mut i32,
        out_h:    *mut i32,
        out_y:    *mut *mut u8,
        out_u:    *mut *mut u8,
        out_v:    *mut *mut u8,
        stride_y: *mut i32,
        stride_u: *mut i32,
        stride_v: *mut i32,
    ) -> i32;
}

// ── VpxDecoder ───────────────────────────────────────────────────────────────

pub struct VpxDecoder {
    handle: *mut VpxDecHandle,
}

unsafe impl Send for VpxDecoder {}

impl Drop for VpxDecoder {
    fn drop(&mut self) {
        unsafe { vpx_dec_destroy(self.handle); }
    }
}

impl VpxDecoder {
    pub fn new() -> Result<Self> {
        let handle = unsafe { vpx_dec_create() };
        if handle.is_null() {
            return Err(anyhow!("vpx_dec_create 실패"));
        }
        Ok(Self { handle })
    }

    /// VP9 비트스트림 디코딩 → XRGB 픽셀 버퍼 (softbuffer용 0x00RRGGBB)
    ///
    /// # 반환
    /// `Some((width, height, xrgb_pixels))` 또는 `None` (프레임 없음/지연)
    pub fn decode(&mut self, data: &[u8]) -> Result<Option<(u32, u32, Vec<u32>)>> {
        let mut w: i32 = 0;
        let mut h: i32 = 0;
        let mut y_ptr: *mut u8 = std::ptr::null_mut();
        let mut u_ptr: *mut u8 = std::ptr::null_mut();
        let mut v_ptr: *mut u8 = std::ptr::null_mut();
        let mut stride_y: i32 = 0;
        let mut stride_u: i32 = 0;
        let mut stride_v: i32 = 0;

        let ret = unsafe {
            vpx_dec_decode(
                self.handle,
                data.as_ptr(),
                data.len() as i32,
                &mut w, &mut h,
                &mut y_ptr, &mut u_ptr, &mut v_ptr,
                &mut stride_y, &mut stride_u, &mut stride_v,
            )
        };

        match ret {
            -1 => Err(anyhow!("vpx_dec_decode 오류")),
            1  => Ok(None), // 프레임 없음
            _  => {
                // I420 → XRGB 변환
                let pixels = i420_to_xrgb(
                    w as u32, h as u32,
                    y_ptr, u_ptr, v_ptr,
                    stride_y, stride_u, stride_v,
                );
                Ok(Some((w as u32, h as u32, pixels)))
            }
        }
    }
}

// ── I420 → XRGB 변환 ─────────────────────────────────────────────────────────
//
// BT.601 full range → XRGB (0x00RRGGBB)
//   R = Y + 1.402 * (V - 128)
//   G = Y - 0.344 * (U - 128) - 0.714 * (V - 128)
//   B = Y + 1.772 * (U - 128)
//
// 정수 근사 (256 고정소수점):
//   R = Y + 359*(V-128) >> 8
//   G = Y - 88*(U-128) >> 8 - 183*(V-128) >> 8
//   B = Y + 454*(U-128) >> 8

fn i420_to_xrgb(
    w: u32, h: u32,
    y_ptr: *mut u8, u_ptr: *mut u8, v_ptr: *mut u8,
    stride_y: i32, stride_u: i32, stride_v: i32,
) -> Vec<u32> {
    let w = w as usize;
    let h = h as usize;
    let mut pixels = Vec::with_capacity(w * h);

    let y_data = unsafe { std::slice::from_raw_parts(y_ptr, stride_y as usize * h) };
    let u_data = unsafe { std::slice::from_raw_parts(u_ptr, stride_u as usize * (h / 2 + 1)) };
    let v_data = unsafe { std::slice::from_raw_parts(v_ptr, stride_v as usize * (h / 2 + 1)) };

    for row in 0..h {
        let y_row = &y_data[row * stride_y as usize..];
        let uv_row_idx = (row / 2) * stride_u as usize;
        let u_row = &u_data[uv_row_idx..];
        let v_row = &v_data[(row / 2) * stride_v as usize..];

        for col in 0..w {
            let y = y_row[col] as i32;
            let u = u_row[col / 2] as i32 - 128;
            let v = v_row[col / 2] as i32 - 128;

            let r = (y + (359 * v >> 8)).clamp(0, 255) as u32;
            let g = (y - (88 * u >> 8) - (183 * v >> 8)).clamp(0, 255) as u32;
            let b = (y + (454 * u >> 8)).clamp(0, 255) as u32;

            pixels.push((r << 16) | (g << 8) | b);
        }
    }

    pixels
}
