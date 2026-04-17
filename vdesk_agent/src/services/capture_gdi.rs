//! GDI BitBlt 기반 화면 캡처 폴백
//!
//! DXGI Desktop Duplication이 실패하는 환경에서 사용:
//!   - GPU 드라이버 상태 불량 (장시간 운용, VM 디스플레이 드라이버 버그)
//!   - RDP/터미널 서버 세션 (Desktop Duplication API 제한)
//!
//! DXGI 대비 차이:
//!   - DirtyRects 없음 → 매 프레임 전체 캡처 (FNV 해시로 변화 없는 프레임 스킵)
//!   - 커서 포함 캡처 (DXGI는 커서 별도 제공)
//!   - CPU 사용량 약간 높음 (~2-5ms/frame)
//!   - 출력: BGRA 포맷 (B=byte0, G=byte1, R=byte2, A=0) — DXGI와 동일 레이아웃

use anyhow::{anyhow, Result};
use std::mem;

use winapi::{
    um::{
        wingdi::{
            BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC,
            DeleteObject, GetDIBits, SelectObject,
            BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, SRCCOPY,
        },
        winuser::{GetDC, GetSystemMetrics, ReleaseDC, SM_CXSCREEN, SM_CYSCREEN},
    },
};

pub struct GdiCapture {
    pub width:  u32,
    pub height: u32,
    buf: Vec<u8>, // BGRA(X) 재사용 버퍼, width * height * 4 bytes
}

impl GdiCapture {
    pub fn new() -> Result<Self> {
        unsafe {
            let dc = GetDC(std::ptr::null_mut());
            if dc.is_null() {
                return Err(anyhow!("GetDC 실패"));
            }
            let w = GetSystemMetrics(SM_CXSCREEN) as u32;
            let h = GetSystemMetrics(SM_CYSCREEN) as u32;
            ReleaseDC(std::ptr::null_mut(), dc);

            if w == 0 || h == 0 {
                return Err(anyhow!("화면 크기 감지 실패: {}x{}", w, h));
            }

            hbb_common::log::info!("[gdi] GDI BitBlt 캡처 초기화: {}x{}", w, h);
            Ok(Self {
                width:  w,
                height: h,
                buf:    vec![0u8; (w * h * 4) as usize],
            })
        }
    }

    /// 현재 화면 전체를 캡처하고 BGRA 픽셀 슬라이스를 반환.
    /// 내부 버퍼를 재사용하므로 호출마다 할당 없음.
    pub fn capture(&mut self) -> Result<&[u8]> {
        unsafe {
            let screen = GetDC(std::ptr::null_mut());
            if screen.is_null() {
                return Err(anyhow!("GetDC 실패"));
            }

            let mem_dc = CreateCompatibleDC(screen);
            let bmp    = CreateCompatibleBitmap(screen, self.width as i32, self.height as i32);
            if bmp.is_null() {
                DeleteDC(mem_dc);
                ReleaseDC(std::ptr::null_mut(), screen);
                return Err(anyhow!("CreateCompatibleBitmap 실패"));
            }
            let old = SelectObject(mem_dc, bmp as *mut _);

            // 화면 → 메모리 DC 복사 (커서 포함)
            BitBlt(
                mem_dc, 0, 0, self.width as i32, self.height as i32,
                screen,  0, 0, SRCCOPY,
            );

            // 메모리 DC → BGRA 버퍼 읽기
            // biHeight 음수 = top-down (DXGI와 동일 방향)
            let mut bmi: BITMAPINFO = mem::zeroed();
            bmi.bmiHeader = BITMAPINFOHEADER {
                biSize:          mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth:         self.width  as i32,
                biHeight:        -(self.height as i32), // top-down
                biPlanes:        1,
                biBitCount:      32,
                biCompression:   BI_RGB,
                ..mem::zeroed()
            };

            GetDIBits(
                mem_dc, bmp, 0, self.height,
                self.buf.as_mut_ptr() as *mut _,
                &mut bmi,
                DIB_RGB_COLORS,
            );

            // 정리 (역순)
            SelectObject(mem_dc, old);
            DeleteObject(bmp as *mut _);
            DeleteDC(mem_dc);
            ReleaseDC(std::ptr::null_mut(), screen);
        }
        Ok(&self.buf)
    }
}
