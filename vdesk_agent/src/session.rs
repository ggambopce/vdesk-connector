//! 원격 세션 루프 — 화면 스트리밍 + 입력 수신
//!
//! ── 에이전트 → 뷰어 메시지 ────────────────────────────────────────────────────
//!   0x10 Init:  [width(4BE), height(4BE), fps(1)]       세션 시작 시 1회
//!   0x11 Frame: [jpeg_len(4BE), jpeg_data]              프레임마다
//!   0x12 Pong:  [timestamp(8BE)]                        Ping 응답
//!
//! ── 뷰어 → 에이전트 입력 메시지 ──────────────────────────────────────────────
//!   0x01 MouseMove:    [x(4BE), y(4BE), win_w(2BE), win_h(2BE)]
//!   0x02 MouseButton:  [button(1), pressed(1)]
//!   0x03 KeyPress:     [keycode(4BE), pressed(1)]
//!   0x04 Scroll:       [dx(2BE), dy(2BE)]
//!   0x05 CharInput:    [len(2BE), utf8_bytes]
//!   0x06 Ping:         [timestamp(8BE)]

use anyhow::Result;
use bytes::{BufMut, Bytes, BytesMut};
use hbb_common::{log, tcp::FramedStream};
use tokio::sync::mpsc;

use crate::services::video::{self, VideoFrame};

// ── 에이전트 → 뷰어 타입 ─────────────────────────────────────────────────────
const MSG_INIT:  u8 = 0x10;
const MSG_FRAME: u8 = 0x11;
const MSG_PONG:  u8 = 0x12;

// ── 뷰어 → 에이전트 타입 ─────────────────────────────────────────────────────
const IN_MOUSE_MOVE:   u8 = 0x01;
const IN_MOUSE_BUTTON: u8 = 0x02;
const IN_KEY_PRESS:    u8 = 0x03;
const IN_SCROLL:       u8 = 0x04;
const IN_CHAR_INPUT:   u8 = 0x05;
const IN_PING:         u8 = 0x06;

pub async fn run(mut stream: FramedStream, session_key: String) -> Result<()> {
    log::info!("[session] 세션 시작: {}", session_key);

    // 비디오 캡처 채널 (캡처 태스크 → 전송 루프)
    let (video_tx, mut video_rx) = mpsc::channel::<VideoFrame>(4);
    // 아웃바운드 제어 메시지 채널 (Pong 등)
    let (out_tx, mut out_rx) = mpsc::channel::<Bytes>(16);

    // 화면 캡처 태스크 (블로킹 → spawn_blocking)
    tokio::task::spawn_blocking({
        let tx = video_tx.clone();
        let key = session_key.clone();
        move || {
            if let Err(e) = video::capture_loop(tx, key) {
                log::error!("[video] 캡처 오류: {:?}", e);
            }
        }
    });

    // 첫 프레임으로 화면 크기 파악 → Init 메시지 전송
    let first_frame = match video_rx.recv().await {
        Some(f) => f,
        None => anyhow::bail!("[session] 첫 프레임 수신 실패"),
    };
    send_init(&mut stream, &first_frame).await?;
    send_frame(&mut stream, &first_frame).await?;

    // ── 메인 세션 루프 ────────────────────────────────────────────────────────
    loop {
        tokio::select! {
            // 뷰어 → 에이전트 입력 수신
            recv = stream.next() => {
                match recv {
                    Some(Ok(b))  => handle_input(&b, &out_tx),
                    Some(Err(e)) => { log::warn!("[session] 수신 오류: {:?}", e); break; }
                    None         => { log::info!("[session] 뷰어 연결 종료"); break; }
                }
            }
            // 화면 프레임 → 뷰어 전송
            Some(frame) = video_rx.recv() => {
                if let Err(e) = send_frame(&mut stream, &frame).await {
                    log::warn!("[session] 프레임 전송 오류: {:?}", e);
                    break;
                }
            }
            // Pong 등 아웃바운드 제어 메시지
            Some(msg) = out_rx.recv() => {
                if let Err(e) = stream.send_bytes(msg).await {
                    log::warn!("[session] 제어 메시지 전송 오류: {:?}", e);
                    break;
                }
            }
        }
    }

    log::info!("[session] 세션 종료: {}", session_key);
    Ok(())
}

