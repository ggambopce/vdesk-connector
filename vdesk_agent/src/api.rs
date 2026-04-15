//! VDesk 백엔드 HTTP API 클라이언트 (async)
//! 에이전트(VM)가 백엔드와 통신하는 모든 HTTP 요청을 담당합니다.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

/// 백엔드 서버 기본 URL (환경 변수 또는 기본값)
pub fn base_url() -> String {
    std::env::var("VDESK_API_URL").unwrap_or_else(|_| "http://localhost:8080".to_string())
}

// ─── 요청/응답 타입 ───────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct RegisterRequest {
    #[serde(rename = "localBox")]
    pub local_box: String,
    #[serde(rename = "agentName")]
    pub agent_name: String,
    #[serde(rename = "osType")]
    pub os_type: String,
    #[serde(rename = "appVersion")]
    pub app_version: String,
    #[serde(rename = "relayIp")]
    pub relay_ip: String,
    #[serde(rename = "relayPort")]
    pub relay_port: u16,
}

#[derive(Deserialize, Debug)]
pub struct RegisterData {
    #[serde(rename = "agentId")]
    pub agent_id: u64,
    #[serde(rename = "deviceId")]
    pub device_id: u64,
    #[serde(rename = "deviceKey")]
    pub device_key: String,
    #[serde(rename = "agentStatus")]
    pub agent_status: String,
}

