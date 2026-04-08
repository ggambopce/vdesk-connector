//! 원격 화면 렌더링 — winit + softbuffer
//!
//! 원격 제어 모드:
//!   - 뷰어 창 클릭 → 원격 제어 ON (클릭 미전달) + 커서를 창 안에 가둠 + 녹색 테두리
//!   - ESC → 제어 OFF, 커서 가둠 해제. 포커스 이탈만으로는 해제하지 않음
//!   - 가둔 동안 마우스는 뷰어 클라이언트 좌표로만 에이전트에 전달 (호스트는 SendInput 경로로만 반영)
//!
//! 단축키:
//!   F11  — 전체화면 토글 (항상 로컬 처리)
//!   Escape — 원격 제어 모드 해제
//!
//! 같은 PC 테스트 시 화면 중첩 방지:
//!   Windows WDA_EXCLUDEFROMCAPTURE 적용 → 에이전트 캡처 시 뷰어 창이 검게 처리됨
//!
//! VDESK_DIRECT=1 또는 VDESK_MOUSE_GLOBAL=1 이면 마우스를 가상 화면 절대 좌표(0x07)로 보냄.
//! 그렇지 않으면 클라이언트 비율→주 모니터만 가정(0x01) — 같은 PC에서 커서가 어긋날 수 있음.

use anyhow::Result;
use softbuffer::Surface;
use std::{num::NonZeroU32, sync::Arc};
use winit::{
    application::ApplicationHandler,
    event::{
        DeviceEvent, DeviceId, ElementState, KeyEvent as WinitKeyEvent,
        MouseButton, MouseScrollDelta, WindowEvent,
    },
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    keyboard::{KeyCode, PhysicalKey},
    window::{CursorGrabMode, CursorIcon, Fullscreen, Window, WindowAttributes, WindowId},
};

/// 렌더링할 프레임 (XRGB 픽셀)
pub struct FrameBuffer {
    pub pixels: Vec<u32>,
    pub width:  u32,
    pub height: u32,
}

/// 뷰어 창에서 수집한 입력 이벤트
#[derive(Debug)]
pub enum InputEvent {
    /// 마우스 이동 — 뷰어 창 좌표 + 현재 창 크기 (원격 PC 일반 경로)
    MouseMove { x: i32, y: i32, win_w: u32, win_h: u32 },
    /// 가상 화면 기준 물리 픽셀 (inner_position + 클라이언트 좌표, 같은 PC 권장)
    MouseMoveGlobal { gx: i32, gy: i32 },
    /// 마우스 버튼 (button: 0=Left, 2=Right, 4=Middle)
    MouseButton { button: u32, pressed: bool },
    /// 물리 키 (winit KeyCode discriminant)
    KeyPress { key: u32, pressed: bool },
    /// 스크롤 휠 (Windows notch 단위: 한 칸 = 120)
    Scroll { dx: i16, dy: i16 },
    /// 글로벌 키보드 훅에서 캡처한 VK 코드 (포커스 무관)
    KeyVk { vk: u32, scan: u16, pressed: bool, extended: bool },
}

/// winit 사용자 이벤트 (프레임 채널에서 수신)
pub enum ViewerEvent {
    Frame(FrameBuffer),
    Close,
}

struct ViewerApp {
    window:        Option<Arc<Window>>,
    context:       Option<softbuffer::Context<Arc<Window>>>,
    surface:       Option<Surface<Arc<Window>, Arc<Window>>>,
    current_frame: Option<FrameBuffer>,
    input_tx:      Option<tokio::sync::mpsc::UnboundedSender<InputEvent>>,
    /// 현재 뷰어 창 크기 (픽셀)
    window_size:   (u32, u32),
    /// 마지막으로 resize()에 적용한 surface 크기
    surface_size:  (u32, u32),

