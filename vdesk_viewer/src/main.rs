//! VDesk Viewer
//!
//! 실행 모드:
//!   1. URI 스킴 모드 — 브라우저에서 vdesk://connect?... 호출 시 자동 실행
//!      인수로 URI를 받아 sessionKey/connectToken/relayIp/relayPort 파싱 후 바로 연결
//!
//!   2. 다이렉트 모드 (VDESK_DIRECT=1) — 백엔드 없이 직접 에이전트에 연결 (개발/테스트용)
//!
//!   3. 백엔드 모드 (기본) — 로그인 → 디바이스 선택 → 세션 생성 → 연결
//!
//! 환경변수:
//!   VDESK_DIRECT      — 1이면 다이렉트 모드
//!   VDESK_DIRECT_HOST — 다이렉트 모드 에이전트 IP (기본: 127.0.0.1)
//!   VDESK_DIRECT_PORT — 다이렉트 모드 포트 (기본: 20020)
//!   VDESK_DIRECT_KEY  — 다이렉트 모드 세션키 (기본: "direct")
//!   VDESK_MOUSE_GLOBAL — 1이면 마우스를 가상 화면 절대 좌표로 전송
//!   VDESK_API_URL     — 백엔드 URL (기본: http://localhost:8080)
//!   VDESK_EMAIL       — 이메일 (백엔드 모드)
//!   VDESK_PASSWORD    — 비밀번호 (백엔드 모드)
//!   RUST_LOG          — 로그 레벨

mod api;
mod connection;
mod decoder;
mod display;
mod session;
mod vpx_dec;

use anyhow::Result;
use hbb_common::log;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

// ── URI 파라미터 ───────────────────────────────────────────────────────────────

struct UriParams {
    session_key:   String,
    connect_token: String,
    relay_ip:      String,
    relay_port:    u16,
    session_id:    u64,
}

/// vdesk://connect?sessionKey=...&connectToken=...&relayIp=...&relayPort=... 파싱
fn parse_uri(uri: &str) -> Result<UriParams> {
    let query = uri.splitn(2, '?').nth(1)
        .ok_or_else(|| anyhow::anyhow!("URI에 쿼리 파라미터가 없습니다: {}", uri))?;

    let mut session_key   = String::new();
    let mut connect_token = String::new();
    let mut relay_ip      = String::new();
    let mut relay_port: u16 = 20020;
    let mut session_id: u64 = 0;

    for pair in query.trim_end_matches('/').split('&') {
        let mut kv  = pair.splitn(2, '=');
        let key = kv.next().unwrap_or("").trim();
        let val = kv.next().unwrap_or("").trim();
        // URLSearchParams가 생성한 %2B 등 최소 디코딩
        let val = val.replace("%3A", ":").replace("%2B", "+").replace("%2F", "/");
        match key {
            "sessionKey"   => session_key   = val,
            "connectToken" => connect_token = val,
            "relayIp"      => relay_ip      = val,
            "relayPort"    => relay_port    = val.parse().unwrap_or(20020),
            "sessionId"    => session_id    = val.parse().unwrap_or(0),
            _ => {}
        }
    }

    if session_key.is_empty() {
        anyhow::bail!("URI 파라미터 누락: sessionKey");
    }
    if relay_ip.is_empty() {
        anyhow::bail!("URI 파라미터 누락: relayIp");
    }

    Ok(UriParams { session_key, connect_token, relay_ip, relay_port, session_id })
}

// ── Windows vdesk:// URI 스킴 레지스트리 등록 ────────────────────────────────

