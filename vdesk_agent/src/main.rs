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
use std::{
    fs::{self, OpenOptions},
    io::{BufWriter, Write},
    net::UdpSocket,
    sync::Mutex,
    time::Duration,
};
use uuid::Uuid;

// ── DualLogger: stderr + 파일 동시 출력 ──────────────────────────────────────

struct DualLogger {
    stderr: env_logger::Logger,
    file: Mutex<BufWriter<std::fs::File>>,
}

impl log::Log for DualLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        self.stderr.enabled(metadata)
    }

    fn log(&self, record: &log::Record) {
        if !self.stderr.matches(record) {
            return;
        }
        // stderr (콘솔이 있을 때만 보임)
        self.stderr.log(record);

        // 파일 (항상 기록)
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        let line = format!("[{}] {:5} {}\n", now, record.level(), record.args());
        if let Ok(mut w) = self.file.lock() {
            let _ = w.write_all(line.as_bytes());
            let _ = w.flush();
        }
    }

    fn flush(&self) {
        if let Ok(mut w) = self.file.lock() {
            let _ = w.flush();
        }
    }
}

fn init_logger() {
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "info");
    }

    // 로그 디렉터리: exe 위치\logs\vdesk_agent.log
    let log_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("logs")))
        .unwrap_or_else(|| std::path::PathBuf::from("logs"));

    let _ = fs::create_dir_all(&log_dir);
    let log_path = log_dir.join("vdesk_agent.log");

    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path);

    match file {
        Ok(f) => {
            let stderr = env_logger::Builder::from_default_env().build();
            let max_level = stderr.filter();
            let logger = DualLogger {
                stderr,
                file: Mutex::new(BufWriter::new(f)),
            };
            log::set_boxed_logger(Box::new(logger)).ok();
            log::set_max_level(max_level);
        }
        Err(e) => {
            // 파일 열기 실패 시 stderr만 사용
            env_logger::init();
            log::warn!("로그 파일 열기 실패 ({:?}): {}", log_path, e);
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    init_logger();
    log::info!("VDesk Agent 시작 (API: {})", api::base_url());

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

    let agent_name = get_hostname();
    let os_type = if cfg!(target_os = "windows") {
        "WINDOWS"
    } else if cfg!(target_os = "macos") {
        "MAC"
    } else {
        "LINUX"
    }
    .to_string();

    let relay_port: u16 = std::env::var("AGENT_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(server::LISTEN_PORT);

    // ── 백엔드 등록 ───────────────────────────────────────────────────────────
    let reg_data = api::register(&api::RegisterRequest {
        local_box,
        agent_name,
        os_type,
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        relay_ip: local_ip.clone(),
        relay_port,
    })
    .await?;
    let device_key = reg_data.device_key.clone();
    log::info!("등록 완료 — deviceKey: {} agentId: {}", device_key, reg_data.agent_id);

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
    let hb_interval   = Duration::from_secs(10); // 비정상 종료 감지: 10s
    let poll_idle     = Duration::from_secs(3);  // Idle: 3s — ngrok free 40req/min 이내 (20/min)
    let poll_pending  = Duration::from_secs(1);  // Pending: 1s — 세션 취소 즉시 감지
    let mut last_hb = std::time::Instant::now();

    log::info!("백엔드 폴링 루프 시작");
    loop {
        // heartbeat (세션 상태와 무관하게 주기적으로 전송)
        if last_hb.elapsed() >= hb_interval {
            let current_session_key = shared_state.lock().unwrap().session_key().map(String::from);
            let is_idle = shared_state.lock().unwrap().is_idle();
            if let Err(e) = api::heartbeat(&api::HeartbeatRequest {
                device_key: device_key.clone(),
                relay_ip: local_ip.clone(),
                relay_port,
                app_version: env!("CARGO_PKG_VERSION").to_string(),
                agent_status: "ONLINE".to_string(),
                session_acceptable: is_idle,
                current_session_key,
            })
            .await
            {
                log::warn!("[heartbeat] 실패: {:?}", e);
            } else {
                log::debug!("[heartbeat] OK");
            }
            last_hb = std::time::Instant::now();
        }

        // Idle 상태일 때만 poll — Streaming 중에는 건너뜀
        // Pending 중에는 세션 취소 여부를 백엔드에 확인 (사용자가 모달 닫기 시 즉시 감지)
        if !shared_state.lock().unwrap().is_idle() {
            let pending_info = {
                let s = shared_state.lock().unwrap();
                match &*s {
                    AgentState::Pending { session_key, device_key } => {
                        Some((session_key.clone(), device_key.clone()))
                    }
                    _ => None,
                }
            };

            if let Some((session_key, device_key)) = pending_info {
                let check_req = api::EndRequest { device_key, session_key };
                match api::check_pending_session(&check_req).await {
                    Ok(data) if data.should_reset => {
                        log::info!("[pending] 백엔드 세션 취소 감지 → Idle 복귀");
                        *shared_state.lock().unwrap() = AgentState::Idle;
                    }
                    Ok(_) => log::trace!("[pending] 세션 유효 — 뷰어 대기 중"),
                    Err(e) => log::warn!("[pending] check-pending 실패 (무시): {:?}", e),
                }
            }

            tokio::time::sleep(poll_pending).await;
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
                        tokio::time::sleep(poll_idle).await;
                        continue;
                    }
                };
                log::info!("[poll] 대기 세션 발견: {}", session_key);

                match api::activate(&device_key, &session_key).await {
                    Ok(data) => {
                        log::info!("[activate] 완료: sessionKey={} status={}", data.session_key, data.status);
                        // Idle → Pending: 원격 제어 세션이 TCP 연결을 기다림
                        *shared_state.lock().unwrap() = AgentState::Pending {
                            session_key: data.session_key.clone(),
                            device_key: device_key.clone(),
                        };

                        // Pending 타임아웃 감시 태스크 (5분 안에 뷰어 미접속 시 Idle 복귀)
                        let timeout_state = shared_state.clone();
                        let timeout_dk = device_key.clone();
                        let timeout_sk = data.session_key.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_secs(300)).await;
                            // 락을 블록으로 감싸 MutexGuard가 await 전에 반드시 해제되도록 함
                            let timed_out = {
                                let mut state = timeout_state.lock().unwrap();
                                if matches!(*state, AgentState::Pending { .. }) {
                                    log::warn!("[pending] 5분 타임아웃 — Idle 전환 + session/end 보고");
                                    *state = AgentState::Idle;
                                    true
                                } else {
                                    false
                                }
                            }; // MutexGuard 해제
                            if timed_out {
                                let end_req = api::EndRequest {
                                    device_key: timeout_dk,
                                    session_key: timeout_sk,
                                };
                                if let Err(e) = api::end_session(&end_req).await {
                                    log::warn!("[pending] session/end 실패 (무시): {:?}", e);
                                }
                            }
                        });
                    }
                    Err(e) => log::warn!("[activate] 실패: {:?}", e),
                }
            }
            Ok(_) => log::trace!("[poll] 대기 세션 없음"),
            Err(e) => log::warn!("[poll] 오류: {:?}", e),
        }

        tokio::time::sleep(poll_idle).await;
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
