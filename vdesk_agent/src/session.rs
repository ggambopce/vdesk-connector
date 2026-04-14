//! 원격 세션 루프 — 화면 스트리밍 + 입력 수신
//!
//! ── 에이전트 → 뷰어 메시지 ────────────────────────────────────────────────────
//!   0x10 Init:  [width(4BE), height(4BE), fps(1), codec(1)]  세션 시작 시 1회
//!               codec: 1=VP9
//!   0x11 Frame: [is_key(1), data_len(4BE), vp9_data]         프레임마다
//!   0x12 Pong:  [timestamp(8BE)]                             Ping 응답
//!
//! ── 뷰어 → 에이전트 입력 메시지 ──────────────────────────────────────────────
//!   0x01 MouseMove    0x02 MouseButton  0x03 KeyPress
//!   0x04 Scroll       0x06 Ping
//!   0x07 MouseGlobal  0x08 KeyVk

use anyhow::Result;
use bytes::{BufMut, Bytes, BytesMut};
use hbb_common::{log, tcp::FramedStream};
use tokio::sync::mpsc;

use crate::services::video::{self, VideoFrame};

// ── 에이전트 → 뷰어 타입 ─────────────────────────────────────────────────────
const MSG_INIT:   u8 = 0x10;
const MSG_FRAME:  u8 = 0x11;
const MSG_PONG:   u8 = 0x12;
const MSG_CURSOR: u8 = 0x13; // [cursor_type(1)]

// ── 뷰어 → 에이전트 타입 ─────────────────────────────────────────────────────
const IN_MOUSE_MOVE:   u8 = 0x01;
const IN_MOUSE_BUTTON: u8 = 0x02;
const IN_KEY_PRESS:    u8 = 0x03;
const IN_SCROLL:       u8 = 0x04;
const IN_PING:         u8 = 0x06;
const IN_MOUSE_GLOBAL: u8 = 0x07;
const IN_KEY_VK:       u8 = 0x08;

