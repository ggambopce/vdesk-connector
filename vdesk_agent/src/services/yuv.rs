//! BGRA → I420 (YUV420p) 변환 — 순수 Rust, libyuv 불필요
//!
//! BT.601 계수 (정수 근사, 256 고정소수점):
//!   Y  =  66R + 129G +  25B + 128 >> 8 + 16
//!   Cb = -38R -  74G + 112B + 128 >> 8 + 128
//!   Cr = 112R -  94G -  18B + 128 >> 8 + 128
//!
//! I420 레이아웃:
//!   [Y plane: w×h] [U plane: (w/2)×(h/2)] [V plane: (w/2)×(h/2)]

/// BGRA 버퍼(B=0,G=1,R=2,A=3)를 I420 형식으로 변환
///
/// # 파라미터
/// - `bgra`: BGRA 픽셀 버퍼 (크기 = w × h × 4)
/// - `w`, `h`: 화면 크기 (짝수여야 함)
/// - `out`: 출력 버퍼 (크기 ≥ w*h*3/2)
pub fn bgra_to_i420(bgra: &[u8], w: usize, h: usize, out: &mut Vec<u8>) {
    let y_size  = w * h;
    let uv_size = (w / 2) * (h / 2);
    let total   = y_size + uv_size * 2;

    out.resize(total, 0);

    let (y_plane, uv) = out.split_at_mut(y_size);
    let (u_plane, v_plane) = uv.split_at_mut(uv_size);

    let uv_w = w / 2;

    for row in 0..h {
        let is_even_row = (row & 1) == 0;
        for col in 0..w {
            let idx  = (row * w + col) * 4;
            let b = bgra[idx    ] as i32;
            let g = bgra[idx + 1] as i32;
            let r = bgra[idx + 2] as i32;

            // Y 평면 (모든 픽셀)
            let y = (66 * r + 129 * g + 25 * b + 128) >> 8;
            y_plane[row * w + col] = (y + 16).clamp(16, 235) as u8;

            // UV 평면: 2×2 블록 좌상단 픽셀에서만 계산
            if is_even_row && (col & 1) == 0 {
                let u = (-38 * r - 74 * g + 112 * b + 128) >> 8;
                let v = (112 * r - 94 * g - 18 * b + 128) >> 8;
                let uv_idx = (row / 2) * uv_w + col / 2;
                u_plane[uv_idx] = (u + 128).clamp(16, 240) as u8;
                v_plane[uv_idx] = (v + 128).clamp(16, 240) as u8;
            }
        }
    }
}
