//! TCP 리스너 — Spring noVNC 프록시 ↔ TightVNC 투명 파이프
//!
//! 기존 방식(DXGI 캡처 + 커스텀 핸드쉐이크)에서 변경:
//!   Spring WebSocket 프록시가 TCP로 연결 → agent가 즉시 127.0.0.1:5900(TightVNC)으로 파이프
//!
//! 보안: Spring NoVncProxyHandler가 sessionKey 검증 후 TCP 연결하므로
//!        agent는 별도 핸드쉐이크 없이 투명 파이프만 담당.
//!
//! AgentState 전환:
//!   Pending ──[Spring 연결 수락]──► Streaming ──[세션 종료]──► Idle

use anyhow::Result;
use hbb_common::{log, tokio::net::{TcpListener, TcpStream}};
use std::net::SocketAddr;

use crate::state::{AgentState, SharedState};

pub const LISTEN_PORT: u16 = 20020;
const VNC_PORT: u16 = 5900;

/// TCP 연결 수락 루프 — Pending 상태에서 Spring 프록시 연결 수신 → TightVNC 파이프
pub async fn listen_loop(state: SharedState) -> Result<()> {
    let port = std::env::var("AGENT_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(LISTEN_PORT);

    let addr: SocketAddr = format!("0.0.0.0:{}", port).parse()?;
    let listener = TcpListener::bind(addr).await?;
    log::info!("[server] Spring noVNC 프록시 대기 (포트 {} → VNC :{})", port, VNC_PORT);

    loop {
        let (spring_tcp, peer_addr) = listener.accept().await?;

        // Pending 상태일 때만 연결 허용
        let session_info = {
            let s = state.lock().unwrap();
            match &*s {
                AgentState::Pending { session_key, device_key } => {
                    Some((session_key.clone(), device_key.clone()))
                }
                other => {
                    log::warn!("[server] {:?} 상태 — Spring 연결 거부: {}", other, peer_addr);
                    None
                }
            }
        };

        let (session_key, device_key) = match session_info {
            Some(info) => info,
            None => continue,
        };

        log::info!("[server] Spring 연결 수락: {} (session={})", peer_addr, &session_key[..8]);
        spring_tcp.set_nodelay(true).ok();

        // Pending → Streaming
        *state.lock().unwrap() = AgentState::Streaming { session_key: session_key.clone() };

        let state_clone = state.clone();
        tokio::spawn(async move {
            if let Err(e) = pipe_to_vnc(spring_tcp, &session_key, &device_key).await {
                log::error!("[server] VNC 파이프 오류: {:?}", e);
            }

            // 세션 종료 백엔드 보고 (최대 3회, idempotent)
            let end_req = crate::api::EndRequest {
                device_key: device_key.clone(),
                session_key: session_key.clone(),
            };
            for attempt in 1..=3u8 {
                match crate::api::end_session(&end_req).await {
                    Ok(_) => {
                        log::info!("[server] session/end 보고 완료 (시도 {})", attempt);
                        break;
                    }
                    Err(e) => {
                        log::warn!("[server] session/end 시도 {}/3 실패: {:?}", attempt, e);
                        if attempt < 3 {
                            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                        }
                    }
                }
            }

            // Streaming → Idle
            *state_clone.lock().unwrap() = AgentState::Idle;
            log::info!("[server] 세션 종료 → Idle");
        });
    }
}

/// Spring TCP ↔ TightVNC(:5900) 양방향 파이프
/// 참조: VDeskNoVNC Program.cs /proxy_novnc (동일 구조, Rust tokio::io::copy 사용)
async fn pipe_to_vnc(spring_tcp: TcpStream, session_key: &str, _device_key: &str) -> Result<()> {
    let vnc_addr = format!("127.0.0.1:{}", VNC_PORT);
    let vnc_tcp = TcpStream::connect(&vnc_addr).await
        .map_err(|e| anyhow::anyhow!("TightVNC 연결 실패 ({}): {}", vnc_addr, e))?;
    vnc_tcp.set_nodelay(true).ok();

    log::info!("[server] VNC 파이프 시작: Spring ↔ {}", vnc_addr);

    let (mut spring_r, mut spring_w) = spring_tcp.into_split();
    let (mut vnc_r,    mut vnc_w)    = vnc_tcp.into_split();

    // 양방향 동시 복사 — 어느 한쪽이 닫히면 tokio::select! 로 양쪽 종료
    tokio::select! {
        r = tokio::io::copy(&mut spring_r, &mut vnc_w) => {
            log::debug!("[server] Spring→VNC 종료: {:?}", r);
        }
        r = tokio::io::copy(&mut vnc_r, &mut spring_w) => {
            log::debug!("[server] VNC→Spring 종료: {:?}", r);
        }
    }

    log::info!("[server] VNC 파이프 종료 (session={})", &session_key[..8]);
    Ok(())
}

/// 다이렉트 모드 (VDESK_DIRECT=1) — 백엔드 없이 VNC 바로 파이프 (테스트용)
pub async fn listen_loop_direct(_fixed_key: String) -> Result<()> {
    let port = std::env::var("AGENT_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(LISTEN_PORT);

    let addr: SocketAddr = format!("0.0.0.0:{}", port).parse()?;
    let listener = TcpListener::bind(addr).await?;
    log::info!("[server] 다이렉트 모드 — VNC 파이프 대기 (포트 {} → :{})", port, VNC_PORT);

    loop {
        let (spring_tcp, peer_addr) = listener.accept().await?;
        log::info!("[server] 연결 수락: {}", peer_addr);
        spring_tcp.set_nodelay(true).ok();

        tokio::spawn(async move {
            if let Err(e) = pipe_to_vnc(spring_tcp, "direct", "direct").await {
                log::error!("[server] VNC 파이프 오류: {:?}", e);
            }
            log::info!("[server] 세션 종료 — 다음 연결 대기");
        });
    }
}
