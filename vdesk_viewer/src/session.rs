//! Viewer 세션 루프
//!
//! ── 수신 (에이전트 → 뷰어) ────────────────────────────────────────────────────
//!   0x10 Init:  [width(4BE), height(4BE), fps(1)]   첫 연결 시 화면 정보
//!   0x11 Frame: [jpeg_len(4BE), jpeg_data]           프레임
//!   0x12 Pong:  [timestamp(8BE)]                     RTT 측정 응답
//!
//! ── 송신 (뷰어 → 에이전트) ───────────────────────────────────────────────────
//!   0x01 MouseMove:   [x(4BE), y(4BE), win_w(2BE), win_h(2BE)]
//!   0x02 MouseButton: [button(1), pressed(1)]
//!   0x03 KeyPress:    [keycode(4BE), pressed(1)]
//!   0x04 Scroll:      [dx(2BE), dy(2BE)]
//!   0x05 CharInput:   [len(2BE), utf8_bytes]
//!   0x06 Ping:        [timestamp(8BE)]

use anyhow::Result;
use hbb_common::{log, tcp::FramedStream};
use std::{
    sync::mpsc,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{
    decoder,
    display::{FrameBuffer, InputEvent},
};

// ── 타입 상수 ────────────────────────────────────────────────────────────────
const MSG_INIT:  u8 = 0x10;
const MSG_FRAME: u8 = 0x11;
const MSG_PONG:  u8 = 0x12;

const IN_MOUSE_MOVE:   u8 = 0x01;
const IN_MOUSE_BUTTON: u8 = 0x02;
const IN_KEY_PRESS:    u8 = 0x03;
const IN_SCROLL:       u8 = 0x04;
const IN_CHAR_INPUT:   u8 = 0x05;
const IN_PING:         u8 = 0x06;

// ── 세션 루프 ────────────────────────────────────────────────────────────────

pub async fn run(
    mut stream: FramedStream,
    frame_tx: mpsc::SyncSender<FrameBuffer>,
    mut input_rx: tokio::sync::mpsc::UnboundedReceiver<InputEvent>,
) -> Result<()> {
    log::info!("[session] Viewer 세션 시작");

    // 5초 주기 Ping (RTT 측정)
    let mut ping_tick = tokio::time::interval(std::time::Duration::from_secs(5));
    ping_tick.tick().await; // 첫 tick 즉시 소모

    loop {
        tokio::select! {
            // ── 에이전트 메시지 수신 ─────────────────────────────────────────
            recv = stream.next() => {
                match recv {
                    Some(Ok(bytes)) => handle_agent_msg(&bytes, &frame_tx),
                    Some(Err(e)) => {
                        log::warn!("[session] 수신 오류: {:?}", e);
                        break;
                    }
                    None => {
                        log::info!("[session] 에이전트 연결 종료");
                        break;
                    }
                }
            }
            // ── 입력 이벤트 → 에이전트 송신 ─────────────────────────────────
            Some(input) = input_rx.recv() => {
                if let Some(data) = input_to_bytes(input) {
                    if let Err(e) = stream.send_bytes(bytes::Bytes::from(data)).await {
                        log::warn!("[session] 입력 전송 오류: {:?}", e);
                        break;
                    }
                }
            }
            // ── 주기적 Ping ──────────────────────────────────────────────────
            _ = ping_tick.tick() => {
                let ts = now_ms();
                let mut data = vec![IN_PING];
                data.extend_from_slice(&ts.to_be_bytes());
                if let Err(e) = stream.send_bytes(bytes::Bytes::from(data)).await {
                    log::warn!("[session] Ping 전송 오류: {:?}", e);
                    break;
                }
            }
        }
    }

    log::info!("[session] Viewer 세션 종료");
    Ok(())
}

// ── 에이전트 메시지 파싱 ─────────────────────────────────────────────────────

fn handle_agent_msg(bytes: &[u8], frame_tx: &mpsc::SyncSender<FrameBuffer>) {
    if bytes.is_empty() {
        return;
    }
    match bytes[0] {
        MSG_INIT if bytes.len() >= 9 => {
            let w   = u32::from_be_bytes(bytes[1..5].try_into().unwrap());
            let h   = u32::from_be_bytes(bytes[5..9].try_into().unwrap());
            let fps = bytes.get(9).copied().unwrap_or(30);
            log::info!("[session] Init: {}x{} @{}fps", w, h, fps);
        }
        MSG_FRAME if bytes.len() >= 5 => {
            let jpeg_len = u32::from_be_bytes(bytes[1..5].try_into().unwrap()) as usize;
            if bytes.len() < 5 + jpeg_len {
                log::warn!("[session] 프레임 데이터 부족");
                return;
            }
            match decoder::decode_jpeg(&bytes[5..5 + jpeg_len]) {
                Ok((w, h, pixels)) => {
                    let _ = frame_tx.try_send(FrameBuffer { pixels, width: w, height: h });
                }
                Err(e) => log::warn!("[session] JPEG 디코딩 실패: {:?}", e),
            }
        }
        MSG_PONG if bytes.len() >= 9 => {
            let sent = u64::from_be_bytes(bytes[1..9].try_into().unwrap());
            let rtt  = now_ms().saturating_sub(sent);
            log::debug!("[session] RTT: {}ms", rtt);
        }
        _ => {}
    }
}

// ── 입력 이벤트 직렬화 ───────────────────────────────────────────────────────

fn input_to_bytes(input: InputEvent) -> Option<Vec<u8>> {
    match input {
        InputEvent::MouseMove { x, y, win_w, win_h } => {
            let mut d = vec![IN_MOUSE_MOVE];
            d.extend_from_slice(&x.to_be_bytes());
            d.extend_from_slice(&y.to_be_bytes());
            d.extend_from_slice(&(win_w as u16).to_be_bytes());
            d.extend_from_slice(&(win_h as u16).to_be_bytes());
            Some(d)
        }
        InputEvent::MouseButton { button, pressed } => {
            Some(vec![IN_MOUSE_BUTTON, button as u8, pressed as u8])
        }
        InputEvent::KeyPress { key, pressed } => {
            let mut d = vec![IN_KEY_PRESS];
            d.extend_from_slice(&key.to_be_bytes());
            d.push(pressed as u8);
            Some(d)
        }
        InputEvent::Scroll { dx, dy } => {
            let mut d = vec![IN_SCROLL];
            d.extend_from_slice(&dx.to_be_bytes());
            d.extend_from_slice(&dy.to_be_bytes());
            Some(d)
        }
        InputEvent::CharInput { text } => {
            let bytes = text.as_bytes();
            if bytes.len() > u16::MAX as usize {
                return None;
            }
            let mut d = vec![IN_CHAR_INPUT];
            d.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
            d.extend_from_slice(bytes);
            Some(d)
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