    // ── 원격 제어 모드 ──────────────────────────────────────────────────────
    /// 원격 제어 모드 활성화 여부
    control_active: bool,
    /// 마지막 CursorMoved (비활성 시에도 갱신 — 진입 직후 위치 동기화)
    last_cursor: Option<(i32, i32)>,
    /// true면 MouseMoveGlobal 전송 (VDESK_DIRECT / VDESK_MOUSE_GLOBAL)
    mouse_global: bool,

    // ── 스케일링 캐시 ────────────────────────────────────────────────────────
    /// x축 소스 인덱스 사전 계산 테이블 — 창 크기/프레임 크기 변경 시에만 재계산
    /// 픽셀당 나눗셈을 제거하여 렌더링 CPU 부하 감소
    x_map: Vec<u32>,
    /// x_map을 계산한 시점의 (dst_w, src_w)
    x_map_key: (u32, u32),
}

impl ApplicationHandler<ViewerEvent> for ViewerApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_none() {
            let attrs = WindowAttributes::default()
                .with_title("VDesk Viewer — 클릭하여 원격 제어")
                .with_maximized(true);
            let window = Arc::new(event_loop.create_window(attrs).unwrap());

            // ── 같은 PC 테스트: 에이전트 화면 캡처에서 뷰어 창 제외 ──────────
            #[cfg(target_os = "windows")]
            set_exclude_from_capture(&window);

            let context = softbuffer::Context::new(window.clone()).unwrap();
            let surface = Surface::new(&context, window.clone()).unwrap();

            let size = window.inner_size();
            self.window_size = (size.width.max(1), size.height.max(1));
            self.context = Some(context);
            self.surface = Some(surface);
            self.window  = Some(window);
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            // ── 창 시스템 이벤트 ────────────────────────────────────────────
            WindowEvent::CloseRequested => {
                if self.control_active {
                    self.deactivate_control_mode();
                }
                event_loop.exit();
            }
            WindowEvent::RedrawRequested => self.redraw(),

            // 창 크기 변경: surface 재조정 + 리드로우
            WindowEvent::Resized(size) => {
                self.window_size = (size.width.max(1), size.height.max(1));
                self.surface_size = (0, 0); // 캐시 무효화
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }

            // ── 마우스 이동 ─────────────────────────────────────────────────
            WindowEvent::CursorMoved { position, .. } => {
                if is_agent_injected() { return; } // 에이전트 주입 이벤트 무시
                let x = position.x as i32;
                let y = position.y as i32;
                self.last_cursor = Some((x, y));
                if self.control_active {
                    // Windows WM_SETCURSOR가 커서 아이콘을 리셋하므로 매번 재적용
                    if let Some(w) = &self.window {
                        w.set_cursor(CursorIcon::Crosshair);
                    }
                    self.send_pointer_move(x, y);
                }
            }

            // ── 마우스 버튼 ─────────────────────────────────────────────────
            WindowEvent::MouseInput { state, button, .. } => {
                if is_agent_injected() { return; } // 에이전트 주입 이벤트 무시
                // 비활성 상태에서 Left 클릭 → 원격 제어 모드 진입
                if !self.control_active
                    && button == MouseButton::Left
                    && state == ElementState::Pressed
                {
                    self.activate_control_mode();
                    // 첫 클릭도 바로 전달해서 "클릭이 안 먹는" 느낌을 없앰.
                    // (release는 다음 이벤트에서 정상 전달됨)
                }

                // 활성 상태에서만 에이전트로 전달
                if self.control_active {
                    if let Some(tx) = &self.input_tx {
                        let btn = match button {
                            MouseButton::Left   => 0u32,
                            MouseButton::Right  => 2u32,
                            MouseButton::Middle => 4u32,
                            _ => return,
                        };
                        let _ = tx.send(InputEvent::MouseButton {
                            button:  btn,
                            pressed: state == ElementState::Pressed,
                        });
                    }
                }
            }

