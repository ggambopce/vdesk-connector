//! TCP 리스너 — 원격 제어 세션 담당
//!
//! AgentState 전환:
//!   Pending ──[핸드쉐이크 성공]──► Streaming ──[세션 종료]──► Idle
//!   Pending ──[핸드쉐이크 실패]──► Pending 유지 (올바른 뷰어 재시도 허용)

use anyhow::Result;
use hbb_common::{log, tcp::FramedStream, tokio::net::TcpListener};
use serde::Deserialize;
use std::net::SocketAddr;

use crate::state::{AgentState, SharedState};

pub const LISTEN_PORT: u16 = 20020;

/// 뷰어가 TCP 핸드쉐이크 시 전송하는 JSON 메시지
#[derive(Deserialize, Debug)]
struct HandshakeMsg {
    #[serde(rename = "sessionKey")]
    session_key: String,
    #[serde(rename = "connectToken")]
    connect_token: String,
    #[serde(rename = "viewerNonce")]
    viewer_nonce: String,
}

/// TCP 연결 수락 루프 — Pending 상태에서만 연결 처리
pub async fn listen_loop(state: SharedState) -> Result<()> {
    let port = std::env::var("AGENT_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(LISTEN_PORT);

    let addr: SocketAddr = format!("0.0.0.0:{}", port).parse()?;
    let listener = TcpListener::bind(addr).await?;
    log::info!("[server] 뷰어 연결 대기 (포트 {})", port);

    loop {
        let (tcp_stream, peer_addr) = listener.accept().await?;

        // Pending 상태일 때만 연결 허용 — session_key, device_key 가져오기
        let pending_info = {
            let s = state.lock().unwrap();
            match &*s {
                AgentState::Pending { session_key, device_key } => {
                    Some((session_key.clone(), device_key.clone()))
                }
                other => {
                    log::warn!(
                        "[server] {:?} 상태 — 연결 거부: {}",
                        other,
                        peer_addr
                    );
                    None
                }
            }
        };

        let (expected_key, device_key) = match pending_info {
            Some(info) => info,
            None => continue,
        };

        log::info!("[server] 연결 수락: {}", peer_addr);
        tcp_stream.set_nodelay(true).ok();

        let local_addr = tcp_stream.local_addr()?;
        let state_task = state.clone();
        let viewer_ip = peer_addr.ip().to_string();

        tokio::spawn(async move {
            let mut stream = FramedStream::from(tcp_stream, local_addr);

            match handshake(&mut stream, &expected_key, &device_key, &viewer_ip).await {
                Ok(session_key) => {
                    log::info!("[server] 핸드쉐이크 성공 → Streaming: {}", session_key);
                    // Pending → Streaming
                    *state_task.lock().unwrap() = AgentState::Streaming {
                        session_key: session_key.clone(),
                    };

                    if let Err(e) = crate::session::run(stream, session_key.clone(), device_key.clone()).await {
                        log::error!("[session] 세션 오류: {:?}", e);
                    }

                    // 세션 종료를 백엔드에 보고 (idempotent — 이미 ENDED여도 무시)
                    let end_req = crate::api::EndRequest {
                        device_key: device_key.clone(),
                        session_key: session_key.clone(),
                    };
                    if let Err(e) = crate::api::end_session(&end_req).await {
                        log::warn!("[server] session/end 호출 실패 (무시): {:?}", e);
                    } else {
                        log::info!("[server] session/end 보고 완료");
                    }

                    // Streaming → Idle (재폴링 허용)
                    *state_task.lock().unwrap() = AgentState::Idle;
                    log::info!("[server] 세션 종료 → Idle");
                }
                Err(e) => {
                    log::warn!("[server] 핸드쉐이크 실패 ({}): {:?} — Pending 유지", peer_addr, e);
                    // Pending 유지: 올바른 뷰어가 재시도할 수 있도록 Idle로 전환하지 않음
                }
            }
        });
    }
}

/// 다이렉트 모드 TCP 리스너 — 백엔드 없이 고정 세션키로 연결 수락
/// 세션 종료 후 자동으로 다음 연결 대기 (반복 테스트 가능)
pub async fn listen_loop_direct(fixed_key: String) -> Result<()> {
    let port = std::env::var("AGENT_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(LISTEN_PORT);

    let addr: SocketAddr = format!("0.0.0.0:{}", port).parse()?;
    let listener = TcpListener::bind(addr).await?;
    log::info!("[server] 다이렉트 모드 — 뷰어 연결 대기 (포트 {}, 키: {})", port, fixed_key);

    loop {
        let (tcp_stream, peer_addr) = listener.accept().await?;
        log::info!("[server] 연결 수락: {}", peer_addr);
        tcp_stream.set_nodelay(true).ok();

        let local_addr = tcp_stream.local_addr()?;
        let viewer_ip = peer_addr.ip().to_string();

        // 다이렉트 모드에서는 verify-connect 없이 sessionKey만 확인
        let mut stream = FramedStream::from(tcp_stream, local_addr);
        match handshake_direct(&mut stream, &fixed_key).await {
            Ok(session_key) => {
                log::info!("[server] 핸드쉐이크 성공 → 스트리밍 시작");
                if let Err(e) = crate::session::run(stream, session_key, viewer_ip).await {
                    log::error!("[session] 세션 오류: {:?}", e);
                }
                log::info!("[server] 세션 종료 — 다음 연결 대기");
            }
            Err(e) => log::warn!("[server] 핸드쉐이크 실패 ({}): {:?}", peer_addr, e),
        }
    }
}

/// JSON 핸드쉐이크: 뷰어가 보낸 {sessionKey, connectToken, viewerNonce} 검증
/// 1차 로컬 검증(sessionKey 일치) 후 서버 API verify-connect 호출로 최종 승인
async fn handshake(
    stream: &mut FramedStream,
    expected_key: &str,
    device_key: &str,
    viewer_ip: &str,
) -> Result<String> {
    let bytes = match stream.next().await {
        Some(Ok(b)) => b,
        Some(Err(e)) => anyhow::bail!("수신 오류: {:?}", e),
        None => anyhow::bail!("연결 종료"),
    };

    let msg: HandshakeMsg = serde_json::from_slice(&bytes)
        .map_err(|e| anyhow::anyhow!("핸드쉐이크 JSON 파싱 실패: {:?}", e))?;

    log::info!(
        "[server] 핸드쉐이크 수신: sessionKey={} viewerNonce={}",
        msg.session_key,
        msg.viewer_nonce
    );

    // 1차 로컬 검증 — sessionKey 일치
    if msg.session_key != expected_key {
        log::warn!(
            "[server] sessionKey 불일치: viewer={}, expected={}",
            msg.session_key,
            expected_key
        );
        stream
            .send_bytes(bytes::Bytes::from_static(&[0u8]))
            .await?;
        anyhow::bail!("sessionKey 불일치");
    }

    // 2차 서버 검증 — verify-connect API 호출 → PENDING → RUNNING
    match crate::api::verify_connect(
        &msg.session_key,
        device_key,
        &msg.connect_token,
        &msg.viewer_nonce,
        viewer_ip,
    )
    .await
    {
        Ok(data) => {
            log::info!(
                "[server] verify-connect 성공: sessionId={} status={}",
                data.session_id,
                data.status
            );
            stream
                .send_bytes(bytes::Bytes::from_static(&[1u8]))
                .await?;
            Ok(msg.session_key)
        }
        Err(e) => {
            log::warn!("[server] verify-connect 실패: {:?}", e);
            stream
                .send_bytes(bytes::Bytes::from_static(&[0u8]))
                .await?;
            anyhow::bail!("verify-connect 실패: {:?}", e)
        }
    }
}

/// 다이렉트 모드용 핸드쉐이크 — JSON 또는 raw bytes 모두 처리 (백엔드 없음)
async fn handshake_direct(stream: &mut FramedStream, expected_key: &str) -> Result<String> {
    let bytes = match stream.next().await {
        Some(Ok(b)) => b,
        Some(Err(e)) => anyhow::bail!("수신 오류: {:?}", e),
        None => anyhow::bail!("연결 종료"),
    };

    // JSON 파싱 시도 → 실패하면 raw bytes로 처리 (하위 호환)
    let session_key = if let Ok(msg) = serde_json::from_slice::<HandshakeMsg>(&bytes) {
        msg.session_key
    } else {
        String::from_utf8(bytes.to_vec())
            .map_err(|_| anyhow::anyhow!("sessionKey UTF-8 오류"))?
            .trim_end_matches('\0')
            .to_string()
    };

    log::info!("[server] 다이렉트 핸드쉐이크: sessionKey={}", session_key);

    let valid = session_key == expected_key;
    stream
        .send_bytes(bytes::Bytes::from_static(if valid { &[1u8] } else { &[0u8] }))
        .await?;

    if valid {
        Ok(session_key)
    } else {
        anyhow::bail!("sessionKey 불일치: viewer={}, expected={}", session_key, expected_key)
    }
}