#[derive(Serialize)]
pub struct HeartbeatRequest {
    #[serde(rename = "deviceKey")]
    pub device_key: String,
    #[serde(rename = "relayIp")]
    pub relay_ip: String,
    #[serde(rename = "relayPort")]
    pub relay_port: u16,
    #[serde(rename = "appVersion")]
    pub app_version: String,
    #[serde(rename = "agentStatus")]
    pub agent_status: String,
    #[serde(rename = "sessionAcceptable")]
    pub session_acceptable: bool,
    #[serde(rename = "currentSessionKey")]
    pub current_session_key: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct HeartbeatData {
    #[serde(rename = "agentStatus")]
    pub agent_status: String,
}

#[derive(Serialize)]
pub struct PollRequest {
    #[serde(rename = "deviceKey")]
    pub device_key: String,
}

#[derive(Deserialize, Debug)]
pub struct PollData {
    #[serde(rename = "hasPendingSession")]
    pub has_pending_session: bool,
    #[serde(rename = "sessionId")]
    pub session_id: Option<u64>,
    #[serde(rename = "sessionKey")]
    pub session_key: Option<String>,
    pub status: Option<String>,
    #[serde(rename = "connectExpireAt")]
    pub connect_expire_at: Option<String>,
}

/// 구형 activate (body에 sessionKey) — 하위 호환 유지
#[derive(Serialize)]
pub struct ActivateRequest {
    #[serde(rename = "deviceKey")]
    pub device_key: String,
    #[serde(rename = "sessionKey")]
    pub session_key: String,
}

/// 신규 activate — 경로 변수에 sessionKey, body에는 deviceKey만
#[derive(Serialize)]
pub struct ActivateByPathRequest {
    #[serde(rename = "deviceKey")]
    pub device_key: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct ActivateByPathData {
    #[serde(rename = "sessionId")]
    pub session_id: u64,
    #[serde(rename = "sessionKey")]
    pub session_key: String,
    pub status: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct SessionData {
    #[serde(rename = "sessionId")]
    pub session_id: u64,
    #[serde(rename = "sessionKey")]
    pub session_key: String,
    pub status: String,
    #[serde(rename = "relayIp")]
    pub relay_ip: Option<String>,
    #[serde(rename = "relayPort")]
    pub relay_port: Option<u16>,
}

#[derive(Serialize)]
pub struct EndRequest {
    #[serde(rename = "deviceKey")]
    pub device_key: String,
    #[serde(rename = "sessionKey")]
    pub session_key: String,
}

#[derive(Serialize)]
pub struct VerifyConnectRequest {
    #[serde(rename = "deviceKey")]
    pub device_key: String,
    #[serde(rename = "connectToken")]
    pub connect_token: String,
    #[serde(rename = "viewerNonce")]
    pub viewer_nonce: String,
    #[serde(rename = "viewerIp")]
    pub viewer_ip: String,
}

#[derive(Deserialize, Debug)]
pub struct VerifyConnectData {
    #[serde(rename = "sessionId")]
    pub session_id: u64,
    #[serde(rename = "sessionKey")]
    pub session_key: String,
    pub status: String,
}

#[derive(Serialize)]
pub struct SessionHeartbeatRequest {
    #[serde(rename = "deviceKey")]
    pub device_key: String,
    #[serde(rename = "sessionKey")]
    pub session_key: String,
    #[serde(rename = "bytesOut")]
    pub bytes_out: u64,
    #[serde(rename = "bytesIn")]
    pub bytes_in: u64,
}

#[derive(Deserialize, Debug)]
pub struct SessionHeartbeatData {
    pub status: String,
    /// true이면 에이전트는 세션을 즉시 종료해야 함 (백엔드 판정)
    #[serde(rename = "shouldTerminate", default)]
    pub should_terminate: bool,
}

// ─── 백엔드 래퍼 응답 역직렬화 ───────────────────────────────────────────────

#[derive(Deserialize)]
struct ApiResponse<T> {
    code: Option<i32>,
    message: Option<String>,
    result: Option<T>,
}

async fn extract<T: for<'de> Deserialize<'de>>(resp: reqwest::Response) -> Result<T> {
    let status = resp.status();
    let body: ApiResponse<T> = resp.json().await?;
    if body.code.unwrap_or(200) != 200 {
        bail!("API error ({}): {:?}", status, body.message);
    }
    body.result
        .ok_or_else(|| anyhow::anyhow!("API returned no result ({}): {:?}", status, body.message))
}

// ─── API 함수 ────────────────────────────────────────────────────────────────

pub async fn register(req: &RegisterRequest) -> Result<RegisterData> {
    let url = format!("{}/api/agent/register", base_url());
    let resp = reqwest::Client::new().post(&url).json(req).send().await?;
    extract(resp).await
}

pub async fn heartbeat(req: &HeartbeatRequest) -> Result<()> {
    let url = format!("{}/api/agent/heartbeat", base_url());
    let resp = reqwest::Client::new().post(&url).json(req).send().await?;
    let status = resp.status();
    if !status.is_success() {
        bail!("Heartbeat failed: {}", status);
    }
    Ok(())
}

pub async fn poll(req: &PollRequest) -> Result<PollData> {
    let url = format!("{}/api/agent/session/poll", base_url());
    let resp = reqwest::Client::new().post(&url).json(req).send().await?;
    extract(resp).await
}

/// 신규 activate — sessionKey를 경로 변수로, body에는 deviceKey만
pub async fn activate(device_key: &str, session_key: &str) -> Result<ActivateByPathData> {
    let url = format!("{}/api/agent/session/activate/{}", base_url(), session_key);
    let req = ActivateByPathRequest { device_key: device_key.to_string() };
    let resp = reqwest::Client::new().post(&url).json(&req).send().await?;
    extract(resp).await
}

/// 구형 activate (하위 호환 — 사용하지 않으나 보존)
#[allow(dead_code)]
pub async fn activate_legacy(req: &ActivateRequest) -> Result<SessionData> {
    let url = format!("{}/api/agent/sessions/activate", base_url());
    let resp = reqwest::Client::new().post(&url).json(req).send().await?;
    extract(resp).await
}

/// Viewer TCP 접속 검증 — PENDING → RUNNING 전환
pub async fn verify_connect(
    session_key: &str,
    device_key: &str,
    connect_token: &str,
    viewer_nonce: &str,
    viewer_ip: &str,
) -> Result<VerifyConnectData> {
    let url = format!("{}/api/agent/sessions/verify-connect/{}", base_url(), session_key);
    let req = VerifyConnectRequest {
        device_key: device_key.to_string(),
        connect_token: connect_token.to_string(),
        viewer_nonce: viewer_nonce.to_string(),
        viewer_ip: viewer_ip.to_string(),
    };
    let resp = reqwest::Client::new().post(&url).json(&req).send().await?;
    extract(resp).await
}

/// 에이전트 세션 heartbeat — 스트리밍 중 주기 호출
/// 응답의 shouldTerminate가 true이면 세션 루프를 break해야 함
pub async fn session_heartbeat(req: &SessionHeartbeatRequest) -> Result<SessionHeartbeatData> {
    let url = format!("{}/api/agent/session/heartbeat", base_url());
    let resp = reqwest::Client::new().post(&url).json(req).send().await?;
    extract(resp).await
}

#[derive(Deserialize, Debug)]
pub struct CheckPendingData {
    #[serde(rename = "shouldReset", default)]
    pub should_reset: bool,
}

/// Pending 상태 유효성 확인 — 세션이 취소됐으면 shouldReset=true
pub async fn check_pending_session(req: &EndRequest) -> Result<CheckPendingData> {
    let url = format!("{}/api/agent/session/check-pending", base_url());
    let resp = reqwest::Client::new().post(&url).json(req).send().await?;
    extract(resp).await
}

pub async fn end_session(req: &EndRequest) -> Result<()> {
    let url = format!("{}/api/agent/session/end", base_url());
    let resp = reqwest::Client::new().post(&url).json(req).send().await?;
    let status = resp.status();
    if !status.is_success() {
        bail!("End session failed: {}", status);
    }
    Ok(())
}
