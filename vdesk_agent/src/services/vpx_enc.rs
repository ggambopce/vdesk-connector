//! VP9 인코더 — C 래퍼(vpx_wrap.c) 호출

use anyhow::{anyhow, Result};

// ── C 래퍼 FFI ───────────────────────────────────────────────────────────────

#[repr(C)]
struct VpxEncHandle(u8); // opaque

extern "C" {
    fn vpx_enc_create(w: i32, h: i32, bitrate_kbps: i32, fps: i32) -> *mut VpxEncHandle;
    fn vpx_enc_create_ex(w: i32, h: i32, bitrate_kbps: i32, fps: i32, out_err: *mut i32) -> *mut VpxEncHandle;
    fn vpx_enc_destroy(h: *mut VpxEncHandle);
    fn vpx_enc_encode(
        h:         *mut VpxEncHandle,
        i420:      *const u8,
        force_key: i32,
        out_buf:   *mut *const u8,
        out_len:   *mut i32,
        is_key:    *mut i32,
    ) -> i32;
}

// ── VpxEncoder ───────────────────────────────────────────────────────────────

pub struct VpxEncoder {
    handle: *mut VpxEncHandle,
}

// handle은 힙 할당이므로 Send 안전
unsafe impl Send for VpxEncoder {}

impl Drop for VpxEncoder {
    fn drop(&mut self) {
        unsafe { vpx_enc_destroy(self.handle); }
    }
}

impl VpxEncoder {
    /// VP9 인코더 초기화
    /// - `bitrate_kbps`: 목표 비트레이트 (kbps). 1080p 권장: 2000~4000
    /// - `fps`: 목표 FPS
    pub fn new(w: u32, h: u32, bitrate_kbps: u32, fps: u32) -> Result<Self> {
        let mut err_code: i32 = 0;
        let handle = unsafe {
            vpx_enc_create_ex(w as i32, h as i32, bitrate_kbps as i32, fps as i32, &mut err_code)
        };
        if handle.is_null() {
            let reason = match err_code {
                1 => "calloc 실패 (OOM)",
                2 => "vpx_codec_enc_config_default 실패 (코덱 미지원?)",
                3 => "vpx_codec_enc_init 실패 (SIMD/CPU 기능 부족 또는 설정 오류)",
                4 => "vpx_img_alloc 실패 (메모리 부족)",
                _ => "알 수 없는 오류",
            };
            return Err(anyhow!("vpx_enc_create 실패 [err={}]: {}", err_code, reason));
        }
        Ok(Self { handle })
    }

    /// I420 프레임 인코딩
    ///
    /// # 반환
    /// `Some((data, is_keyframe))` 또는 `None` (이번 프레임 출력 없음)
    pub fn encode(&mut self, i420: &[u8], force_key: bool) -> Result<Option<(&[u8], bool)>> {
        let mut out_buf: *const u8 = std::ptr::null();
        let mut out_len: i32 = 0;
        let mut is_key: i32 = 0;

        let ret = unsafe {
            vpx_enc_encode(
                self.handle,
                i420.as_ptr(),
                force_key as i32,
                &mut out_buf,
                &mut out_len,
                &mut is_key,
            )
        };

        if ret != 0 {
            return Err(anyhow!("vpx_enc_encode 오류: {}", ret));
        }

        if out_buf.is_null() || out_len == 0 {
            return Ok(None);
        }

        let data = unsafe { std::slice::from_raw_parts(out_buf, out_len as usize) };
        Ok(Some((data, is_key != 0)))
    }
}
