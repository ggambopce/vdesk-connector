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
use std::{num::NonZeroU32, sync::{atomic::AtomicU8, Arc}};

/// 에이전트에서 수신한 원격 커서 타입 (session.rs에서 갱신)
/// 0=Arrow 1=IBeam 2=SizeWE 3=SizeNS 4=SizeNWSE 5=SizeNESW 6=SizeAll 7=Hand 8=Wait 9=No
pub static REMOTE_CURSOR_TYPE: AtomicU8 = AtomicU8::new(0);
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

    // ── 스크롤 오프셋 ────────────────────────────────────────────────────────
    /// 프레임 내 뷰포트 좌상단 위치 (픽셀). 창이 프레임보다 작을 때 패닝에 사용.
    scroll_x: u32,
    scroll_y: u32,

    /// 현재 눌린 마우스 버튼 상태 (비트마스크: bit0=Left, bit1=Right, bit2=Middle)
    /// release_all_inputs 시 실제로 눌린 버튼만 해제 전송 (오발 방지)
    mouse_btns: u8,
    /// 제어 모드에서 커서가 창 가장자리에 있는지 여부
    /// true면 grab 해제 상태 — 에이전트 마우스 전달 중단, OS 리사이즈 허용
    at_edge: bool,
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
                self.surface_size = (0, 0);
                // 리사이즈 완료 후 grab 복구 — trigger_edge_resize가 ClipCursor(NULL)로
                // grab을 해제했으므로 새 창 크기 기준으로 재적용
                if self.control_active {
                    self.at_edge = false;
                    if let Some(w) = self.window.clone() {
                        let _ = w.set_cursor_grab(CursorGrabMode::Confined);
                    }
                }
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
                let (win_w, win_h) = self.window_size;

                if self.control_active {
                    // at_edge: 에이전트 입력 필터 + 가장자리 리사이즈 트리거용 (grab은 항상 유지)
                    self.at_edge = ht_code_for_pos(x, y, win_w, win_h).is_some();
                    // 가장자리에서는 에이전트에 마우스 이동 미전달
                    if !self.at_edge {
                        self.send_pointer_move(x, y);
                    }
                    // 원격 커서 모양 미러링
                    apply_cursor_win32();
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

                // 제어 모드 + 가장자리: Left 클릭 → OS 리사이즈 트리거 (에이전트 미전달)
                if self.control_active && self.at_edge {
                    if button == MouseButton::Left && state == ElementState::Pressed {
                        let (win_w, win_h) = self.window_size;
                        if let (Some((cx, cy)), Some(w)) = (self.last_cursor, self.window.clone()) {
                            if let Some(ht) = ht_code_for_pos(cx, cy, win_w, win_h) {
                                let screen = w.inner_position().unwrap_or_default();
                                trigger_edge_resize(&w, ht, screen.x + cx, screen.y + cy);
                            }
                        }
                    }
                    return;
                }

                // 활성 상태에서만 에이전트로 전달
                if self.control_active {
                    if let Some(tx) = &self.input_tx {
                        let (btn, bit) = match button {
                            MouseButton::Left   => (0u32, 0b001u8),
                            MouseButton::Right  => (2u32, 0b010u8),
                            MouseButton::Middle => (4u32, 0b100u8),
                            _ => return,
                        };
                        let pressed = state == ElementState::Pressed;
                        if pressed { self.mouse_btns |= bit; } else { self.mouse_btns &= !bit; }
                        let _ = tx.send(InputEvent::MouseButton { button: btn, pressed });
                    }
                }
            }

            // ── 스크롤 휠 ───────────────────────────────────────────────────
            WindowEvent::MouseWheel { delta, .. } => {
                if is_agent_injected() { return; }

                let (raw_dx, raw_dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => {
                        ((x * 120.0) as i32, (y * 120.0) as i32)
                    }
                    MouseScrollDelta::PixelDelta(pos) => {
                        (pos.x as i32, pos.y as i32)
                    }
                };

                if self.control_active {
                    // 제어 모드: 원격 PC로 스크롤 전달
                    if let Some(tx) = &self.input_tx {
                        let _ = tx.send(InputEvent::Scroll {
                            dx: raw_dx as i16,
                            dy: raw_dy as i16,
                        });
                    }
                } else {
                    // 비제어 모드: 뷰포트 패닝
                    // dy 양수 = 위로 스크롤(=뷰 위로 이동), 음수 = 아래
                    let scroll_step = 80i32;
                    let step_x = if raw_dx != 0 { raw_dx.signum() * scroll_step } else { 0 };
                    let step_y = if raw_dy != 0 { raw_dy.signum() * scroll_step } else { 0 };

                    let max_sx = self.current_frame.as_ref()
                        .map_or(0, |f| f.width.saturating_sub(self.window_size.0));
                    let max_sy = self.current_frame.as_ref()
                        .map_or(0, |f| f.height.saturating_sub(self.window_size.1));

                    self.scroll_x = (self.scroll_x as i32 - step_x)
                        .clamp(0, max_sx as i32) as u32;
                    self.scroll_y = (self.scroll_y as i32 - step_y)
                        .clamp(0, max_sy as i32) as u32;

                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
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
            // 같은 PC 모드: 가상 화면 절대 좌표 — 스크롤 오프셋 불필요
            if let Some(w) = &self.window {
                if let Ok(origin) = w.inner_position() {
                    let gx = origin.x + x;
                    let gy = origin.y + y;
                    let _ = tx.send(InputEvent::MouseMoveGlobal { gx, gy });
                    return;
                }
            }
        }
        // 원격 PC 모드: 창 좌표 + 스크롤 오프셋 = 프레임 실제 좌표
        // 에이전트는 (fx, fy, frame_w, frame_h)로 screen 좌표를 계산:
        //   screen_x = fx * screen_w / frame_w
        // frame_w == screen_w 이므로 screen_x = fx = x + scroll_x (정확)
        let frame_w = self.current_frame.as_ref().map_or(self.window_size.0, |f| f.width);
        let frame_h = self.current_frame.as_ref().map_or(self.window_size.1, |f| f.height);
        let fx = (x + self.scroll_x as i32).clamp(0, frame_w as i32 - 1);
        let fy = (y + self.scroll_y as i32).clamp(0, frame_h as i32 - 1);
        let _ = tx.send(InputEvent::MouseMove {
            x:     fx,
            y:     fy,
            win_w: frame_w,
            win_h: frame_h,
        });
    }

    /// 원격 제어 모드 활성화
    fn activate_control_mode(&mut self) {
        // Arc 클론으로 window 참조를 분리해 release_all_inputs(&mut self) 호출 허용
        let Some(window) = self.window.clone() else { return };

        // 진입 시 에이전트 입력 상태 초기화 — 이전 세션에서 고착된 키/버튼이 있어도 해제
        self.release_all_inputs();

        // 커서를 창 안에 가둠 (항상 적용)
        self.at_edge = false;
        if let Err(e) = window.set_cursor_grab(CursorGrabMode::Confined) {
            hbb_common::log::warn!("[display] CursorGrab::Confined 실패: {:?}", e);
        }
        window.set_cursor_visible(true);
        apply_cursor_win32();
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
        let Some(window) = self.window.clone() else { return };

        // 훅 제거 전 모든 입력 강제 해제
        // — 수정 키·마우스 버튼을 누른 채 ESC 를 누르면 훅이 사라져 keyup/mouseup 이
        //   에이전트에 전달되지 않아 입력이 고착된다. 미리 해제 이벤트를 보낸다.
        self.release_all_inputs();
        self.remove_kb_hook();

        let _ = window.set_cursor_grab(CursorGrabMode::None);
        window.set_cursor(CursorIcon::Default);
        window.set_title("VDesk Viewer — 클릭하여 원격 제어");
        self.control_active = false;
        self.at_edge = false;
        window.request_redraw(); // 녹색 테두리 즉시 제거

        hbb_common::log::info!("[display] 원격 제어 모드 OFF");
    }

    /// 에이전트에 모든 수정 키·마우스 버튼 해제 이벤트 전송
    ///
    /// 제어 모드 진입/해제 양쪽에서 호출해 에이전트 입력 상태를 항상 클린하게 유지한다.
    fn release_all_inputs(&mut self) {
        let Some(tx) = &self.input_tx else { return };

        // ── 수정 키 해제 ────────────────────────────────────────────────────
        // (VK 코드, extended 여부)
        const MODS: &[(u32, bool)] = &[
            (0xA0, false), // VK_LSHIFT
            (0xA1, false), // VK_RSHIFT
            (0xA2, false), // VK_LCONTROL
            (0xA3, true),  // VK_RCONTROL  (extended)
            (0xA4, false), // VK_LMENU  (LAlt)
            (0xA5, true),  // VK_RMENU  (RAlt, extended)
            (0x5B, true),  // VK_LWIN   (extended)
            (0x5C, true),  // VK_RWIN   (extended)
        ];
        for &(vk, extended) in MODS {
            let _ = tx.send(InputEvent::KeyVk {
                vk,
                scan: 0,
                pressed: false,
                extended,
            });
        }

        // ── 마우스 버튼 해제 (실제로 눌린 버튼만) ──────────────────────────
        // 무조건 UP을 보내면 remote에서 우클릭 등 오발이 발생하므로 추적된 것만 해제
        const BTN_MAP: &[(u8, u32)] = &[(0b001, 0), (0b010, 2), (0b100, 4)];
        for &(bit, btn) in BTN_MAP {
            if self.mouse_btns & bit != 0 {
                let _ = tx.send(InputEvent::MouseButton { button: btn, pressed: false });
            }
        }
        self.mouse_btns = 0;

        hbb_common::log::debug!("[display] 모든 입력 강제 해제 전송");
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

        // 스크롤 범위 클램프 (창 크기/프레임 크기 변동 대응)
        let max_sx = frame.width.saturating_sub(win_w);
        let max_sy = frame.height.saturating_sub(win_h);
        self.scroll_x = self.scroll_x.min(max_sx);
        self.scroll_y = self.scroll_y.min(max_sy);

        // 프레임이 창보다 작을 때 중앙 정렬 오프셋
        let off_x = if frame.width  < win_w { (win_w - frame.width)  / 2 } else { 0 };
        let off_y = if frame.height < win_h { (win_h - frame.height) / 2 } else { 0 };

        if let Ok(mut buf) = surface.buffer_mut() {
            // 1:1 픽셀 복사 (스케일링 없음) — 창보다 큰 영역은 스크롤로 패닝
            blit_frame(
                &frame.pixels, frame.width, frame.height,
                &mut buf,      win_w,       win_h,
                self.scroll_x, self.scroll_y,
                off_x,         off_y,
            );

            // 스크롤바 표시 (스크롤이 필요한 경우)
            if frame.width > win_w || frame.height > win_h {
                draw_scrollbars(
                    &mut buf, win_w, win_h,
                    frame.width, frame.height,
                    self.scroll_x, self.scroll_y,
                );
            }

            // 원격 제어 모드 활성 시 녹색 테두리 오버레이
            if self.control_active {
                draw_control_border(&mut buf, win_w, win_h);
            }

            let _ = buf.present();
        }
    }
}

