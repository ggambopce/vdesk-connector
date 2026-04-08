//! TCP 리스너 — 원격 제어 세션 담당
//!
//! AgentState 전환:
//!   Pending ──[핸드쉐이크 성공]──► Streaming ──[세션 종료]──► Idle
//!   Pending ──[핸드쉐이크 실패]──► Idle

use anyhow::Result;
use hbb_common::{log, tcp::FramedStream, tokio::net::TcpListener};
use std::net::SocketAddr;

use crate::state::{AgentState, SharedState};

pub const LISTEN_PORT: u16 = 20020;

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

        // Pending 상태일 때만 연결 허용 — expected session key 가져오기
        let expected_key = {
            let s = state.lock().unwrap();
            match &*s {
                AgentState::Pending { session_key } => Some(session_key.clone()),
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

        let expected_key = match expected_key {
            Some(k) => k,
            None => continue,
        };

        log::info!("[server] 연결 수락: {}", peer_addr);
        tcp_stream.set_nodelay(true).ok();

        let local_addr = tcp_stream.local_addr()?;
        let state_task = state.clone();

        tokio::spawn(async move {
            let mut stream = FramedStream::from(tcp_stream, local_addr);

            match handshake(&mut stream, &expected_key).await {
                Ok(session_key) => {
                    log::info!("[server] 핸드쉐이크 성공 → Streaming: {}", session_key);
                    // Pending → Streaming
                    *state_task.lock().unwrap() = AgentState::Streaming {
                        session_key: session_key.clone(),
                    };

                    if let Err(e) = crate::session::run(stream, session_key).await {
                        log::error!("[session] 세션 오류: {:?}", e);
                    }

                    // Streaming → Idle (재폴링 허용)
                    *state_task.lock().unwrap() = AgentState::Idle;
                    log::info!("[server] 세션 종료 → Idle");
                }
                Err(e) => {
                    log::warn!("[server] 핸드쉐이크 실패 ({}): {:?}", peer_addr, e);
                    // Pending → Idle (재폴링 허용)
                    *state_task.lock().unwrap() = AgentState::Idle;
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

        // spawn 없이 인라인 실행 — 세션이 완전히 종료(DXGI 핸들 해제)된 후 다음 연결 수락
        let mut stream = FramedStream::from(tcp_stream, local_addr);
        match handshake(&mut stream, &fixed_key).await {
            Ok(session_key) => {
                log::info!("[server] 핸드쉐이크 성공 → 스트리밍 시작");
                if let Err(e) = crate::session::run(stream, session_key).await {
                    log::error!("[session] 세션 오류: {:?}", e);
                }
                log::info!("[server] 세션 종료 — 다음 연결 대기");
            }
            Err(e) => log::warn!("[server] 핸드쉐이크 실패 ({}): {:?}", peer_addr, e),
        }
    }
}

/// 세션 키 핸드쉐이크: 뷰어가 보낸 sessionKey 검증 후 0x01/0x00 응답
///
/// 검증 방식: 뷰어가 보낸 sessionKey를 activate() 응답으로 얻은 expected_key와 비교.
/// relay API 재검증은 하지 않음 — 해당 endpoint가 뷰어 JWT 쿠키를 요구하여 에이전트가
/// 인증 없이 호출하면 404를 반환하기 때문. activate() 성공 자체가 세션 유효성 보증.
async fn handshake(stream: &mut FramedStream, expected_key: &str) -> Result<String> {
    let bytes = match stream.next().await {
        Some(Ok(b)) => b,
        Some(Err(e)) => anyhow::bail!("수신 오류: {:?}", e),
        None => anyhow::bail!("연결 종료"),
    };

    let session_key = String::from_utf8(bytes.to_vec())
        .map_err(|_| anyhow::anyhow!("sessionKey UTF-8 오류"))?;
    let session_key = session_key.trim_end_matches('\0').to_string();
    log::info!("[server] sessionKey 수신: {}", session_key);

    // activate()로 받은 expected_key와 일치 여부만 확인
    let valid = session_key == expected_key;
    log::info!(
        "[server] 검증: {} → {}",
        session_key,
        if valid { "OK" } else { "DENY (키 불일치)" }
    );

    stream
        .send_bytes(bytes::Bytes::from_static(if valid { &[1u8] } else { &[0u8] }))
        .await?;

    if valid {
        Ok(session_key)
    } else {
        anyhow::bail!("세션 키 불일치: viewer={}, expected={}", session_key, expected_key)
    }
}
