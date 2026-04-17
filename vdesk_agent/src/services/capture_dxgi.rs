//! DXGI Desktop Duplication API 화면 캡처
//!
//! screenshots(GDI) 대비 장점:
//!   - GPU 메모리에서 직접 캡처 → CPU 사용률 대폭 감소
//!   - 변경된 영역(DirtyRects)만 부분 복사 → 추가 CPU/메모리 절약
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
        minwindef::UINT,
        windef::RECT,
        winerror::{DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, S_OK},
    },
    um::{
        d3d11::*,
        d3dcommon,
        unknwnbase::IUnknown,
    },
};

/// 변경된 영역을 나타내는 사각형
#[derive(Clone, Copy, Debug, Default)]
pub struct DirtyRect {
    pub left:   u32,
    pub top:    u32,
    pub right:  u32,
    pub bottom: u32,
}

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

// ── CaptureFrame ─────────────────────────────────────────────────────────────

/// capture() 가 반환하는 프레임 정보
pub struct CaptureFrame<'a> {
    pub bgra:             &'a [u8],
    pub is_full_frame:    bool,
    pub dirty_rects:      &'a [DirtyRect],
    /// DXGI가 DirtyRects 정보를 제공했는지 여부.
    /// false(num_rects==0)이면 어느 영역이 바뀌었는지 알 수 없으므로
    /// 해시 스킵을 적용하면 안 됨.
    pub has_dirty_rects:  bool,
}

// ── DxgiCapture ─────────────────────────────────────────────────────────────

pub struct DxgiCapture {
    device:           ComPtr<ID3D11Device>,
    context:          ComPtr<ID3D11DeviceContext>,
    duplication:      ComPtr<IDXGIOutputDuplication>,
    staging:          ComPtr<ID3D11Texture2D>,
    pub width:        u32,
    pub height:       u32,
    buf:              Vec<u8>,        // BGRA 재사용 버퍼 (이전 프레임 상태 유지)
    dirty_rects_raw:  Vec<RECT>,      // winapi RECT 임시 버퍼
    dirty_rects:      Vec<DirtyRect>, // 현재 프레임의 변경 영역
    is_full_frame:    bool,
    has_dirty_rects:  bool,           // 현재 프레임에 DirtyRects 정보가 있는지
    first_frame:      bool,
}

unsafe impl Send for DxgiCapture {}

