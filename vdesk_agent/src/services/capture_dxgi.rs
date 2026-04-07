//! DXGI Desktop Duplication API 화면 캡처
//!
//! screenshots(GDI) 대비 장점:
//!   - GPU 메모리에서 직접 캡처 → CPU 사용률 대폭 감소
//!   - 변경된 영역(DirtyRects) 정보 제공 (미래 최적화용)
//!   - 레이턴시 < 1ms (vs GDI ~15ms)
//!   - 출력: BGRA 포맷 (B=byte0, G=byte1, R=byte2, A=byte3)

use anyhow::{anyhow, Result};
use std::{mem, ptr, slice};

use winapi::{
    shared::{
        dxgi::*,
        dxgi1_2::*,
        dxgiformat::DXGI_FORMAT_B8G8R8A8_UNORM,
        dxgitype::*,
        minwindef::{TRUE, UINT},
        winerror::{DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, S_OK},
    },
    um::{
        d3d11::*,
        d3dcommon::D3D_DRIVER_TYPE_UNKNOWN,
        unknwnbase::IUnknown,
    },
};

// ── COM 스마트 포인터 (scrap 방식) ──────────────────────────────────────────

struct ComPtr<T>(*mut T);

impl<T> ComPtr<T> {
    fn null() -> Self { Self(ptr::null_mut()) }
    fn is_null(&self) -> bool { self.0.is_null() }
}

impl<T> Drop for ComPtr<T> {
    fn drop(&mut self) {
        unsafe {
            if !self.is_null() {
                (*(self.0 as *mut IUnknown)).Release();
            }
        }
    }
}

// ── DxgiCapture ─────────────────────────────────────────────────────────────

pub struct DxgiCapture {
    device:      ComPtr<ID3D11Device>,
    context:     ComPtr<ID3D11DeviceContext>,
    duplication: ComPtr<IDXGIOutputDuplication>,
    staging:     ComPtr<ID3D11Texture2D>,
    pub width:   u32,
    pub height:  u32,
    buf:         Vec<u8>, // BGRA 재사용 버퍼
}

unsafe impl Send for DxgiCapture {}

impl DxgiCapture {
    /// 주 모니터의 DXGI Desktop Duplication 캡처 생성
    pub fn new() -> Result<Self> {
        unsafe {
            // 1. D3D11 디바이스 생성
            let mut device: *mut ID3D11Device = ptr::null_mut();
            let mut context: *mut ID3D11DeviceContext = ptr::null_mut();
            let hr = D3D11CreateDevice(
                ptr::null_mut(),             // pAdapter: NULL = 기본 어댑터
                D3D_DRIVER_TYPE_UNKNOWN,     // DriverType: unknown (NULL adapter 사용 시)
                ptr::null_mut(),             // Software: 없음
                0,                           // Flags
                ptr::null_mut(),             // pFeatureLevels: 기본
                0,                           // FeatureLevels
                D3D11_SDK_VERSION,
                &mut device,
                ptr::null_mut(),             // pFeatureLevel: 무시
                &mut context,
            );
            // D3D_DRIVER_TYPE_UNKNOWN + NULL adapter 는 에러나므로 HARDWARE로 재시도
            let (device, context) = if hr != S_OK {
                let mut d: *mut ID3D11Device = ptr::null_mut();
                let mut c: *mut ID3D11DeviceContext = ptr::null_mut();
                let hr2 = D3D11CreateDevice(
                    ptr::null_mut(),
                    winapi::um::d3dcommon::D3D_DRIVER_TYPE_HARDWARE,
                    ptr::null_mut(), 0, ptr::null_mut(), 0,
                    D3D11_SDK_VERSION, &mut d, ptr::null_mut(), &mut c,
                );
                if hr2 != S_OK {
                    return Err(anyhow!("D3D11CreateDevice 실패: 0x{:08X}", hr2));
                }
                (d, c)
            } else {
                (device, context)
            };
            let device = ComPtr(device);
            let context = ComPtr(context);

            // 2. IDXGIDevice → IDXGIAdapter → IDXGIOutput → IDXGIOutput1
            let mut dxgi_device: *mut IDXGIDevice = ptr::null_mut();
            let hr = (*device.0).QueryInterface(
                &IID_IDXGIDevice,
                &mut dxgi_device as *mut *mut _ as *mut *mut _,
            );
            if hr != S_OK { return Err(anyhow!("QueryInterface IDXGIDevice: 0x{:08X}", hr)); }
            let dxgi_device = ComPtr(dxgi_device);

            let mut adapter: *mut IDXGIAdapter = ptr::null_mut();
            let hr = (*dxgi_device.0).GetParent(
                &IID_IDXGIAdapter,
                &mut adapter as *mut *mut _ as *mut *mut _,
            );
            if hr != S_OK { return Err(anyhow!("GetParent IDXGIAdapter: 0x{:08X}", hr)); }
            let adapter = ComPtr(adapter);

            let mut output: *mut IDXGIOutput = ptr::null_mut();
            let hr = (*adapter.0).EnumOutputs(0, &mut output);
            if hr != S_OK { return Err(anyhow!("EnumOutputs: 0x{:08X}", hr)); }
            let output = ComPtr(output);

            // IDXGIOutput → IDXGIOutput1
            let mut output1: *mut IDXGIOutput1 = ptr::null_mut();
            let hr = (*output.0).QueryInterface(
                &IID_IDXGIOutput1,
                &mut output1 as *mut *mut _ as *mut *mut _,
            );
            if hr != S_OK { return Err(anyhow!("QueryInterface IDXGIOutput1: 0x{:08X}", hr)); }
            let output1 = ComPtr(output1);

            // 3. Desktop Duplication 생성
            let mut duplication: *mut IDXGIOutputDuplication = ptr::null_mut();
            let hr = (*output1.0).DuplicateOutput(
                device.0 as *mut IUnknown,
                &mut duplication,
            );
            if hr != S_OK { return Err(anyhow!("DuplicateOutput: 0x{:08X}", hr)); }
            let duplication = ComPtr(duplication);

            // 4. 화면 크기 확인
            let mut desc: DXGI_OUTDUPL_DESC = mem::zeroed();
            (*duplication.0).GetDesc(&mut desc);
            let width  = desc.ModeDesc.Width;
            let height = desc.ModeDesc.Height;

            // 5. CPU 읽기용 스테이징 텍스처 생성
            let staging = Self::create_staging(&device, width, height)?;

            let buf_size = (width * height * 4) as usize;
            Ok(DxgiCapture {
                device,
                context,
                duplication,
                staging: ComPtr(staging),
                width,
                height,
                buf: vec![0u8; buf_size],
            })
        }
    }

