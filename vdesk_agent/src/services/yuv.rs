//! BGRA → I420 (YUV420p) 변환 — 순수 Rust, libyuv 불필요
//!
//! BT.601 계수 (정수 근사, 256 고정소수점):
//!   Y  =  66R + 129G +  25B + 128 >> 8 + 16
//!   Cb = -38R -  74G + 112B + 128 >> 8 + 128
//!   Cr = 112R -  94G -  18B + 128 >> 8 + 128
//!
//! I420 레이아웃:
//!   [Y plane: w×h] [U plane: (w/2)×(h/2)] [V plane: (w/2)×(h/2)]

use super::capture_dxgi::DirtyRect;

/// BGRA 버퍼(B=0,G=1,R=2,A=3)를 I420 형식으로 전체 변환
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

            let y = (66 * r + 129 * g + 25 * b + 128) >> 8;
            y_plane[row * w + col] = (y + 16).clamp(16, 235) as u8;

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

/// 지정된 DirtyRect 목록의 영역만 I420로 업데이트 (나머지는 기존 값 유지)
///
/// 변경된 영역이 적을 때 전체 변환 대비 CPU 사용을 크게 줄인다.
/// UV는 2×2 블록 단위이므로 rect 경계를 짝수로 확장해 처리한다.
///
/// # 파라미터
/// - `bgra` : 현재 BGRA 버퍼 (dirty 영역만 업데이트된 상태)
/// - `w`, `h`: 전체 화면 크기
/// - `out`  : 기존 I420 버퍼 (in-place 업데이트)
/// - `rects`: 변경된 영역 목록
pub fn bgra_to_i420_rects(bgra: &[u8], w: usize, h: usize, out: &mut Vec<u8>, rects: &[DirtyRect]) {
    let y_size  = w * h;
    let uv_w    = w / 2;
    let uv_size = uv_w * (h / 2);
    let total   = y_size + uv_size * 2;

    // 버퍼 미초기화 시 전체 초기화 (첫 호출 방어)
    if out.len() < total {
        out.resize(total, 0);
    }

    let (y_plane, uv) = out.split_at_mut(y_size);
    let (u_plane, v_plane) = uv.split_at_mut(uv_size);

    for rect in rects {
        let x0 = rect.left   as usize;
        let x1 = (rect.right  as usize).min(w);
        let y0 = rect.top    as usize;
        let y1 = (rect.bottom as usize).min(h);

        // Y 평면: 모든 픽셀 업데이트
        for row in y0..y1 {
            for col in x0..x1 {
                let idx = (row * w + col) * 4;
                let b = bgra[idx    ] as i32;
                let g = bgra[idx + 1] as i32;
                let r = bgra[idx + 2] as i32;
                let y = (66 * r + 129 * g + 25 * b + 128) >> 8;
                y_plane[row * w + col] = (y + 16).clamp(16, 235) as u8;
            }
        }

        // UV 평면: 2×2 블록 경계에 맞게 확장
        let ux0 = x0 & !1;           // 짝수 내림
        let ux1 = (x1 + 1) & !1;    // 짝수 올림
        let uy0 = y0 & !1;
        let uy1 = ((y1 + 1) & !1).min(h);

        for row in (uy0..uy1).step_by(2) {
            for col in (ux0..ux1).step_by(2) {
                let idx = (row * w + col) * 4;
                let b = bgra[idx    ] as i32;
                let g = bgra[idx + 1] as i32;
                let r = bgra[idx + 2] as i32;
                let u = (-38 * r - 74 * g + 112 * b + 128) >> 8;
                let v = (112 * r - 94 * g - 18 * b + 128) >> 8;
                let uv_idx = (row / 2) * uv_w + col / 2;
                u_plane[uv_idx] = (u + 128).clamp(16, 240) as u8;
                v_plane[uv_idx] = (v + 128).clamp(16, 240) as u8;
            }
        }
    }
}
