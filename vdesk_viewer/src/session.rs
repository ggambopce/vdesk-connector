//! Viewer 세션 루프
//!
//! ── 수신 (에이전트 → 뷰어) ────────────────────────────────────────────────────
//!   0x10 Init:  [width(4BE), height(4BE), fps(1), codec(1)]
//!               codec: 1=VP9
//!   0x11 Frame: [is_key(1), data_len(4BE), vp9_data]
//!   0x12 Pong:  [timestamp(8BE)]
//!   0x13 CursorShape: [cursor_type(1)]  — 0=Arrow 1=IBeam 2=SizeWE … 9=No
//!
//! ── 송신 (뷰어 → 에이전트) ───────────────────────────────────────────────────
//!   0x01 MouseMove   0x02 MouseButton  0x03 KeyPress
//!   0x04 Scroll      0x06 Ping
//!   0x07 MouseGlobal 0x08 KeyVk

use anyhow::Result;
use hbb_common::{log, tcp::FramedStream};
use std::{
    sync::mpsc,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{
    decoder,
    display::{FrameBuffer, InputEvent},
    vpx_dec::VpxDecoder,
};

// ── 타입 상수 ────────────────────────────────────────────────────────────────
const MSG_INIT:   u8 = 0x10;
const MSG_FRAME:  u8 = 0x11;
const MSG_PONG:   u8 = 0x12;
const MSG_CURSOR: u8 = 0x13;

const IN_MOUSE_MOVE:   u8 = 0x01;
const IN_MOUSE_BUTTON: u8 = 0x02;
const IN_KEY_PRESS:    u8 = 0x03;
const IN_SCROLL:       u8 = 0x04;
const IN_PING:         u8 = 0x06;
const IN_MOUSE_GLOBAL: u8 = 0x07;
const IN_KEY_VK:       u8 = 0x08;

// ── 코덱 타입 ────────────────────────────────────────────────────────────────
#[derive(Debug, Clone, Copy, PartialEq)]
enum Codec { Jpeg, Vp9 }

// ── 세션 루프 ────────────────────────────────────────────────────────────────

pub async fn run(
    mut stream: FramedStream,
    frame_tx: mpsc::SyncSender<FrameBuffer>,
    mut input_rx: tokio::sync::mpsc::UnboundedReceiver<InputEvent>,
) -> Result<()> {
    log::info!("[session] Viewer 세션 시작");

    // VP9 디코더 (Init에서 codec=VP9 확인 후 초기화)
    let mut decoder: Option<VpxDecoder> = None;
    let mut codec = Codec::Vp9; // 기본값

    let mut ping_tick = tokio::time::interval(std::time::Duration::from_secs(1));
    ping_tick.tick().await;

    loop {
        tokio::select! {
            recv = stream.next() => {
                match recv {
                    Some(Ok(bytes)) => {
                        handle_agent_msg(&bytes, &frame_tx, &mut decoder, &mut codec);
                    }
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
            Some(input) = input_rx.recv() => {
                if let Some(data) = input_to_bytes(input) {
                    if let Err(e) = stream.send_bytes(bytes::Bytes::from(data)).await {
                        log::warn!("[session] 입력 전송 오류: {:?}", e);
                        break;
                    }
                }
            }
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

fn handle_agent_msg(
    bytes: &[u8],
    frame_tx: &mpsc::SyncSender<FrameBuffer>,
    decoder: &mut Option<VpxDecoder>,
    codec: &mut Codec,
) {
    if bytes.is_empty() { return; }

    match bytes[0] {
        MSG_INIT if bytes.len() >= 9 => {
            let w   = u32::from_be_bytes(bytes[1..5].try_into().unwrap());
            let h   = u32::from_be_bytes(bytes[5..9].try_into().unwrap());
            let fps = bytes.get(9).copied().unwrap_or(60);
            let c   = bytes.get(10).copied().unwrap_or(1);

            *codec = if c == 1 { Codec::Vp9 } else { Codec::Jpeg };
            log::info!("[session] Init: {}x{} @{}fps codec={} ({})",
                w, h, fps, c, if *codec == Codec::Vp9 { "VP9" } else { "JPEG" });

            if *codec == Codec::Vp9 {
                match VpxDecoder::new() {
                    Ok(dec) => {
                        *decoder = Some(dec);
                        log::info!("[session] VP9 디코더 초기화 완료");
                    }
                    Err(e) => log::error!("[session] VP9 디코더 초기화 실패: {:?}", e),
                }
            } else {
                *decoder = None;
                log::info!("[session] JPEG 폴백 모드");
            }
        }

        // Frame: [is_key(1), data_len(4BE), frame_data]
        MSG_FRAME if bytes.len() >= 6 => {
            let _is_key  = bytes[1] != 0;
            let data_len = u32::from_be_bytes(bytes[2..6].try_into().unwrap()) as usize;
            if bytes.len() < 6 + data_len {
                log::warn!("[session] 프레임 데이터 부족");
                return;
            }
            let frame_data = &bytes[6..6 + data_len];

            match codec {
                Codec::Vp9 => {
                    if let Some(dec) = decoder.as_mut() {
                        match dec.decode(frame_data) {
                            Ok(Some((w, h, pixels))) => {
                                let _ = frame_tx.try_send(FrameBuffer { pixels, width: w, height: h });
                            }
                            Ok(None) => {}
                            Err(e)   => log::warn!("[session] VP9 디코딩 오류: {:?}", e),
                        }
                    }
                }
                Codec::Jpeg => {
                    match decoder::decode_jpeg(frame_data) {
                        Ok((w, h, pixels)) => {
                            let _ = frame_tx.try_send(FrameBuffer { pixels, width: w, height: h });
                        }
                        Err(e) => log::warn!("[session] JPEG 디코딩 오류: {:?}", e),
                    }
                }
            }
        }

        MSG_PONG if bytes.len() >= 9 => {
            let sent = u64::from_be_bytes(bytes[1..9].try_into().unwrap());
            let rtt  = now_ms().saturating_sub(sent);
            if rtt > 150 {
                log::warn!("[session] RTT 높음: {}ms — 화면 지연 가능", rtt);
            } else {
                log::debug!("[session] RTT: {}ms", rtt);
            }
        }

        MSG_CURSOR if bytes.len() >= 2 => {
            let ty = bytes[1];
            crate::display::REMOTE_CURSOR_TYPE.store(ty, std::sync::atomic::Ordering::Relaxed);
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
        InputEvent::MouseMoveGlobal { gx, gy } => {
            let mut d = vec![IN_MOUSE_GLOBAL];
            d.extend_from_slice(&gx.to_be_bytes());
            d.extend_from_slice(&gy.to_be_bytes());
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
        InputEvent::KeyVk { vk, scan, pressed, extended } => {
            let mut d = vec![IN_KEY_VK];
            d.extend_from_slice(&vk.to_be_bytes());
            d.extend_from_slice(&scan.to_be_bytes());
            d.push(pressed as u8);
            d.push(extended as u8);
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