            // ── 스크롤 휠 ───────────────────────────────────────────────────
            WindowEvent::MouseWheel { delta, .. } => {
                if is_agent_injected() { return; } // 에이전트 주입 이벤트 무시
                if !self.control_active {
                    return;
                }
                if let Some(tx) = &self.input_tx {
                    let (dx, dy) = match delta {
                        MouseScrollDelta::LineDelta(x, y) => {
                            ((x * 120.0) as i16, (y * 120.0) as i16)
                        }
                        MouseScrollDelta::PixelDelta(pos) => {
                            (pos.x as i16, pos.y as i16)
                        }
                    };
                    let _ = tx.send(InputEvent::Scroll { dx, dy });
                }
            }

            // ── 키보드 ──────────────────────────────────────────────────────
            WindowEvent::KeyboardInput {
                event: WinitKeyEvent { .. }, ..
            } if is_agent_injected() => {} // 에이전트 주입 키 무시
            WindowEvent::KeyboardInput {
                event: WinitKeyEvent {
                    physical_key: PhysicalKey::Code(code),
                    state,
                    ..
                },
                ..
            } => {
                // F11: 전체화면 토글 — 항상 로컬 처리, 에이전트 미전달
                if code == KeyCode::F11 && state == ElementState::Pressed {
                    if let Some(window) = &self.window {
                        if window.fullscreen().is_some() {
                            window.set_fullscreen(None);
                        } else {
                            window.set_fullscreen(Some(Fullscreen::Borderless(None)));
                        }
                    }
                    return;
                }

                // Escape: 원격 제어 모드 해제 — 에이전트 미전달
                if code == KeyCode::Escape && state == ElementState::Pressed && self.control_active {
                    self.deactivate_control_mode();
                    return;
                }

                // 나머지 키: 제어 모드 활성 시에만 에이전트로 전달
                if self.control_active {
                    if let Some(tx) = &self.input_tx {
                        let _ = tx.send(InputEvent::KeyPress {
                            key:     code as u32,
                            pressed: state == ElementState::Pressed,
                        });
                    }
                }
            }

            _ => {}
        }
    }

    fn device_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _device_id: DeviceId,
        _event: DeviceEvent,
    ) {
        // 제어 모드에서 Confined grab — 상대 이동은 CursorMoved로 처리
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: ViewerEvent) {
        match event {
            ViewerEvent::Frame(f) => {
                self.current_frame = Some(f);
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            ViewerEvent::Close => {}
        }
    }
}

impl ViewerApp {
    fn send_pointer_move(&mut self, x: i32, y: i32) {
        let Some(tx) = &self.input_tx else { return };
        if self.mouse_global {
            if let Some(w) = &self.window {
                if let Ok(origin) = w.inner_position() {
                    let gx = origin.x + x;
                    let gy = origin.y + y;
                    let _ = tx.send(InputEvent::MouseMoveGlobal { gx, gy });
                    return;
                }
            }
        }
        let (ww, wh) = self.window_size;
        let _ = tx.send(InputEvent::MouseMove {
            x,
            y,
            win_w: ww,
            win_h: wh,
        });
    }

    /// 원격 제어 모드 활성화
    fn activate_control_mode(&mut self) {
        let Some(window) = &self.window else { return };

        // 커서를 창 안에 가둠 (VM 제어 시 실수 클릭 방지)
        if let Err(e) = window.set_cursor_grab(CursorGrabMode::Confined) {
            hbb_common::log::warn!("[display] CursorGrab::Confined 실패: {:?}", e);
        }
        window.set_cursor_visible(true);
        window.set_cursor(CursorIcon::Crosshair);
        window.set_title("VDesk Viewer [원격 제어 중] — ESC로 해제");
        self.control_active = true;

        // 글로벌 키보드 훅: 뷰어 포커스 여부와 무관하게 모든 키 캡처
        self.install_kb_hook();

        if let Some((x, y)) = self.last_cursor {
            self.send_pointer_move(x, y);
        }

        hbb_common::log::info!("[display] 원격 제어 모드 ON");
    }