#[cfg(target_os = "windows")]
fn register_uri_scheme() {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;

    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => { log::warn!("[uri] 실행 경로 조회 실패: {:?}", e); return; }
    };
    let exe_str = exe.to_string_lossy();
    let open_cmd = format!("\"{}\" \"%1\"", exe_str);

    let entries = [
        (r"HKCU\SOFTWARE\Classes\vdesk",                        "/ve",            "VDesk Remote Viewer"),
        (r"HKCU\SOFTWARE\Classes\vdesk",                        "URL Protocol",   ""),
        (r"HKCU\SOFTWARE\Classes\vdesk\DefaultIcon",            "/ve",            &format!("\"{}\",0", exe_str)),
        (r"HKCU\SOFTWARE\Classes\vdesk\shell\open\command",     "/ve",            &open_cmd),
    ];

    for (key, name, value) in &entries {
        let ok = std::process::Command::new("reg")
            .args(["add", key, "/v", name, "/d", value, "/f"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ok {
            log::warn!("[uri] 레지스트리 등록 실패: {} / {}", key, name);
        }
    }
    log::info!("[uri] vdesk:// URI 스킴 등록: {}", exe_str);
}

#[cfg(not(target_os = "windows"))]
fn register_uri_scheme() {}

// ── URI 스킴 모드 진입점 ──────────────────────────────────────────────────────

fn run_uri_mode(params: UriParams) -> Result<()> {
    log::info!(
        "[uri] 브라우저 연동 모드 — {}:{} sessionKey={}",
        params.relay_ip, params.relay_port,
        &params.session_key[..params.session_key.len().min(8)]
    );

    let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<display::FrameBuffer>(2);
    let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel::<display::InputEvent>();

    let relay_ip      = params.relay_ip;
    let relay_port    = params.relay_port;
    let session_key   = params.session_key;
    let connect_token = params.connect_token;

    // 창 닫기 후 세션 종료에 쓸 복사본 (session_key는 스레드로 이동됨)
    let session_key_for_end = session_key.clone();

    // ── alive 폴 스레드 ──────────────────────────────────────────────────────
    // 대시보드에서 "뷰어 닫기"를 누르면 세션이 ENDED → alive=false → 프로세스 종료
    // 에이전트 heartbeat(15s)를 기다리지 않고 5초 이내에 창을 닫는다
    {
        let sk = session_key.clone();
        std::thread::spawn(move || {
            // 연결 확립 전 10초 대기 (PENDING 구간에는 alive=true이므로 오작동 없음)
            std::thread::sleep(std::time::Duration::from_secs(10));
            loop {
                std::thread::sleep(std::time::Duration::from_secs(5));
                match api::check_alive(&sk) {
                    Ok(true)  => { /* 정상 — 계속 */ }
                    Ok(false) => {
                        log::info!("[uri] alive=false 감지 → 뷰어 종료");
                        std::process::exit(0);
                    }
                    Err(e) => {
                        log::warn!("[uri] alive 폴링 실패 (무시): {:?}", e);
                    }
                }
            }
        });
    }

    // ── viewer heartbeat 스레드 ───────────────────────────────────────────────
    // SessionTimeoutScheduler가 lastViewerSeenAt > 30s인 RUNNING 세션을 TIMEOUT 처리함.
    // URI 모드에서 JWT 없이 heartbeat를 보내 세션이 30초 만에 끊기지 않도록 함.
    if params.session_id > 0 {
        let (sk, sid) = (session_key.clone(), params.session_id);
        std::thread::spawn(move || {
            // 연결 확립 후 8초 대기 (PENDING→RUNNING 전환 대기)
            std::thread::sleep(std::time::Duration::from_secs(8));
            loop {
                if let Err(e) = api::viewer_heartbeat_uri(sid, &sk) {
                    log::warn!("[uri] viewer heartbeat 실패 (무시): {:?}", e);
                } else {
                    log::debug!("[uri] viewer heartbeat 전송 완료");
                }
                std::thread::sleep(std::time::Duration::from_secs(10));
            }
        });
    }

    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            log::info!("[uri] 에이전트 연결 대기 중 (최대 60s)...");
            match connection::retry_connect(&relay_ip, relay_port, &session_key, &connect_token, 60).await {
                Ok(stream) => {
                    log::info!("[uri] 에이전트 연결 성공 — 스트리밍 시작");
                    if let Err(e) = session::run(stream, frame_tx, input_rx).await {
                        log::error!("[uri] 세션 오류: {:?}", e);
                    }
                }
                Err(e) => {
                    log::error!("[uri] 연결 실패: {:?}", e);
                    // 연결 실패 시 즉시 세션 종료 → 대시보드 poll이 ENDED 감지 가능
                    // (window.location.href 방식이라 대시보드가 새로고침 안 됐으므로 여기서 정리 필요)
                    let sk = session_key.clone();
                    tokio::task::spawn_blocking(move || {
                        if let Err(e2) = api::end_session_by_key(&sk) {
                            log::warn!("[uri] end-by-key 실패 (무시, 스케줄러가 처리): {:?}", e2);
                        } else {
                            log::info!("[uri] 연결 실패 후 세션 종료 완료");
                        }
                    }).await.ok();
                }
            }
        });
    });

    let mouse_global = std::env::var("VDESK_MOUSE_GLOBAL").map_or(false, |v| v == "1");
    display::run_event_loop(frame_rx, Some(input_tx), mouse_global)?;

    // 창이 닫히면 무조건 세션 종료 (연결 중이든 스트리밍 중이든)
    log::info!("[uri] 뷰어 창 닫힘 → 세션 종료 요청");
    if let Err(e) = api::end_session_by_key(&session_key_for_end) {
        log::warn!("[uri] 세션 종료 실패 (무시, 스케줄러가 처리): {:?}", e);
    } else {
        log::info!("[uri] 세션 종료 완료");
    }
    Ok(())
}

