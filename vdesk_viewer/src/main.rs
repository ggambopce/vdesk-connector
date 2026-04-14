//! VDesk Viewer
//!
//! 환경변수:
//!   VDESK_DIRECT      — 1이면 백엔드 없이 다이렉트 모드
//!   VDESK_DIRECT_HOST — 다이렉트 모드 에이전트 IP (기본: 127.0.0.1)
//!   VDESK_DIRECT_PORT — 다이렉트 모드 포트 (기본: 20020)
//!   VDESK_DIRECT_KEY  — 다이렉트 모드 세션키 (기본: "direct")
//!   VDESK_MOUSE_GLOBAL — 1이면 마우스를 가상 화면 절대 좌표로 전송 (백엔드 경로에서 같은 PC 테스트 시)
//!   VDESK_API_URL     — 백엔드 URL (기본: http://localhost:8080)
//!   VDESK_EMAIL       — 이메일
//!   VDESK_PASSWORD    — 비밀번호
//!   RUST_LOG          — 로그 레벨

mod api;
mod connection;
mod decoder;
mod display;
mod session;
mod vpx_dec;

use anyhow::Result;
use hbb_common::log;

fn main() -> Result<()> {
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "info");
    }
    env_logger::init();
    log::info!("VDesk Viewer 시작");

    // ── 다이렉트 모드: 백엔드 없이 에이전트에 직접 연결 ─────────────────────
    if std::env::var("VDESK_DIRECT").map_or(false, |v| v == "1") {
        let host = std::env::var("VDESK_DIRECT_HOST")
            .unwrap_or_else(|_| "127.0.0.1".to_string());
        let port: u16 = std::env::var("VDESK_DIRECT_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(20020);
        let session_key = std::env::var("VDESK_DIRECT_KEY")
            .unwrap_or_else(|_| "direct".to_string());
        // 다이렉트 모드에서는 connectToken을 빈 문자열로 (에이전트도 직접 모드라 검증 생략)
        let connect_token = std::env::var("VDESK_DIRECT_TOKEN")
            .unwrap_or_else(|_| "direct".to_string());

        log::info!("★ 다이렉트 모드 — {}:{} 직접 연결 (키: {})", host, port, session_key);

        let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<display::FrameBuffer>(1);
        let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel::<display::InputEvent>();

        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async move {
                match connection::connect(&host, port, &session_key, &connect_token).await {
                    Ok(stream) => {
                        log::info!("[main] 에이전트 연결 성공");
                        if let Err(e) = session::run(stream, frame_tx, input_rx).await {
                            log::error!("[main] 세션 오류: {:?}", e);
                        }
                    }
                    Err(e) => log::error!("[main] 연결 실패: {:?}", e),
                }
            });
        });

        display::run_event_loop(frame_rx, Some(input_tx), false)?;
        log::info!("[main] 종료");
        return Ok(());
    }

    // CLI 인자: --device <id>
    let device_id = parse_device_id();

    // 인증 정보
    let email = std::env::var("VDESK_EMAIL").unwrap_or_else(|_| prompt("이메일: "));
    let password = std::env::var("VDESK_PASSWORD").unwrap_or_else(|_| prompt("비밀번호: "));

    // 백엔드 로그인 (AT 쿠키 획득)
    log::info!("[main] 로그인 중...");
    let client = api::login(&email, &password)?;
    log::info!("[main] 로그인 성공");

    // --device 미지정 시 디바이스 목록 표시 (연결된 것 → 없으면 탐색 후 link)
    let device_id = if device_id == 0 {
        let linked = api::list_devices(&client).unwrap_or_default();
        if !linked.is_empty() {
            println!("\n연결된 디바이스 목록:");
            for d in &linked {
                println!("  [{}] {} ({})", d.device_id, d.device_name, d.host_status);
            }
            let s = prompt("디바이스 ID: ");
            s.trim().parse().expect("유효한 숫자를 입력하세요")
        } else {
            // 연결된 디바이스 없음 → discover
            match api::discover_devices(&client) {
                Ok(discovered) if !discovered.is_empty() => {
                    println!("\n발견된 미연결 디바이스:");
                    for d in &discovered {
                        println!("  [{}] {} ({})", d.device_id, d.host_name, d.os_type);
                    }
                    let s = prompt("연결할 디바이스 ID: ");
                    let chosen_id: u64 = s.trim().parse().expect("유효한 숫자를 입력하세요");
                    // deviceKey로 link
                    if let Some(d) = discovered.iter().find(|d| d.device_id == chosen_id) {
                        log::info!("[main] 디바이스 연결 중: {}", d.device_key);
                        match api::link_device(&client, &d.device_key) {
                            Ok(linked) => {
                                log::info!("[main] 디바이스 연결 완료: {}", linked.device_name);
                            }
                            Err(e) => log::warn!("[main] link 실패 (이미 연결됐을 수 있음): {:?}", e),
                        }
                    }
                    chosen_id
                }
                Ok(_) => {
                    eprintln!("연결 가능한 디바이스가 없습니다. 에이전트가 실행 중인지 확인하세요.");
                    return Ok(());
                }
                Err(e) => {
                    log::warn!("[main] 디바이스 탐색 실패: {:?}", e);
                    let s = prompt("디바이스 ID를 직접 입력: ");
                    s.trim().parse().expect("유효한 숫자를 입력하세요")
                }
            }
        }
    } else {
        device_id
    };

    // 세션 생성 — connectToken + relayIp/relayPort 포함
    log::info!("[main] 세션 생성 (device={})", device_id);
    let session_info = api::create_session(&client, device_id)?;
    log::info!(
        "[main] 세션: {} ({}) relay={}:{}",
        session_info.session_key,
        session_info.status,
        session_info.relay_ip,
        session_info.relay_port
    );

    let session_id = session_info.session_id;
    let relay_ip = session_info.relay_ip.clone();
    let relay_port = session_info.relay_port;
    let session_key = session_info.session_key.clone();
    let connect_token = session_info.connect_token.clone();

    // 채널 생성
    let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<display::FrameBuffer>(2);
    let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel::<display::InputEvent>();

    // 세션 루프 (별도 스레드 + Tokio 런타임)
    // Viewer heartbeat용 client 공유 — Arc로 감싸서 두 스레드에서 사용
    let client = std::sync::Arc::new(client);
    let hb_client = client.clone();
    let hb_session_key = session_key.clone();

    std::thread::spawn(move || {
        // ── async 세션 루프 ──────────────────────────────────────────────────
        // client(reqwest::blocking)를 block_on 안에 두면 내부 Tokio 런타임이
        // async 컨텍스트 안에서 drop되어 패닉이 발생하므로, 세션 코드만 block_on
        // 안에서 실행하고 end_session은 block_on 이후에 호출한다.
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            // 에이전트가 아직 poll 전일 수 있으므로 재시도 루프로 연결
            log::info!("[main] 에이전트 연결 대기 중 (최대 60s)...");
            match connection::retry_connect(&relay_ip, relay_port, &session_key, &connect_token, 60).await {
                Ok(stream) => {
                    log::info!("[main] 에이전트 연결 성공");
                    if let Err(e) = session::run(stream, frame_tx, input_rx).await {
                        log::error!("[main] 세션 오류: {:?}", e);
                    }
                }
                Err(e) => {
                    log::error!("[main] 연결 실패: {:?}", e);
                }
            }
        });
        // rt drop 후 blocking API 호출 (reqwest blocking ↔ Tokio 런타임 충돌 방지)
        drop(rt);
        let _ = api::end_session(&client, session_id);
        log::info!("[main] 세션 종료");
    });

    // Viewer heartbeat 스레드 — 스트리밍 중 10초마다 서버에 보고
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(10));
            if let Err(e) = api::viewer_heartbeat(&hb_client, session_id, &hb_session_key) {
                log::warn!("[main] viewer heartbeat 실패: {:?}", e);
            } else {
                log::debug!("[main] viewer heartbeat OK");
            }
        }
    });

    // winit 이벤트 루프 (메인 스레드 필수)
    let mouse_global =
        std::env::var("VDESK_MOUSE_GLOBAL").map_or(false, |v| v == "1");
    display::run_event_loop(frame_rx, Some(input_tx), mouse_global)?;

    log::info!("[main] 종료");
    Ok(())
}

/// --device <id> 인자가 있으면 반환, 없으면 0 (로그인 후 목록 표시)
fn parse_device_id() -> u64 {
    let args: Vec<String> = std::env::args().collect();
    let mut iter = args.iter().skip(1);
    while let Some(arg) = iter.next() {
        if arg == "--device" {
            if let Some(val) = iter.next() {
                if let Ok(id) = val.parse() {
                    return id;
                }
            }
        }
    }
    0
}

fn prompt(msg: &str) -> String {
    use std::io::Write;
    print!("{}", msg);
    std::io::stdout().flush().ok();
    let mut s = String::new();
    std::io::stdin().read_line(&mut s).ok();
    s.trim().to_string()
}