    /// 원격 제어 모드 비활성화
    fn deactivate_control_mode(&mut self) {
        let Some(window) = &self.window else { return };

        self.remove_kb_hook();
        let _ = window.set_cursor_grab(CursorGrabMode::None);
        window.set_cursor(CursorIcon::Default);
        window.set_title("VDesk Viewer — 클릭하여 원격 제어");
        self.control_active = false;
        window.request_redraw(); // 녹색 테두리 즉시 제거

        hbb_common::log::info!("[display] 원격 제어 모드 OFF");
    }

    /// 글로벌 키보드 훅 설치 (Windows 전용)
    #[cfg(target_os = "windows")]
    fn install_kb_hook(&self) {
        if let Some(tx) = &self.input_tx {
            let ptr = Box::into_raw(Box::new(tx.clone()));
            global_kb::TX_PTR.store(ptr, std::sync::atomic::Ordering::Relaxed);
        }
        extern "system" {
            fn SetWindowsHookExA(
                id_hook: i32,
                lpfn: unsafe extern "system" fn(i32, usize, isize) -> isize,
                h_mod: isize, dw_thread_id: u32,
            ) -> isize;
        }
        let h = unsafe { SetWindowsHookExA(13, global_kb::hook_proc, 0, 0) }; // WH_KEYBOARD_LL=13
        global_kb::HOOK.store(h, std::sync::atomic::Ordering::Relaxed);
        hbb_common::log::info!("[display] 글로벌 키보드 훅 설치 (h={})", h);
    }
    #[cfg(not(target_os = "windows"))]
    fn install_kb_hook(&self) {}

    /// 글로벌 키보드 훅 제거 (Windows 전용)
    #[cfg(target_os = "windows")]
    fn remove_kb_hook(&self) {
        extern "system" { fn UnhookWindowsHookEx(hhk: isize) -> i32; }
        let h = global_kb::HOOK.swap(0, std::sync::atomic::Ordering::Relaxed);
        if h != 0 { unsafe { UnhookWindowsHookEx(h); } }
        let ptr = global_kb::TX_PTR.swap(std::ptr::null_mut(), std::sync::atomic::Ordering::Relaxed);
        if !ptr.is_null() { unsafe { drop(Box::from_raw(ptr)); } }
        hbb_common::log::info!("[display] 글로벌 키보드 훅 제거");
    }
    #[cfg(not(target_os = "windows"))]
    fn remove_kb_hook(&self) {}

    fn redraw(&mut self) {
        let (Some(surface), Some(frame)) = (&mut self.surface, &self.current_frame) else {
            return;
        };

        let (win_w, win_h) = self.window_size;
        let win_w = win_w.max(1);
        let win_h = win_h.max(1);

        // surface를 창 크기에 맞게 resize (크기 변경 시에만)
        if self.surface_size != (win_w, win_h) {
            let nw = NonZeroU32::new(win_w).unwrap();
            let nh = NonZeroU32::new(win_h).unwrap();
            if surface.resize(nw, nh).is_ok() {
                self.surface_size = (win_w, win_h);
            } else {
                return;
            }
        }

        // x축 인덱스 맵 캐시 갱신 (창 크기 or 프레임 크기 변경 시에만)
        let x_key = (win_w, frame.width);
        if self.x_map_key != x_key {
            self.x_map = (0..win_w).map(|dx| dx * frame.width / win_w).collect();
            self.x_map_key = x_key;
        }

        if let Ok(mut buf) = surface.buffer_mut() {
            // 프레임을 창 크기에 맞게 스케일링
            scale_pixels(
                &frame.pixels, frame.width, frame.height,
                &mut buf,      win_w,       win_h,
                &self.x_map,
            );

            // 원격 제어 모드 활성 시 녹색 테두리 오버레이
            if self.control_active {
                draw_control_border(&mut buf, win_w, win_h);
            }

            let _ = buf.present();
        }
    }
}