// ── 메인 ──────────────────────────────────────────────────────────────────────

fn main() -> Result<()> {
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "info");
    }
    env_logger::init();
    log::info!("VDesk Viewer v{} 시작", env!("CARGO_PKG_VERSION"));

    // vdesk:// URI 스킴 레지스트리 등록 (항상 실행 — 경로 변경 시 자동 갱신)
    register_uri_scheme();

    // ── URI 스킴 모드: 브라우저가 인수로 vdesk://connect?... 전달 ────────────
    let args: Vec<String> = std::env::args().collect();
    if let Some(uri) = args.iter().skip(1).find(|a| a.starts_with("vdesk://")) {
        match parse_uri(uri) {
            Ok(params) => return run_uri_mode(params),
            Err(e) => {
                log::error!("[uri] URI 파싱 실패: {:?}", e);
                // 파싱 실패해도 창은 열어서 오류 안내
                eprintln!("잘못된 연결 URI입니다. 브라우저에서 다시 시도하세요.\n{:?}", e);
                return Ok(());
            }
        }
    }

    // ── 다이렉트 모드: 백엔드 없이 에이전트에 직접 연결 ─────────────────────
    if std::env::var("VDESK_DIRECT").map_or(false, |v| v == "1") {
        let host = std::env::var("VDESK_DIRECT_HOST")
            .unwrap_or_else(|_| "127.0.0.1".to_string());
        let port: u16 = std::env::var("VDESK_DIRECT_PORT")
            .ok().and_then(|p| p.parse().ok()).unwrap_or(20020);
        let session_key   = std::env::var("VDESK_DIRECT_KEY").unwrap_or_else(|_| "direct".to_string());
        let connect_token = std::env::var("VDESK_DIRECT_TOKEN").unwrap_or_else(|_| "direct".to_string());

        log::info!("★ 다이렉트 모드 — {}:{} (키: {})", host, port, session_key);

        let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<display::FrameBuffer>(1);
        let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel::<display::InputEvent>();

        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async move {
                match connection::connect(&host, port, &session_key, &connect_token).await {
                    Ok(stream) => {
                        if let Err(e) = session::run(stream, frame_tx, input_rx).await {
                            log::error!("[main] 세션 오류: {:?}", e);
                        }
                    }
                    Err(e) => log::error!("[main] 연결 실패: {:?}", e),
                }
            });
        });

        display::run_event_loop(frame_rx, Some(input_tx), false)?;
        return Ok(());
    }

    // ── 백엔드 모드: 로그인 → 디바이스 선택 → 세션 생성 → 연결 ──────────────
    let email    = std::env::var("VDESK_EMAIL").unwrap_or_else(|_| prompt("이메일: "));
    let password = std::env::var("VDESK_PASSWORD").unwrap_or_else(|_| prompt("비밀번호: "));

    log::info!("[main] 로그인 중...");
    let client = api::login(&email, &password)?;
    log::info!("[main] 로그인 성공");

    let device_id = select_device(&client)?;

    log::info!("[main] 세션 생성 (device={})", device_id);
    let session_info = api::create_session(&client, device_id)?;
    log::info!(
        "[main] 세션: {} ({}) relay={}:{}",
        session_info.session_key, session_info.status,
        session_info.relay_ip, session_info.relay_port
    );

    let session_id    = session_info.session_id;
    let relay_ip      = session_info.relay_ip.clone();
    let relay_port    = session_info.relay_port;
    let session_key   = session_info.session_key.clone();
    let connect_token = session_info.connect_token.clone();

    let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<display::FrameBuffer>(2);
    let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel::<display::InputEvent>();

    let client    = Arc::new(client);
    let hb_client = client.clone();
    let hb_key    = session_key.clone();

    // 세션 종료 시 heartbeat 스레드를 멈추기 위한 공유 플래그
    let session_ended = Arc::new(AtomicBool::new(false));
    let hb_stop       = session_ended.clone();
    let conn_stop     = session_ended.clone();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            log::info!("[main] 에이전트 연결 대기 중 (최대 60s)...");
            match connection::retry_connect(&relay_ip, relay_port, &session_key, &connect_token, 60).await {
                Ok(stream) => {
                    log::info!("[main] 에이전트 연결 성공");
                    if let Err(e) = session::run(stream, frame_tx, input_rx).await {
                        log::error!("[main] 세션 오류: {:?}", e);
                    }
                }
                Err(e) => log::error!("[main] 연결 실패: {:?}", e),
            }
        });
        // 세션 종료(정상/비정상 모두) → heartbeat 스레드 중단 + 백엔드 종료 보고
        conn_stop.store(true, Ordering::Relaxed);
        let _ = api::end_session(&client, session_id);
        log::info!("[main] 세션 종료");
    });

    std::thread::spawn(move || {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(10));
            // 연결 스레드가 종료됐으면 heartbeat 중단
            if hb_stop.load(Ordering::Relaxed) {
                log::info!("[main] heartbeat 스레드 종료 (세션 종료됨)");
                break;
            }
            match api::viewer_heartbeat(&hb_client, session_id, &hb_key) {
                Ok(data) => {
                    if data.should_terminate {
                        log::info!(
                            "[main] 백엔드 종료 지시 수신 (status={}) → heartbeat 중단",
                            data.status
                        );
                        // 연결 스레드에 종료 알림 (다음 heartbeat 방지)
                        hb_stop.store(true, Ordering::Relaxed);
                        break;
                    }
                }
                Err(e) => log::warn!("[main] viewer heartbeat 실패: {:?}", e),
            }
        }
    });

    let mouse_global = std::env::var("VDESK_MOUSE_GLOBAL").map_or(false, |v| v == "1");
    display::run_event_loop(frame_rx, Some(input_tx), mouse_global)?;

    log::info!("[main] 종료");
    Ok(())
}