// ── 메시지 빌더 ──────────────────────────────────────────────────────────────

async fn send_init(stream: &mut FramedStream, frame: &VideoFrame) -> Result<()> {
    let mut buf = BytesMut::with_capacity(10);
    buf.put_u8(MSG_INIT);
    buf.put_u32(frame.width);
    buf.put_u32(frame.height);
    buf.put_u8(frame.fps);
    stream.send_bytes(buf.freeze()).await?;
    Ok(())
}

async fn send_frame(stream: &mut FramedStream, frame: &VideoFrame) -> Result<()> {
    let mut buf = BytesMut::with_capacity(5 + frame.jpeg.len());
    buf.put_u8(MSG_FRAME);
    buf.put_u32(frame.jpeg.len() as u32);
    buf.extend_from_slice(&frame.jpeg);
    stream.send_bytes(buf.freeze()).await?;
    Ok(())
}

// ── 입력 파서 ────────────────────────────────────────────────────────────────

fn handle_input(bytes: &[u8], out_tx: &mpsc::Sender<Bytes>) {
    if bytes.is_empty() {
        return;
    }
    use crate::services::input as inp;

    match bytes[0] {
        // 마우스 이동: 뷰어 창 좌표 + 창 크기 → 에이전트가 스케일 계산
        IN_MOUSE_MOVE if bytes.len() >= 13 => {
            let x     = i32::from_be_bytes(bytes[1..5].try_into().unwrap());
            let y     = i32::from_be_bytes(bytes[5..9].try_into().unwrap());
            let win_w = u16::from_be_bytes(bytes[9..11].try_into().unwrap()) as i32;
            let win_h = u16::from_be_bytes(bytes[11..13].try_into().unwrap()) as i32;
            inp::inject_mouse_move(x, y, win_w, win_h);
        }
        // 마우스 버튼: button(0=Left,2=Right,4=Middle), pressed(0/1)
        IN_MOUSE_BUTTON if bytes.len() >= 3 => {
            inp::inject_mouse_button(bytes[1], bytes[2] != 0);
        }
        // 키보드: winit PhysicalKey discriminant, pressed(0/1)
        IN_KEY_PRESS if bytes.len() >= 6 => {
            let key = u32::from_be_bytes(bytes[1..5].try_into().unwrap());
            inp::inject_key(key, bytes[5] != 0);
        }
        // 스크롤 휠: dx/dy (i16, Windows 단위 120=한 칸)
        IN_SCROLL if bytes.len() >= 5 => {
            let dx = i16::from_be_bytes(bytes[1..3].try_into().unwrap());
            let dy = i16::from_be_bytes(bytes[3..5].try_into().unwrap());
            inp::inject_scroll(dx, dy);
        }
        // 유니코드 문자 입력 (한글/IME Commit)
        IN_CHAR_INPUT if bytes.len() >= 3 => {
            let len = u16::from_be_bytes(bytes[1..3].try_into().unwrap()) as usize;
            if bytes.len() >= 3 + len {
                if let Ok(text) = std::str::from_utf8(&bytes[3..3 + len]) {
                    inp::inject_char(text);
                }
            }
        }
        // Ping → Pong (timestamp 그대로 반사)
        IN_PING if bytes.len() >= 9 => {
            let mut buf = BytesMut::with_capacity(9);
            buf.put_u8(MSG_PONG);
            buf.extend_from_slice(&bytes[1..9]);
            let _ = out_tx.try_send(buf.freeze());
        }
        _ => {}
    }
}
