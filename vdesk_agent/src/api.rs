//! VDesk 백엔드 HTTP API 클라이언트 (async)
//! 에이전트(VM)가 백엔드와 통신하는 모든 HTTP 요청을 담당합니다.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

/// 백엔드 서버 기본 URL
/// 우선순위: 런타임 VDESK_API_URL 환경변수 → 빌드 시 고정값 (build.rs에서 주입)
pub fn base_url() -> String {
    std::env::var("VDESK_API_URL").unwrap_or_else(|_| env!("VDESK_API_URL").to_string())
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
    #[serde(rename = "deviceKey")]
    pub device_key: String,
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

#[derive(Serialize)]
pub struct PollRequest {
    #[serde(rename = "deviceKey")]
    pub device_key: String,
}

#[derive(Deserialize, Debug)]
pub struct PollData {
    #[serde(rename = "hasPendingSession")]
    pub has_pending_session: bool,
    #[serde(rename = "sessionKey")]
    pub session_key: Option<String>,
}

#[derive(Serialize)]
pub struct ActivateByPathRequest {
    #[serde(rename = "deviceKey")]
    pub device_key: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct ActivateByPathData {
    #[serde(rename = "sessionKey")]
    pub session_key: String,
    pub status: String,
}

#[derive(Serialize)]
pub struct EndRequest {
    #[serde(rename = "deviceKey")]
    pub device_key: String,
    #[serde(rename = "sessionKey")]
    pub session_key: String,
}

#[derive(Deserialize, Debug)]
pub struct CheckPendingData {
    #[serde(rename = "shouldReset", default)]
    pub should_reset: bool,
}

// ─── HTTP 클라이언트 ─────────────────────────────────────────────────────────
/// ngrok 무료 플랜은 비브라우저 요청에 HTML 인터스티셜을 반환하므로
/// ngrok-skip-browser-warning 헤더를 모든 요청에 포함합니다.
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .default_headers({
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert("ngrok-skip-browser-warning", reqwest::header::HeaderValue::from_static("true"));
            headers
        })
        .build()
        .unwrap_or_default()
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
    let resp = client().post(&url).json(req).send().await?;
    extract(resp).await
}

pub async fn heartbeat(req: &HeartbeatRequest) -> Result<()> {
    let url = format!("{}/api/agent/heartbeat", base_url());
    let resp = client().post(&url).json(req).send().await?;
    let status = resp.status();
    if !status.is_success() {
        bail!("Heartbeat failed: {}", status);
    }
    Ok(())
}

pub async fn poll(req: &PollRequest) -> Result<PollData> {
    let url = format!("{}/api/agent/session/poll", base_url());
    let resp = client().post(&url).json(req).send().await?;
    extract(resp).await
}

pub async fn activate(device_key: &str, session_key: &str) -> Result<ActivateByPathData> {
    let url = format!("{}/api/agent/session/activate/{}", base_url(), session_key);
    let req = ActivateByPathRequest { device_key: device_key.to_string() };
    let resp = client().post(&url).json(&req).send().await?;
    extract(resp).await
}

/// Pending 상태 유효성 확인 — 세션이 취소됐으면 shouldReset=true
pub async fn check_pending_session(req: &EndRequest) -> Result<CheckPendingData> {
    let url = format!("{}/api/agent/session/check-pending", base_url());
    let resp = client().post(&url).json(req).send().await?;
    extract(resp).await
}

pub async fn end_session(req: &EndRequest) -> Result<()> {
    let url = format!("{}/api/agent/session/end", base_url());
    let resp = client().post(&url).json(req).send().await?;
    let status = resp.status();
    if !status.is_success() {
        bail!("End session failed: {}", status);
    }
    Ok(())
}