pub async fn run(mut stream: FramedStream, session_key: String, device_key: String) -> Result<()> {
    log::info!("[session] 세션 시작: {}", session_key);

    // 비디오 캡처 채널
    let (video_tx, mut video_rx) = mpsc::channel::<VideoFrame>(2);
    // 아웃바운드 제어 메시지 (Pong, CursorShape)
    let (out_tx, mut out_rx) = mpsc::channel::<Bytes>(16);

    // 세션 heartbeat: 15초 간격으로 서버에 보고
    let mut hb_tick = tokio::time::interval(std::time::Duration::from_secs(15));
    hb_tick.tick().await; // 첫 틱 즉시 소모 (시작 직후 호출 방지)
    let mut bytes_out_total: u64 = 0;
    let mut bytes_in_total: u64 = 0;

    // 커서 폴링: 50ms 간격으로 원격 커서 모양 감지 후 전송
    let mut cursor_tick = tokio::time::interval(std::time::Duration::from_millis(50));
    cursor_tick.tick().await;
    let mut last_cursor_type: u8 = 0xFF; // 첫 전송 강제

    // 화면 캡처 태스크 — JoinHandle 보관하여 종료 대기에 사용
    let capture_handle = tokio::task::spawn_blocking({
        let tx = video_tx.clone();
        let key = session_key.clone();
        move || {
            if let Err(e) = video::capture_loop(tx, key) {
                log::error!("[video] 캡처 오류: {:?}", e);
            }
        }
    });

    let hb_device_key = device_key.clone();
    let hb_session_key = session_key.clone();

    // video_tx 원본 drop — capture task의 clone이 유일한 sender가 됨
    // → capture task가 실패하면 recv()가 None을 반환해 영구 블록 방지
    drop(video_tx);

    // 첫 프레임으로 화면 크기 파악 → Init 메시지 전송
    let first_frame = match video_rx.recv().await {
        Some(f) => f,
        None => {
            let _ = capture_handle.await;
            anyhow::bail!("[session] 첫 프레임 수신 실패 (캡처 태스크 종료)")
        }
    };
    send_init(&mut stream, &first_frame).await?;
    send_frame(&mut stream, &first_frame).await?;

    // ── 메인 세션 루프 ────────────────────────────────────────────────────────
    loop {
        tokio::select! {
            recv = stream.next() => {
                match recv {
                    Some(Ok(b))  => handle_input(&b, &out_tx),
                    Some(Err(e)) => { log::warn!("[session] 수신 오류: {:?}", e); break; }
                    None         => { log::info!("[session] 뷰어 연결 종료"); break; }
                }
            }
            Some(frame) = video_rx.recv() => {
                if let Err(e) = send_frame(&mut stream, &frame).await {
                    log::warn!("[session] 프레임 전송 오류: {:?}", e);
                    break;
                }
            }
            Some(msg) = out_rx.recv() => {
                if let Err(e) = stream.send_bytes(msg).await {
                    log::warn!("[session] 제어 메시지 전송 오류: {:?}", e);
                    break;
                }
            }
            _ = cursor_tick.tick() => {
                let ct = get_cursor_type();
                if ct != last_cursor_type {
                    last_cursor_type = ct;
                    let msg = Bytes::from(vec![MSG_CURSOR, ct]);
                    if let Err(e) = stream.send_bytes(msg).await {
                        log::warn!("[session] 커서 전송 오류: {:?}", e);
                        break;
                    }
                }
            }
            _ = hb_tick.tick() => {
                let req = crate::api::SessionHeartbeatRequest {
                    device_key: hb_device_key.clone(),
                    session_key: hb_session_key.clone(),
                    bytes_out: bytes_out_total,
                    bytes_in: bytes_in_total,
                };
                if let Err(e) = crate::api::session_heartbeat(&req).await {
                    log::warn!("[session] 세션 heartbeat 실패: {:?}", e);
                } else {
                    log::debug!("[session] 세션 heartbeat OK (out={}, in={})", bytes_out_total, bytes_in_total);
                }
            }
        }
    }

    // video_rx drop → capture_loop가 tx.is_closed()/TrySendError::Closed 감지 후 종료
    // DXGI 핸들이 해제될 때까지 대기 — 재연결 시 새 DuplicateOutput 충돌 방지
    drop(video_rx);
    let _ = capture_handle.await;
    log::info!("[session] 캡처 루프 종료 확인 — DXGI 핸들 해제됨");

    log::info!("[session] 세션 종료: {}", session_key);
    Ok(())
}

// ── 커서 타입 (에이전트 → 뷰어 공통 정의) ────────────────────────────────────
// 0=Arrow 1=IBeam 2=SizeWE 3=SizeNS 4=SizeNWSE 5=SizeNESW 6=SizeAll 7=Hand 8=Wait 9=No

/// 현재 시스템 커서를 감지해 커서 타입(0~9)으로 반환
#[cfg(target_os = "windows")]
fn get_cursor_type() -> u8 {
    #[repr(C)]
    struct CURSORINFO { cb: u32, flags: u32, hcursor: isize, pt_x: i32, pt_y: i32 }
    extern "system" {
        fn GetCursorInfo(pci: *mut CURSORINFO) -> i32;
        fn LoadCursorW(instance: isize, name: usize) -> isize;
    }
    let mut ci = CURSORINFO { cb: std::mem::size_of::<CURSORINFO>() as u32,
                              flags: 0, hcursor: 0, pt_x: 0, pt_y: 0 };
    if unsafe { GetCursorInfo(&mut ci) } == 0 { return 0; }
    let h = ci.hcursor;
    // 시스템 커서 ID
    const IDC_ARROW:    usize = 32512;
    const IDC_IBEAM:    usize = 32513;
    const IDC_WAIT:     usize = 32514;
    const IDC_SIZENWSE: usize = 32642;
    const IDC_SIZENESW: usize = 32643;
    const IDC_SIZEWE:   usize = 32644;
    const IDC_SIZENS:   usize = 32645;
    const IDC_SIZEALL:  usize = 32646;
    const IDC_NO:       usize = 32648;
    const IDC_HAND:     usize = 32649;
    let ids: &[(usize, u8)] = &[
        (IDC_IBEAM,    1),
        (IDC_SIZEWE,   2),
        (IDC_SIZENS,   3),
        (IDC_SIZENWSE, 4),
        (IDC_SIZENESW, 5),
        (IDC_SIZEALL,  6),
        (IDC_HAND,     7),
        (IDC_WAIT,     8),
        (IDC_NO,       9),
        (IDC_ARROW,    0),
    ];
    for &(id, ty) in ids {
        if unsafe { LoadCursorW(0, id) } == h { return ty; }
    }
    0 // 알 수 없으면 Arrow
}
#[cfg(not(target_os = "windows"))]
fn get_cursor_type() -> u8 { 0 }