/// 프레임을 1:1 픽셀 비율로 복사 (스케일링 없음)
///
/// - 창이 프레임보다 클 때: `off_x / off_y` 만큼 중앙 정렬, 나머지는 검정
/// - 창이 프레임보다 작을 때: `scroll_x / scroll_y` 위치부터 창 크기만큼 표시
fn blit_frame(
    src: &[u32], src_w: u32, src_h: u32,
    dst: &mut [u32], dst_w: u32, dst_h: u32,
    scroll_x: u32, scroll_y: u32,
    off_x: u32, off_y: u32,
) {
    if src_w == 0 || src_h == 0 { return; }

    let dst_w_us = dst_w as usize;

    for dy in 0..dst_h {
        let dst_row = &mut dst[dy as usize * dst_w_us..(dy as usize + 1) * dst_w_us];

        // 위/아래 여백 (프레임이 창보다 작아 중앙 정렬된 경우)
        if dy < off_y || dy >= off_y + src_h {
            dst_row.fill(0);
            continue;
        }

        let fy = (dy - off_y + scroll_y) as usize;
        if fy >= src_h as usize {
            dst_row.fill(0);
            continue;
        }

        // 왼쪽 여백
        let left = off_x as usize;
        if left > 0 {
            dst_row[..left.min(dst_w_us)].fill(0);
        }

        // 프레임 픽셀 복사
        let fx_start = scroll_x as usize;
        let frame_avail = src_w as usize - fx_start;      // 프레임에서 꺼낼 수 있는 너비
        let dst_avail   = dst_w_us.saturating_sub(left);  // 목적지 남은 너비
        let copy_w      = frame_avail.min(dst_avail);

        if copy_w > 0 {
            let src_off = fy * src_w as usize + fx_start;
            dst_row[left..left + copy_w].copy_from_slice(&src[src_off..src_off + copy_w]);
        }

        // 오른쪽 여백
        let right_start = left + copy_w;
        if right_start < dst_w_us {
            dst_row[right_start..].fill(0);
        }
    }
}