/// 프레임을 창 크기에 맞게 최근접 스케일링 (꽉 채움)
///
/// x_map: 호출자가 캐싱한 x축 소스 인덱스 테이블 (dst_w 길이)
/// 픽셀당 나눗셈을 dst_w + dst_h 번으로 줄임 (기존 dst_w × dst_h 번)
fn scale_pixels(
    src: &[u32], src_w: u32, src_h: u32,
    dst: &mut [u32], dst_w: u32, dst_h: u32,
    x_map: &[u32],
) {
    if src_w == 0 || src_h == 0 {
        return;
    }
    // 동일 크기: memcpy fast path
    if src_w == dst_w && src_h == dst_h {
        dst.copy_from_slice(src);
        return;
    }
    let dst_w_us = dst_w as usize;
    for dy in 0..dst_h {
        let src_row = (dy * src_h / dst_h) as usize * src_w as usize;
        let dst_off = dy as usize * dst_w_us;
        let src_slice = &src[src_row..];
        let dst_row   = &mut dst[dst_off..dst_off + dst_w_us];
        for (dst_px, &sx) in dst_row.iter_mut().zip(x_map.iter()) {
            *dst_px = src_slice[sx as usize];
        }
    }
}

/// 원격 제어 모드 활성 시 녹색 테두리 표시 (두께 4px)
fn draw_control_border(buf: &mut softbuffer::Buffer<'_, Arc<Window>, Arc<Window>>, w: u32, h: u32) {
    const COLOR: u32 = 0x0022_CC22; // 진한 녹색 (XRGB)
    const T: u32     = 4;           // 테두리 두께

    let thick = T.min(h / 2).min(w / 2);
    for x in 0..w {
        for t in 0..thick {
            buf[(t * w + x) as usize]           = COLOR; // 상단
            buf[((h - 1 - t) * w + x) as usize] = COLOR; // 하단
        }
    }
    for y in 0..h {
        for t in 0..thick {
            buf[(y * w + t) as usize]           = COLOR; // 좌측
            buf[(y * w + (w - 1 - t)) as usize] = COLOR; // 우측
        }
    }
}

/// Windows: 현재 메시지가 에이전트 SendInput 주입인지 확인
/// 에이전트는 dwExtraInfo=0x5DEC_0001 로 마킹한다.
/// 뷰어가 이 값을 보면 피드백 루프로 돌아온 이벤트이므로 무시한다.
#[cfg(target_os = "windows")]
fn is_agent_injected() -> bool {
    extern "system" {
        fn GetMessageExtraInfo() -> usize;
    }
    unsafe { GetMessageExtraInfo() == 0x5DEC_0001 }
}
#[cfg(not(target_os = "windows"))]
fn is_agent_injected() -> bool { false }

/// Windows: 이 창을 화면 캡처 API에서 제외 (WDA_EXCLUDEFROMCAPTURE)
/// 같은 PC에서 에이전트+뷰어 실행 시 무한 거울 현상을 방지합니다.
#[cfg(target_os = "windows")]
fn set_exclude_from_capture(window: &Window) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    extern "system" {
        fn SetWindowDisplayAffinity(hwnd: isize, dw_affinity: u32) -> i32;
    }
    const WDA_EXCLUDEFROMCAPTURE: u32 = 0x00000011;

    let Ok(handle) = window.window_handle() else { return };
    let RawWindowHandle::Win32(h) = handle.as_raw() else { return };

    let result = unsafe { SetWindowDisplayAffinity(h.hwnd.get(), WDA_EXCLUDEFROMCAPTURE) };
    if result != 0 {
        hbb_common::log::info!("[display] WDA_EXCLUDEFROMCAPTURE 적용 완료");
    } else {
        hbb_common::log::warn!("[display] WDA_EXCLUDEFROMCAPTURE 적용 실패");
    }
}

