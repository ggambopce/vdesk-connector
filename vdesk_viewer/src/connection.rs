//! TCP 연결 + sessionKey 핸드쉐이크

use anyhow::Result;
use hbb_common::{log, tcp::FramedStream};

/// 에이전트에 TCP 연결하고 sessionKey로 핸드쉐이크합니다.
/// 핸드쉐이크 프로토콜:
///   1. Viewer → Agent: sessionKey bytes
///   2. Agent → Viewer: [1]=OK, [0]=DENY
pub async fn connect(relay_ip: &str, relay_port: u16, session_key: &str) -> Result<FramedStream> {
    let addr = format!("{}:{}", relay_ip, relay_port);
    log::info!("[conn] 에이전트 연결 중: {}", addr);

    let mut stream = FramedStream::new(addr.as_str(), None, 10_000).await?;
    log::info!("[conn] TCP 연결 성공");

    // sessionKey 전송
    stream
        .send_bytes(bytes::Bytes::copy_from_slice(session_key.as_bytes()))
        .await?;

    // 응답 수신
    let bytes = match stream.next().await {
        Some(Ok(b)) => b,
        Some(Err(e)) => anyhow::bail!("핸드쉐이크 오류: {:?}", e),
        None => anyhow::bail!("연결 종료"),
    };

    if bytes.get(0) == Some(&1u8) {
        log::info!("[conn] 핸드쉐이크 성공");
        Ok(stream)
    } else {
        anyhow::bail!("에이전트가 세션을 거부했습니다")
    }
}