impl DxgiCapture {
    /// 주 모니터의 DXGI Desktop Duplication 캡처 생성
    pub fn new() -> Result<Self> {
        unsafe {
            // 1. D3D11 하드웨어 디바이스 생성
            let mut device: *mut ID3D11Device = ptr::null_mut();
            let mut context: *mut ID3D11DeviceContext = ptr::null_mut();
            let hr = D3D11CreateDevice(
                ptr::null_mut(),                               // pAdapter: NULL = 기본 어댑터
                winapi::um::d3dcommon::D3D_DRIVER_TYPE_HARDWARE, // HARDWARE 직접 사용
                ptr::null_mut(), 0, ptr::null_mut(), 0,
                D3D11_SDK_VERSION,
                &mut device,
                ptr::null_mut(),
                &mut context,
            );
            if hr != S_OK {
                return Err(anyhow!("D3D11CreateDevice 실패: 0x{:08X}", hr));
            }
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
                buf:              vec![0u8; buf_size],
                dirty_rects_raw:  Vec::new(),
                dirty_rects:      Vec::new(),
                is_full_frame:    true,
                has_dirty_rects:  false,
                first_frame:      true,
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

    /// 다음 프레임 캡처
    /// - Ok(Some(frame)) : 새 프레임. frame.bgra, frame.is_full_frame, frame.dirty_rects 제공
    /// - Ok(None)        : 변화 없음 (timeout 또는 LastPresentTime==0)
    /// - Err             : 재연결 필요
    pub fn capture(&mut self) -> Result<Option<CaptureFrame<'_>>> {
        unsafe {
            let mut frame_resource: *mut IDXGIResource = ptr::null_mut();
            let mut frame_info: DXGI_OUTDUPL_FRAME_INFO = mem::zeroed();

            let hr = (*self.duplication.0).AcquireNextFrame(
                16, // ms timeout (약 60fps 간격)
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

            // ── DirtyRects 조회 ──────────────────────────────────────────────
            let mut required: UINT = 0;
            (*self.duplication.0).GetFrameDirtyRects(0, ptr::null_mut(), &mut required);
            let num_rects = required as usize / mem::size_of::<RECT>();
            self.dirty_rects_raw.resize(num_rects, mem::zeroed());
            if num_rects > 0 {
                (*self.duplication.0).GetFrameDirtyRects(
                    required,
                    self.dirty_rects_raw.as_mut_ptr(),
                    &mut required,
                );
            }

            // 전체 화면 면적의 50% 이상 변경 시 → 전체 복사 (부분 복사 오버헤드 방지)
            let screen_area = (self.width * self.height) as i64;
            let dirty_area: i64 = self.dirty_rects_raw.iter().map(|r| {
                (r.right - r.left).max(0) as i64 * (r.bottom - r.top).max(0) as i64
            }).sum();
            let use_full_copy = self.first_frame || num_rects == 0 || dirty_area * 2 >= screen_area;
            // num_rects==0이면 어느 영역이 바뀌었는지 모름 → 해시 스킵 금지
            self.has_dirty_rects = num_rects > 0;

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

            if use_full_copy {
                // ── 전체 복사 ────────────────────────────────────────────────
                (*self.context.0).CopyResource(
                    self.staging.0 as *mut ID3D11Resource,
                    desktop_tex.0 as *mut ID3D11Resource,
                );
            } else {
                // ── 부분 복사: 변경된 영역만 ────────────────────────────────
                for rect in &self.dirty_rects_raw {
                    let box_ = D3D11_BOX {
                        left:  rect.left  as UINT,
                        top:   rect.top   as UINT,
                        front: 0,
                        right: rect.right as UINT,
                        bottom: rect.bottom as UINT,
                        back:  1,
                    };
                    (*self.context.0).CopySubresourceRegion(
                        self.staging.0 as *mut ID3D11Resource,
                        0,
                        rect.left as UINT, rect.top as UINT, 0,
                        desktop_tex.0 as *mut ID3D11Resource,
                        0,
                        &box_,
                    );
                }
            }

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

            let row_pitch = mapped.RowPitch as usize;
            let w = self.width as usize;
            let h = self.height as usize;
            let src_base = mapped.pData as *const u8;

            if use_full_copy {
                // ── 전체 버퍼 복사 (stride 처리) ────────────────────────────
                if row_pitch == w * 4 {
                    let src = slice::from_raw_parts(src_base, w * h * 4);
                    self.buf.copy_from_slice(src);
                } else {
                    for row in 0..h {
                        let src_row = slice::from_raw_parts(src_base.add(row * row_pitch), w * 4);
                        let dst_row = &mut self.buf[row * w * 4..(row + 1) * w * 4];
                        dst_row.copy_from_slice(src_row);
                    }
                }
                // dirty_rects 를 전체 화면 1개 rect 로 표시
                self.dirty_rects.clear();
                self.dirty_rects.push(DirtyRect { left: 0, top: 0, right: self.width, bottom: self.height });
                self.is_full_frame = true;
            } else {
                // ── 변경 영역만 버퍼 업데이트 ────────────────────────────────
                self.dirty_rects.clear();
                for rect in &self.dirty_rects_raw {
                    let x0 = rect.left  as usize;
                    let x1 = (rect.right as usize).min(w);
                    let y0 = rect.top    as usize;
                    let y1 = (rect.bottom as usize).min(h);
                    let col_bytes = (x1 - x0) * 4;

                    for row in y0..y1 {
                        let src_off = row * row_pitch + x0 * 4;
                        let dst_off = row * w * 4 + x0 * 4;
                        let src_slice = slice::from_raw_parts(src_base.add(src_off), col_bytes);
                        self.buf[dst_off..dst_off + col_bytes].copy_from_slice(src_slice);
                    }

                    self.dirty_rects.push(DirtyRect {
                        left:   rect.left  as u32,
                        top:    rect.top   as u32,
                        right:  rect.right as u32,
                        bottom: rect.bottom as u32,
                    });
                }
                self.is_full_frame = false;
            }

            (*self.context.0).Unmap(self.staging.0 as *mut ID3D11Resource, 0);
            self.first_frame = false;

            Ok(Some(CaptureFrame {
                bgra:            &self.buf,
                is_full_frame:   self.is_full_frame,
                dirty_rects:     &self.dirty_rects,
                has_dirty_rects: self.has_dirty_rects,
            }))
        }
    }
}

// ── 이전 프로세스 고스트 상태 강제 회수 ──────────────────────────────────────

/// 에이전트 시작 / 세션 시작 직전에 호출.
///
/// 이전 에이전트 프로세스가 비정상 종료되면 GPU 드라이버에 Desktop Duplication
/// "고스트 상태"가 남아 다음 `DuplicateOutput` 호출이 `0x8000FFFF`를 반환한다.
/// `IDXGIOutput::TakeOwnership(exclusive=true)` + `ReleaseOwnership()`으로
/// 드라이버가 이전 소유권을 강제로 회수하도록 트리거한다.
pub fn reclaim_output() {
    unsafe {
        // 임시 D3D 디바이스 생성
        let mut device: *mut ID3D11Device = ptr::null_mut();
        let mut _ctx:   *mut ID3D11DeviceContext = ptr::null_mut();
        let hr = D3D11CreateDevice(
            ptr::null_mut(),
            winapi::um::d3dcommon::D3D_DRIVER_TYPE_HARDWARE,
            ptr::null_mut(), 0, ptr::null_mut(), 0,
            D3D11_SDK_VERSION, &mut device, ptr::null_mut(), &mut _ctx,
        );
        if hr != S_OK { return; }
        // 로컬 변수는 역선언 순서로 drop → device가 마지막 (올바른 COM 해제 순서)
        let _ctx    = ComPtr(_ctx);
        let device  = ComPtr(device);

        // IDXGIDevice → IDXGIAdapter → IDXGIOutput
        let mut dxgi_dev: *mut IDXGIDevice = ptr::null_mut();
        if (*device.0).QueryInterface(
            &IID_IDXGIDevice,
            &mut dxgi_dev as *mut *mut _ as *mut *mut _,
        ) != S_OK { return; }
        let dxgi_dev = ComPtr(dxgi_dev);

        let mut adapter: *mut IDXGIAdapter = ptr::null_mut();
        if (*dxgi_dev.0).GetParent(
            &IID_IDXGIAdapter,
            &mut adapter as *mut *mut _ as *mut *mut _,
        ) != S_OK { return; }
        let adapter = ComPtr(adapter);

        let mut output: *mut IDXGIOutput = ptr::null_mut();
        if (*adapter.0).EnumOutputs(0, &mut output) != S_OK { return; }
        let output = ComPtr(output);

        // TakeOwnership(exclusive=true): 이전 프로세스 Desktop Duplication 강제 회수
        // ReleaseOwnership: 즉시 반납하여 후속 DuplicateOutput이 성공하도록 함
        let hr = (*output.0).TakeOwnership(device.0 as *mut IUnknown, 1 /* TRUE = exclusive */);
        if hr == S_OK {
            (*output.0).ReleaseOwnership();
            hbb_common::log::info!("[dxgi] 이전 세션 고스트 상태 강제 회수 완료");
        } else {
            hbb_common::log::debug!("[dxgi] TakeOwnership 불가 (hr=0x{:08X}) — 건너뜀", hr);
        }
        // output → adapter → dxgi_dev → _ctx → device 순으로 자동 drop (역선언 순서)
    }
}

// ── 명시적 Drop: 올바른 COM 해제 순서 보장 ───────────────────────────────────
//
// Rust는 struct 필드를 선언 순서대로 drop하므로, 기본 동작 시 device가 가장 먼저
// Release된다. 그러나 COM 규칙상 자식 객체(duplication, staging)는 부모(device)보다
// 먼저 해제되어야 한다. 순서가 잘못되면 GPU 드라이버가 Desktop Duplication 상태를
// 완전히 회수하지 못해 다음 DuplicateOutput 호출이 0x8000FFFF를 반환한다.
impl Drop for DxgiCapture {
    fn drop(&mut self) {
        unsafe {
            // 파이프라인 바인딩 초기화 + GPU 커맨드 큐 비우기
            // (진행 중인 CopyResource/Map 작업이 완료된 뒤 리소스를 해제하기 위해 필수)
            if !self.context.is_null() {
                (*self.context.0).ClearState();
                (*self.context.0).Flush();
            }

            // 자식 → 부모 순서로 명시적 Release
            // (포인터를 null로 만들어 ComPtr 자동 Drop에서 중복 Release 방지)
            if !self.staging.is_null() {
                (*(self.staging.0 as *mut IUnknown)).Release();
                self.staging.0 = ptr::null_mut();
            }
            if !self.duplication.is_null() {
                (*(self.duplication.0 as *mut IUnknown)).Release();
                self.duplication.0 = ptr::null_mut();
            }
            if !self.context.is_null() {
                (*(self.context.0 as *mut IUnknown)).Release();
                self.context.0 = ptr::null_mut();
            }
            // device는 모든 자식 해제 후 마지막으로 Release
            if !self.device.is_null() {
                (*(self.device.0 as *mut IUnknown)).Release();
                self.device.0 = ptr::null_mut();
            }
        }
        // ComPtr<T>의 자동 Drop: is_null() == true이므로 Release 호출 없음
    }
}