// ── 메시지 빌더 ──────────────────────────────────────────────────────────────

async fn send_init(stream: &mut FramedStream, frame: &VideoFrame) -> Result<()> {
    let mut buf = BytesMut::with_capacity(11);
    buf.put_u8(MSG_INIT);
    buf.put_u32(frame.width);
    buf.put_u32(frame.height);
    buf.put_u8(frame.fps);
    buf.put_u8(frame.codec as u8); // 코덱 타입 (1=VP9)
    stream.send_bytes(buf.freeze()).await?;
    Ok(())
}

async fn send_frame(stream: &mut FramedStream, frame: &VideoFrame) -> Result<()> {
    // [is_key(1), data_len(4BE), vp9_data]
    let mut buf = BytesMut::with_capacity(6 + frame.data.len());
    buf.put_u8(MSG_FRAME);
    buf.put_u8(frame.is_key as u8);
    buf.put_u32(frame.data.len() as u32);
    buf.extend_from_slice(&frame.data);
    stream.send_bytes(buf.freeze()).await?;
    Ok(())
}

// ── 입력 파서 ────────────────────────────────────────────────────────────────

fn handle_input(bytes: &[u8], out_tx: &mpsc::Sender<Bytes>) {
    if bytes.is_empty() { return; }
    use crate::services::input as inp;

    match bytes[0] {
        IN_MOUSE_MOVE if bytes.len() >= 13 => {
            let x     = i32::from_be_bytes(bytes[1..5].try_into().unwrap());
            let y     = i32::from_be_bytes(bytes[5..9].try_into().unwrap());
            let win_w = u16::from_be_bytes(bytes[9..11].try_into().unwrap()) as i32;
            let win_h = u16::from_be_bytes(bytes[11..13].try_into().unwrap()) as i32;
            inp::inject_mouse_move(x, y, win_w, win_h);
        }
        IN_MOUSE_GLOBAL if bytes.len() >= 9 => {
            let gx = i32::from_be_bytes(bytes[1..5].try_into().unwrap());
            let gy = i32::from_be_bytes(bytes[5..9].try_into().unwrap());
            inp::inject_mouse_move_global(gx, gy);
        }
        IN_MOUSE_BUTTON if bytes.len() >= 3 => {
            inp::inject_mouse_button(bytes[1], bytes[2] != 0);
        }
        IN_KEY_PRESS if bytes.len() >= 6 => {
            let key = u32::from_be_bytes(bytes[1..5].try_into().unwrap());
            inp::inject_key(key, bytes[5] != 0);
        }
        IN_SCROLL if bytes.len() >= 5 => {
            let dx = i16::from_be_bytes(bytes[1..3].try_into().unwrap());
            let dy = i16::from_be_bytes(bytes[3..5].try_into().unwrap());
            inp::inject_scroll(dx, dy);
        }
        IN_KEY_VK if bytes.len() >= 8 => {
            let vk       = u32::from_be_bytes(bytes[1..5].try_into().unwrap());
            let scan     = u16::from_be_bytes(bytes[5..7].try_into().unwrap());
            let pressed  = bytes[7] != 0;
            let extended = bytes.get(8).copied().unwrap_or(0) != 0;
            inp::inject_key_vk(vk, scan, pressed, extended);
        }
        IN_PING if bytes.len() >= 9 => {
            let mut buf = BytesMut::with_capacity(9);
            buf.put_u8(MSG_PONG);
            buf.extend_from_slice(&bytes[1..9]);
            let _ = out_tx.try_send(buf.freeze());
        }
        _ => {}
    }
}