    unsafe fn create_staging(device: &ComPtr<ID3D11Device>, w: u32, h: u32)
        -> Result<*mut ID3D11Texture2D>
    {
        let desc = D3D11_TEXTURE2D_DESC {
            Width:     w,
            Height:    h,
            MipLevels: 1,
            ArraySize: 1,
            Format:    DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage:     D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ,
            MiscFlags: 0,
        };
        let mut tex: *mut ID3D11Texture2D = ptr::null_mut();
        let hr = (*device.0).CreateTexture2D(&desc, ptr::null(), &mut tex);
        if hr != S_OK {
            return Err(anyhow!("CreateTexture2D staging: 0x{:08X}", hr));
        }
        Ok(tex)
    }

    /// 다음 프레임 캡처 (BGRA 슬라이스 반환)
    /// - Ok(Some(bgra)) : 새 프레임
    /// - Ok(None)       : 변화 없음 (timeout)
    /// - Err            : 재연결 필요
    pub fn capture(&mut self) -> Result<Option<&[u8]>> {
        unsafe {
            let mut frame_resource: *mut IDXGIResource = ptr::null_mut();
            let mut frame_info: DXGI_OUTDUPL_FRAME_INFO = mem::zeroed();

            let hr = (*self.duplication.0).AcquireNextFrame(
                33, // ms timeout (약 30fps 간격)
                &mut frame_info,
                &mut frame_resource,
            );

            if hr == DXGI_ERROR_WAIT_TIMEOUT {
                return Ok(None);
            }
            if hr == DXGI_ERROR_ACCESS_LOST {
                return Err(anyhow!("DXGI_ERROR_ACCESS_LOST: 재초기화 필요"));
            }
            if hr != S_OK {
                return Err(anyhow!("AcquireNextFrame: 0x{:08X}", hr));
            }

            let frame_resource = ComPtr(frame_resource);

            // LastPresentTime == 0 이면 변화 없는 프레임
            if *frame_info.LastPresentTime.QuadPart() == 0 {
                (*self.duplication.0).ReleaseFrame();
                return Ok(None);
            }

            // IDXGIResource → ID3D11Texture2D
            let mut desktop_tex: *mut ID3D11Texture2D = ptr::null_mut();
            let hr = (*frame_resource.0).QueryInterface(
                &IID_ID3D11Texture2D,
                &mut desktop_tex as *mut *mut _ as *mut *mut _,
            );
            if hr != S_OK {
                (*self.duplication.0).ReleaseFrame();
                return Err(anyhow!("QueryInterface ID3D11Texture2D: 0x{:08X}", hr));
            }
            let desktop_tex = ComPtr(desktop_tex);

            // GPU 텍스처 → 스테이징 텍스처 복사 (CPU 읽기 가능)
            (*self.context.0).CopyResource(
                self.staging.0 as *mut ID3D11Resource,
                desktop_tex.0 as *mut ID3D11Resource,
            );

            // 스테이징 텍스처 Map
            let mut mapped: D3D11_MAPPED_SUBRESOURCE = mem::zeroed();
            let hr = (*self.context.0).Map(
                self.staging.0 as *mut ID3D11Resource,
                0,
                D3D11_MAP_READ,
                0,
                &mut mapped,
            );
            (*self.duplication.0).ReleaseFrame();

            if hr != S_OK {
                return Err(anyhow!("Map staging: 0x{:08X}", hr));
            }

            // BGRA 데이터 복사 (stride 주의: 텍스처 행 간격 ≥ width*4)
            let row_pitch = mapped.RowPitch as usize;
            let w = self.width as usize;
            let h = self.height as usize;

            if row_pitch == w * 4 {
                // 패딩 없음: 한 번에 복사
                let src = slice::from_raw_parts(mapped.pData as *const u8, w * h * 4);
                self.buf.copy_from_slice(src);
            } else {
                // 행마다 복사 (stride 처리)
                let src = mapped.pData as *const u8;
                for row in 0..h {
                    let src_row = slice::from_raw_parts(src.add(row * row_pitch), w * 4);
                    let dst_row = &mut self.buf[row * w * 4..(row + 1) * w * 4];
                    dst_row.copy_from_slice(src_row);
                }
            }

            (*self.context.0).Unmap(self.staging.0 as *mut ID3D11Resource, 0);

            Ok(Some(&self.buf))
        }
    }
}