/// 글로벌 저수준 키보드 훅 — 뷰어 포커스 없이도 모든 키 입력 캡처
/// ESC·F11은 로컬 처리를 위해 패스스루. 나머지는 에이전트로 전달 후 로컬 억제.
#[cfg(target_os = "windows")]
mod global_kb {
    use std::sync::atomic::{AtomicIsize, AtomicPtr};
    use super::InputEvent;

    /// 훅 핸들
    pub static HOOK: AtomicIsize = AtomicIsize::new(0);
    /// Box<UnboundedSender<InputEvent>> 를 raw pointer로 보관
    pub static TX_PTR: AtomicPtr<tokio::sync::mpsc::UnboundedSender<InputEvent>> =
        AtomicPtr::new(std::ptr::null_mut());

    const MARK:         usize = 0x5DEC_0001; // 에이전트 주입 이벤트 마커
    const LLKHF_EXT:    u32   = 0x01;
    const HC_ACTION:    i32   = 0;
    const WM_KEYDOWN:   usize = 0x0100;
    const WM_SYSKEYDOWN:usize = 0x0104;
    const VK_ESCAPE:    u32   = 0x1B;
    const VK_F11:       u32   = 0x7A;

    #[repr(C)]
    struct KbdHook { vk: u32, scan: u32, flags: u32, time: u32, extra: usize }

    extern "system" {
        fn CallNextHookEx(h: isize, code: i32, w: usize, l: isize) -> isize;
    }

    /// LL 키보드 훅 프로시저 — winit 메인 스레드에서 실행
    pub unsafe extern "system" fn hook_proc(code: i32, w: usize, l: isize) -> isize {
        if code == HC_ACTION {
            let kb = &*(l as *const KbdHook);
            // 에이전트가 주입한 이벤트는 패스스루 (뷰어에서 마커로 무시됨)
            if kb.extra != MARK {
                // ESC, F11 은 로컬 처리 유지
                if kb.vk != VK_ESCAPE && kb.vk != VK_F11 {
                    let ptr = TX_PTR.load(std::sync::atomic::Ordering::Relaxed);
                    if !ptr.is_null() {
                        let _ = (*ptr).send(InputEvent::KeyVk {
                            vk:       kb.vk,
                            scan:     kb.scan as u16,
                            pressed:  w == WM_KEYDOWN || w == WM_SYSKEYDOWN,
                            extended: (kb.flags & LLKHF_EXT) != 0,
                        });
                    }
                    return 1; // 로컬 앱에 전달하지 않음
                }
            }
        }
        CallNextHookEx(0, code, w, l)
    }
}

/// winit 이벤트 루프 실행 (반드시 메인 스레드에서 호출)
pub fn run_event_loop(
    frame_rx: std::sync::mpsc::Receiver<FrameBuffer>,
    input_tx: Option<tokio::sync::mpsc::UnboundedSender<InputEvent>>,
    mouse_global: bool,
) -> Result<()> {
    if mouse_global {
        hbb_common::log::info!("[display] 마우스: 가상 화면 절대 좌표 모드 (0x07)");
    }

    let event_loop = EventLoop::<ViewerEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();

    // 프레임 채널 → 이벤트 루프 전달 스레드
    std::thread::spawn(move || {
        while let Ok(frame) = frame_rx.recv() {
            if proxy.send_event(ViewerEvent::Frame(frame)).is_err() {
                break;
            }
        }
    });

    let mut app = ViewerApp {
        window:          None,
        context:         None,
        surface:         None,
        current_frame:   None,
        input_tx,
        window_size:     (1280, 720),
        surface_size:    (0, 0),
        control_active:  false,
        last_cursor:     None,
        mouse_global,
        x_map:           Vec::new(),
        x_map_key:       (0, 0),
    };

    // Wait: 프레임/입력 이벤트가 있을 때만 루프 실행
    event_loop.set_control_flow(ControlFlow::Wait);
    event_loop.run_app(&mut app)?;
    Ok(())
}
