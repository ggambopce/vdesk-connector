//! TCP 연결 + JSON 핸드쉐이크
//!
//! 핸드쉐이크 프로토콜 (신규):
//!   1. Viewer → Agent: JSON bytes {"sessionKey":"...","connectToken":"ct_...","viewerNonce":"uuid-..."}
//!   2. Agent → Viewer: [1]=OK (RUNNING 전환 완료), [0]=DENY (connectToken 불일치 또는 만료)

use anyhow::Result;
use hbb_common::{log, tcp::FramedStream};
use serde::Serialize;

#[derive(Serialize)]
struct HandshakeMsg<'a> {
    #[serde(rename = "sessionKey")]
    session_key: &'a str,
    #[serde(rename = "connectToken")]
    connect_token: &'a str,
    #[serde(rename = "viewerNonce")]
    viewer_nonce: String,
}

/// 에이전트에 TCP 연결하고 JSON 핸드쉐이크를 수행합니다.
/// - `connect_token`: 서버 발급 1회용 토큰 (세션 생성 시 응답에 포함)
/// - 응답 `0x01` → 스트림 반환, `0x00` → Error (재시도 신호)
pub async fn connect(
    relay_ip: &str,
    relay_port: u16,
    session_key: &str,
    connect_token: &str,
) -> Result<FramedStream> {
    let addr = format!("{}:{}", relay_ip, relay_port);
    log::info!("[conn] 에이전트 연결 중: {}", addr);

    let mut stream = FramedStream::new(addr.as_str(), None, 10_000).await?;
    log::info!("[conn] TCP 연결 성공");

    // viewerNonce — 재전송 공격 방지용 1회성 식별자
    let viewer_nonce = generate_nonce();

    let msg = HandshakeMsg {
        session_key,
        connect_token,
        viewer_nonce,
    };
    let json_bytes = serde_json::to_vec(&msg)?;

    stream
        .send_bytes(bytes::Bytes::from(json_bytes))
        .await?;

    // 응답 수신
    let bytes = match stream.next().await {
        Some(Ok(b)) => b,
        Some(Err(e)) => anyhow::bail!("핸드쉐이크 오류: {:?}", e),
        None => anyhow::bail!("연결 종료"),
    };

    if bytes.first() == Some(&1u8) {
        log::info!("[conn] 핸드쉐이크 성공 — 스트리밍 시작");
        Ok(stream)
    } else {
        anyhow::bail!("에이전트가 세션을 거부했습니다 (connectToken 불일치 또는 만료)")
    }
}

/// 재시도 루프 — 에이전트가 아직 PENDING이 아닐 수 있으므로 연결 실패 시 1s 대기 후 재시도
pub async fn retry_connect(
    relay_ip: &str,
    relay_port: u16,
    session_key: &str,
    connect_token: &str,
    timeout_secs: u64,
) -> Result<FramedStream> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let mut attempt = 0u32;

    loop {
        attempt += 1;
        match connect(relay_ip, relay_port, session_key, connect_token).await {
            Ok(stream) => return Ok(stream),
            Err(e) => {
                if std::time::Instant::now() >= deadline {
                    anyhow::bail!(
                        "{}초 내에 에이전트 연결 실패 ({}번 시도): {:?}",
                        timeout_secs, attempt, e
                    );
                }
                log::info!(
                    "[conn] 재시도 {}: {:?} — 1초 대기",
                    attempt, e
                );
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
    }
}

/// UUID v4 기반 nonce 생성 (128-bit 엔트로피, 재전송 공격 방지)
fn generate_nonce() -> String {
    uuid::Uuid::new_v4().to_string()
}