// ── 헬퍼 ──────────────────────────────────────────────────────────────────────

fn select_device(client: &api::ViewerClient) -> Result<u64> {
    // --device <id> CLI 인자
    let args: Vec<String> = std::env::args().collect();
    let mut iter = args.iter().skip(1);
    while let Some(arg) = iter.next() {
        if arg == "--device" {
            if let Some(val) = iter.next() {
                if let Ok(id) = val.parse::<u64>() {
                    return Ok(id);
                }
            }
        }
    }

    // 연결된 디바이스 목록
    let linked = api::list_devices(client).unwrap_or_default();
    if !linked.is_empty() {
        println!("\n연결된 디바이스 목록:");
        for d in &linked {
            println!("  [{}] {} ({})", d.device_id, d.device_name, d.host_status);
        }
        let s = prompt("디바이스 ID: ");
        return Ok(s.trim().parse().expect("유효한 숫자를 입력하세요"));
    }

    // 탐색 후 link
    match api::discover_devices(client) {
        Ok(discovered) if !discovered.is_empty() => {
            println!("\n발견된 미연결 디바이스:");
            for d in &discovered {
                println!("  [{}] {} ({})", d.device_id, d.host_name, d.os_type);
            }
            let s         = prompt("연결할 디바이스 ID: ");
            let chosen_id: u64 = s.trim().parse().expect("유효한 숫자를 입력하세요");
            if let Some(d) = discovered.iter().find(|d| d.device_id == chosen_id) {
                match api::link_device(client, &d.device_key) {
                    Ok(l)  => log::info!("[main] 디바이스 연결 완료: {}", l.device_name),
                    Err(e) => log::warn!("[main] link 실패 (이미 연결됐을 수 있음): {:?}", e),
                }
            }
            Ok(chosen_id)
        }
        Ok(_) => {
            eprintln!("연결 가능한 디바이스가 없습니다. 에이전트가 실행 중인지 확인하세요.");
            anyhow::bail!("디바이스 없음");
        }
        Err(e) => {
            log::warn!("[main] 탐색 실패: {:?}", e);
            let s = prompt("디바이스 ID를 직접 입력: ");
            Ok(s.trim().parse().expect("유효한 숫자를 입력하세요"))
        }
    }
}

fn prompt(msg: &str) -> String {
    use std::io::Write;
    print!("{}", msg);
    std::io::stdout().flush().ok();
    let mut s = String::new();
    std::io::stdin().read_line(&mut s).ok();
    s.trim().to_string()
}
