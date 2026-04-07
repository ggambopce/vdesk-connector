//! VDesk Agent — VM에 설치되어 백엔드에 등록하고 뷰어의 직접 TCP 연결을 수락합니다.
//!
//! 구조:
//!   - main() : 백엔드 폴링 담당 (heartbeat + poll/activate → Pending 상태 설정)
//!   - server : 원격 제어 담당 (TCP 수락 → Streaming 상태 → 세션 종료 → Idle)
//!   공유 상태(SharedState)로 두 역할을 명확히 분리합니다.
//!
//! 환경변수:
//!   VDESK_DIRECT    — 1이면 백엔드 없이 다이렉트 모드 실행
//!   VDESK_DIRECT_KEY— 다이렉트 모드 세션키 (기본: "direct")
//!   VDESK_API_URL   — 백엔드 서버 URL (기본: http://localhost:8080)
//!   AGENT_PORT      — 리스닝 포트 (기본: 20020)
//!   AGENT_RELAY_IP  — 뷰어에게 알려줄 접속 IP (기본: 자동 감지)
//!                     같은 PC에서 테스트할 때: AGENT_RELAY_IP=127.0.0.1
//!   AGENT_NO_INJECT — 1로 설정 시 입력 주입 비활성화 (스트리밍만 테스트할 때)
//!   RUST_LOG        — 로그 레벨 (기본: info)

mod api;
mod server;
mod session;
mod services;
mod state;

use anyhow::Result;
use hbb_common::log;
use state::AgentState;
use std::{net::UdpSocket, time::Duration};
use uuid::Uuid;

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "info");
    }
    env_logger::init();
    log::info!("VDesk Agent 시작");

    // ── 다이렉트 모드: 백엔드 없이 뷰어와 직접 연결 ─────────────────────────
    if std::env::var("VDESK_DIRECT").map_or(false, |v| v == "1") {
        let session_key = std::env::var("VDESK_DIRECT_KEY")
            .unwrap_or_else(|_| "direct".to_string());
        log::info!("★ 다이렉트 모드 (백엔드 불필요) — 세션키: {}", session_key);

        // AGENT_NO_INJECT=1이면 입력 주입 비활성화
        if std::env::var("AGENT_NO_INJECT").map_or(false, |v| v == "1") {
            services::input::set_no_inject(true);
            log::info!("입력 주입 비활성화 (AGENT_NO_INJECT=1)");
        }

        return server::listen_loop_direct(session_key).await;
    }

    // AGENT_RELAY_IP 환경변수 우선 사용 (같은 PC 테스트: 127.0.0.1)
    let local_ip = std::env::var("AGENT_RELAY_IP").unwrap_or_else(|_| get_local_ip());
    log::info!("Relay IP: {}", local_ip);

    // AGENT_NO_INJECT=1 이면 입력 주입 비활성화 (명시적 옵션)
    if std::env::var("AGENT_NO_INJECT").map_or(false, |v| v == "1") {
        services::input::set_no_inject(true);
        log::info!("입력 주입 비활성화 (AGENT_NO_INJECT=1)");
    }

    let local_box = load_or_create_local_box();
    log::info!("LocalBox: {}", local_box);

    let host_name = get_hostname();
    let os_type = if cfg!(target_os = "windows") {
        "WINDOWS"
    } else if cfg!(target_os = "macos") {
        "MAC"
    } else {
        "LINUX"
    }
    .to_string();

    // ── 백엔드 등록 ───────────────────────────────────────────────────────────
    let reg_data = api::register(&api::RegisterRequest {
        local_box,
        host_name,
        os_type,
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        relay_ip: local_ip.clone(),
    })
    .await?;
    let device_key = reg_data.device_key.clone();
    log::info!("등록 완료 — deviceKey: {}", device_key);

    // ── 공유 상태 생성 (백엔드 폴링 ↔ 원격 제어 세션 분리) ───────────────────
    let shared_state = state::new_state();

    // ── 원격 제어 담당: TCP 리스너 태스크 (독립 실행) ─────────────────────────
    let server_state = shared_state.clone();
    tokio::spawn(async move {
        if let Err(e) = server::listen_loop(server_state).await {
            log::error!("[server] 리스너 오류: {:?}", e);
        }
    });

    // ── 백엔드 폴링 담당: heartbeat + poll 루프 ───────────────────────────────
    let hb_interval = Duration::from_secs(15);
    let poll_interval = Duration::from_secs(3);
    let mut last_hb = std::time::Instant::now();

    log::info!("백엔드 폴링 루프 시작");
    loop {
        // heartbeat (세션 상태와 무관하게 주기적으로 전송)
        if last_hb.elapsed() >= hb_interval {
            if let Err(e) = api::heartbeat(&api::HeartbeatRequest {
                device_key: device_key.clone(),
                relay_ip: local_ip.clone(),
            })
            .await
            {
                log::warn!("[heartbeat] 실패: {:?}", e);
            } else {
                log::debug!("[heartbeat] OK");
            }
            last_hb = std::time::Instant::now();
        }

        // Idle 상태일 때만 poll — Pending/Streaming 중에는 건너뜀
        if !shared_state.lock().unwrap().is_idle() {
            log::trace!("[poll] 세션 활성 중 — 건너뜀");
            tokio::time::sleep(poll_interval).await;
            continue;
        }

        match api::poll(&api::PollRequest {
            device_key: device_key.clone(),
        })
        .await
        {
            Ok(data) if data.has_pending_session => {
                let session_key = match data.session_key {
                    Some(k) => k,
                    None => {
                        log::warn!("[poll] pending인데 sessionKey 없음");
                        tokio::time::sleep(poll_interval).await;
                        continue;
                    }
                };
                log::info!("[poll] 대기 세션 발견: {}", session_key);

                match api::activate(&api::ActivateRequest {
                    device_key: device_key.clone(),
                    session_key: session_key.clone(),
                })
                .await
                {
                    Ok(data) => {
                        log::info!("[activate] 완료: sessionKey={}", data.session_key);
                        // Idle → Pending: 원격 제어 세션이 TCP 연결을 기다림
                        *shared_state.lock().unwrap() = AgentState::Pending {
                            session_key: data.session_key,
                        };
                    }
                    Err(e) => log::warn!("[activate] 실패: {:?}", e),
                }
            }
            Ok(_) => log::trace!("[poll] 대기 세션 없음"),
            Err(e) => log::warn!("[poll] 오류: {:?}", e),
        }

        tokio::time::sleep(poll_interval).await;
    }
}

fn get_local_ip() -> String {
    let socket = UdpSocket::bind("0.0.0.0:0").ok();
    if let Some(s) = socket {
        s.connect("8.8.8.8:80").ok();
        if let Ok(addr) = s.local_addr() {
            return addr.ip().to_string();
        }
    }
    "127.0.0.1".to_string()
}

fn load_or_create_local_box() -> String {
    let path = std::env::temp_dir().join("vdesk_agent_id");
    if let Ok(id) = std::fs::read_to_string(&path) {
        let id = id.trim().to_string();
        if !id.is_empty() {
            return id;
        }
    }
    let id = format!("box{}", &Uuid::new_v4().to_string().replace('-', "")[..16]);
    let _ = std::fs::write(&path, &id);
    id
}

fn get_hostname() -> String {
    #[cfg(target_os = "windows")]
    {
        std::env::var("COMPUTERNAME").unwrap_or_else(|_| "unknown".to_string())
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown".to_string())
    }
}
