//! 원격 화면 렌더링 — winit + softbuffer
//!
//! 원격 제어 모드:
//!   - 뷰어 창 클릭 → 원격 제어 모드 ON (클릭 자체는 에이전트 미전달)
//!   - Escape 키 / 창 포커스 이탈 → 원격 제어 모드 OFF
//!   - 활성화 시 CursorMoved 절대 좌표를 에이전트로 전달 (커서 자유 이동)
//!   - 비활성화 상태에서는 마우스/키보드 입력을 에이전트로 전달하지 않음
//!
//! 단축키:
//!   F11  — 전체화면 토글 (항상 로컬 처리)
//!   Escape — 원격 제어 모드 해제
//!
//! 같은 PC 테스트 시 화면 중첩 방지:
//!   Windows WDA_EXCLUDEFROMCAPTURE 적용 → 에이전트 캡처 시 뷰어 창이 검게 처리됨

use anyhow::Result;
use softbuffer::Surface;
use std::{num::NonZeroU32, sync::Arc};
use winit::{
    application::ApplicationHandler,
    event::{
        DeviceEvent, DeviceId, ElementState, Ime, KeyEvent as WinitKeyEvent,
        MouseButton, MouseScrollDelta, WindowEvent,
    },
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    keyboard::{KeyCode, PhysicalKey},
    window::{CursorIcon, Fullscreen, Window, WindowAttributes, WindowId},
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
    /// 마우스 이동 — 뷰어 창 좌표 + 현재 창 크기
    MouseMove { x: i32, y: i32, win_w: u32, win_h: u32 },
    /// 마우스 버튼 (button: 0=Left, 2=Right, 4=Middle)
    MouseButton { button: u32, pressed: bool },
    /// 물리 키 (winit KeyCode discriminant)
    KeyPress { key: u32, pressed: bool },
    /// 스크롤 휠 (Windows notch 단위: 한 칸 = 120)
    Scroll { dx: i16, dy: i16 },
    /// IME 완성 문자 (한글·CJK) 또는 유니코드 문자
    CharInput { text: String },
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
}

impl ApplicationHandler<ViewerEvent> for ViewerApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_none() {
            let attrs = WindowAttributes::default()
                .with_title("VDesk Viewer — 클릭하여 원격 제어")
                .with_maximized(true);
            let window = Arc::new(event_loop.create_window(attrs).unwrap());

            // 한글·IME 입력 활성화 (비활성화 상태의 기본값)
            window.set_ime_allowed(true);

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
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => self.redraw(),

            // 창 크기 변경: surface 재조정 + 리드로우
            WindowEvent::Resized(size) => {
                self.window_size = (size.width.max(1), size.height.max(1));
                self.surface_size = (0, 0); // 캐시 무효화
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }

            // ── 포커스 이탈 → 원격 제어 자동 해제 ──────────────────────────
            WindowEvent::Focused(false) => {
                if self.control_active {
                    self.deactivate_control_mode();
                }
            }

            // ── 마우스 이동 ─────────────────────────────────────────────────
            WindowEvent::CursorMoved { position, .. } => {
                if self.control_active {
                    if let Some(tx) = &self.input_tx {
                        let (ww, wh) = self.window_size;
                        let _ = tx.send(InputEvent::MouseMove {
                            x:     position.x as i32,
                            y:     position.y as i32,
                            win_w: ww,
                            win_h: wh,
                        });
                    }
                }
            }

            // ── 마우스 버튼 ─────────────────────────────────────────────────
            WindowEvent::MouseInput { state, button, .. } => {
                // 비활성 상태에서 Left 클릭 → 원격 제어 모드 진입 (클릭 자체는 전달 안 함)
                if !self.control_active
                    && button == MouseButton::Left
                    && state == ElementState::Pressed
                {
                    self.activate_control_mode();
                    return;
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

            // ── IME 완성 문자 (한글·CJK) ────────────────────────────────────
            WindowEvent::Ime(Ime::Commit(text)) => {
                if self.control_active {
                    if let Some(tx) = &self.input_tx {
                        if !text.is_empty() {
                            let _ = tx.send(InputEvent::CharInput { text });
                        }
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
        // 커서 grab 없이 CursorMoved에서 절대 좌표를 수신하므로 여기서는 처리하지 않음
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
    /// 원격 제어 모드 활성화
    fn activate_control_mode(&mut self) {
        let Some(window) = &self.window else { return };

        window.set_cursor(CursorIcon::Crosshair);
        window.set_ime_allowed(false);
        window.set_title("VDesk Viewer [원격 제어 중] — ESC로 해제");
        self.control_active = true;

        hbb_common::log::info!("[display] 원격 제어 모드 ON");
    }

    /// 원격 제어 모드 비활성화
    fn deactivate_control_mode(&mut self) {
        let Some(window) = &self.window else { return };

        window.set_cursor(CursorIcon::Default);
        window.set_ime_allowed(true);
        window.set_title("VDesk Viewer — 클릭하여 원격 제어");
        self.control_active = false;

        hbb_common::log::info!("[display] 원격 제어 모드 OFF");
    }

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

        if let Ok(mut buf) = surface.buffer_mut() {
            // 프레임을 창 크기에 맞게 스케일링
            scale_pixels(
                &frame.pixels, frame.width, frame.height,
                &mut buf,      win_w,       win_h,
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
fn scale_pixels(
    src: &[u32], src_w: u32, src_h: u32,
    dst: &mut [u32], dst_w: u32, dst_h: u32,
) {
    if src_w == 0 || src_h == 0 {
        return;
    }
    for dy in 0..dst_h {
        let sy = dy * src_h / dst_h;
        let src_row = sy * src_w;
        let dst_row = dy * dst_w;
        for dx in 0..dst_w {
            let sx = dx * src_w / dst_w;
            dst[(dst_row + dx) as usize] = src[(src_row + sx) as usize];
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

/// winit 이벤트 루프 실행 (반드시 메인 스레드에서 호출)
pub fn run_event_loop(
    frame_rx: std::sync::mpsc::Receiver<FrameBuffer>,
    input_tx: Option<tokio::sync::mpsc::UnboundedSender<InputEvent>>,
) -> Result<()> {
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
    };

    // Wait: 프레임/입력 이벤트가 있을 때만 루프 실행
    event_loop.set_control_flow(ControlFlow::Wait);
    event_loop.run_app(&mut app)?;
    Ok(())
}