/// 스크롤바 오버레이 (반투명 효과 없이 단색)
///
/// - 수직 스크롤바: 우측 8px
/// - 수평 스크롤바: 하단 8px
/// - 썸(thumb): 밝은 회색, 트랙: 어두운 회색
fn draw_scrollbars(
    buf: &mut softbuffer::Buffer<'_, Arc<Window>, Arc<Window>>,
    win_w: u32, win_h: u32,
    frame_w: u32, frame_h: u32,
    scroll_x: u32, scroll_y: u32,
) {
    const BAR:   u32 = 8;          // 스크롤바 두께 (px)
    const THUMB: u32 = 0x00AAAAAA; // 썸 색상 (밝은 회색, XRGB)
    const TRACK: u32 = 0x00333333; // 트랙 색상 (어두운 회색)

    let need_h = frame_w > win_w;
    let need_v = frame_h > win_h;

    // 수직 스크롤바 (우측)
    if need_v {
        let track_h = if need_h { win_h.saturating_sub(BAR) } else { win_h };
        let max_sy  = frame_h - win_h;
        let thumb_h = ((track_h as u64 * win_h as u64 / frame_h as u64) as u32).max(20).min(track_h);
        let thumb_y = (scroll_y as u64 * (track_h - thumb_h) as u64 / max_sy as u64) as u32;

        let bar_x = win_w.saturating_sub(BAR);
        for y in 0..track_h {
            let color = if y >= thumb_y && y < thumb_y + thumb_h { THUMB } else { TRACK };
            for x in bar_x..win_w {
                let idx = (y * win_w + x) as usize;
                if idx < buf.len() { buf[idx] = color; }
            }
        }
    }

    // 수평 스크롤바 (하단)
    if need_h {
        let track_w = if need_v { win_w.saturating_sub(BAR) } else { win_w };
        let max_sx  = frame_w - win_w;
        let thumb_w = ((track_w as u64 * win_w as u64 / frame_w as u64) as u32).max(20).min(track_w);
        let thumb_x = (scroll_x as u64 * (track_w - thumb_w) as u64 / max_sx as u64) as u32;

        let bar_y = win_h.saturating_sub(BAR);
        for y in bar_y..win_h {
            for x in 0..track_w {
                let color = if x >= thumb_x && x < thumb_x + thumb_w { THUMB } else { TRACK };
                let idx = (y * win_w + x) as usize;
                if idx < buf.len() { buf[idx] = color; }
            }
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

/// 커서 타입 코드(0~9) → Windows IDC 커서 상수
/// 0=Arrow 1=IBeam 2=SizeWE 3=SizeNS 4=SizeNWSE 5=SizeNESW 6=SizeAll 7=Hand 8=Wait 9=No
fn cursor_type_to_idc(ty: u8) -> usize {
    match ty {
        1 => 32513, // IDC_IBEAM
        2 => 32644, // IDC_SIZEWE
        3 => 32645, // IDC_SIZENS
        4 => 32642, // IDC_SIZENWSE
        5 => 32643, // IDC_SIZENESW
        6 => 32646, // IDC_SIZEALL
        7 => 32649, // IDC_HAND
        8 => 32514, // IDC_WAIT
        9 => 32648, // IDC_NO
        _ => 32512, // IDC_ARROW
    }
}

/// Win32 SetCursor를 직접 호출해 원격 커서 모양 미러링
/// winit의 set_cursor는 set_cursor_grab 이후 WM_SETCURSOR에 의해 재설정될 수 있으므로
/// 직접 Win32 API를 통해 확실하게 적용한다.
#[cfg(target_os = "windows")]
fn apply_cursor_win32() {
    extern "system" {
        fn SetCursor(cursor: isize) -> isize;
        fn LoadCursorW(instance: isize, cursor_name: usize) -> isize;
    }
    let ty = REMOTE_CURSOR_TYPE.load(std::sync::atomic::Ordering::Relaxed);
    unsafe { SetCursor(LoadCursorW(0, cursor_type_to_idc(ty))); }
}
#[cfg(not(target_os = "windows"))]
fn apply_cursor_win32() {}

/// 커서 위치 → Windows HT(hit-test) 코드 반환 (가장자리면 Some, 내부면 None)
fn ht_code_for_pos(x: i32, y: i32, win_w: u32, win_h: u32) -> Option<usize> {
    const EDGE: i32 = 12;
    let at_left   = x < EDGE;
    let at_right  = x >= win_w as i32 - EDGE;
    let at_top    = y < EDGE;
    let at_bottom = y >= win_h as i32 - EDGE;
    match (at_top, at_bottom, at_left, at_right) {
        (true,  _,     true,  _    ) => Some(13), // HTTOPLEFT
        (true,  _,     _,     true ) => Some(14), // HTTOPRIGHT
        (_,     true,  true,  _    ) => Some(16), // HTBOTTOMLEFT
        (_,     true,  _,     true ) => Some(17), // HTBOTTOMRIGHT
        (true,  _,     _,     _    ) => Some(12), // HTTOP
        (_,     true,  _,     _    ) => Some(15), // HTBOTTOM
        (_,     _,     true,  _    ) => Some(10), // HTLEFT
        (_,     _,     _,     true ) => Some(11), // HTRIGHT
        _                            => None,
    }
}

/// Windows: 가장자리 클릭 시 OS 리사이즈 루프 트리거
/// ReleaseCapture + WM_NCLBUTTONDOWN 으로 OS가 리사이즈 핸들을 처리하도록 위임
#[cfg(target_os = "windows")]
fn trigger_edge_resize(window: &Arc<Window>, ht: usize, screen_x: i32, screen_y: i32) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    extern "system" {
        fn ReleaseCapture() -> i32;
        fn ClipCursor(rect: *const u8) -> i32;
        fn PostMessageA(hwnd: isize, msg: u32, w: usize, l: isize) -> isize;
    }
    const WM_NCLBUTTONDOWN: u32 = 0x00A1;
    let Ok(handle) = window.window_handle() else { return };
    let RawWindowHandle::Win32(h) = handle.as_raw() else { return };
    let hwnd = h.hwnd.get();
    let lparam = (((screen_y as i32 as isize) << 16) | (screen_x as u16 as isize)) as isize;
    unsafe {
        ReleaseCapture();
        ClipCursor(std::ptr::null()); // grab 해제 → OS 리사이즈 루프가 자유롭게 커서 이동
        PostMessageA(hwnd, WM_NCLBUTTONDOWN, ht, lparam);
    }
}
#[cfg(not(target_os = "windows"))]
fn trigger_edge_resize(_window: &Arc<Window>, _ht: usize, _sx: i32, _sy: i32) {}


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
        scroll_x:        0,
        scroll_y:        0,
        mouse_btns:      0,
        at_edge:         false,
    };

    // Wait: 프레임/입력 이벤트가 있을 때만 루프 실행
    event_loop.set_control_flow(ControlFlow::Wait);
    event_loop.run_app(&mut app)?;
    Ok(())
}
