//! VDesk 백엔드 HTTP API 클라이언트 — 뷰어(사용자 PC) 측
//!
//! 인증: POST /api/auth/login → AT 쿠키 (reqwest cookie_store로 자동 관리)

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

pub fn base_url() -> String {
    std::env::var("VDESK_API_URL").unwrap_or_else(|_| "http://localhost:8080".to_string())
}

// ─── 공통 응답 래퍼 ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ApiResponse<T> {
    code: Option<i32>,
    message: Option<String>,
    result: Option<T>,
}

fn extract<T: for<'de> Deserialize<'de>>(resp: reqwest::blocking::Response) -> Result<T> {
    let status = resp.status();
    let body: ApiResponse<T> = resp.json()?;
    if body.code.unwrap_or(200) != 200 {
        bail!("API error ({}): {:?}", status, body.message);
    }
    body.result
        .ok_or_else(|| anyhow::anyhow!("API returned no result ({}): {:?}", status, body.message))
}

// ─── 세션 유지 클라이언트 ─────────────────────────────────────────────────────

/// AT 쿠키를 자동으로 유지하는 HTTP 클라이언트 래퍼
pub struct ViewerClient {
    inner: reqwest::blocking::Client,
}

impl ViewerClient {
    fn new() -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .cookie_store(true)
            .build()?;
        Ok(Self { inner: client })
    }
}

// ─── 인증 ─────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct LoginRequest<'a> {
    email: &'a str,
    password: &'a str,
}

/// 이메일/비밀번호 로그인 → AT 쿠키 획득 (내부에 저장)
pub fn login(email: &str, password: &str) -> Result<ViewerClient> {
    let client = ViewerClient::new()?;
    let url = format!("{}/api/auth/login", base_url());
    let req = LoginRequest { email, password };
    let resp = client.inner.post(&url).json(&req).send()?;
    let status = resp.status();
    // 로그인 응답 data=null이 정상 (쿠키만 받음)
    let body: serde_json::Value = resp.json()?;
    if !status.is_success() {
        bail!(
            "로그인 실패 ({}): {}",
            status,
            body.get("message").and_then(|v| v.as_str()).unwrap_or("unknown")
        );
    }
    Ok(client)
}

// ─── 디바이스 목록 ────────────────────────────────────────────────────────────

#[derive(Deserialize, Debug, Clone)]
pub struct DeviceInfo {
    #[serde(rename = "deviceId")]
    pub device_id: u64,
    #[serde(rename = "deviceName")]
    pub device_name: String,
    #[serde(rename = "hostStatus")]
    pub host_status: String,
}

#[derive(Deserialize)]
struct DeviceListData {
    items: Vec<DeviceInfo>,
}

pub fn list_devices(client: &ViewerClient) -> Result<Vec<DeviceInfo>> {
    let url = format!("{}/api/user/device/list", base_url());
    let resp = client.inner.get(&url).send()?;
    let data: DeviceListData = extract(resp)?;
    Ok(data.items)
}

// ─── 디바이스 탐색 (미연결 ONLINE 디바이스) ──────────────────────────────────

#[derive(Deserialize, Debug, Clone)]
pub struct DiscoverDevice {
    #[serde(rename = "deviceId")]
    pub device_id: u64,
    #[serde(rename = "deviceKey")]
    pub device_key: String,
    #[serde(rename = "hostName")]
    pub host_name: String,
    #[serde(rename = "osType")]
    pub os_type: String,
}

pub fn discover_devices(client: &ViewerClient) -> Result<Vec<DiscoverDevice>> {
    let url = format!("{}/api/user/device/discover", base_url());
    let resp = client.inner.get(&url).send()?;
    extract(resp)
}

// ─── 디바이스 연결 ────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct LinkDeviceRequest<'a> {
    #[serde(rename = "deviceKey")]
    device_key: &'a str,
}

pub fn link_device(client: &ViewerClient, device_key: &str) -> Result<DeviceInfo> {
    let url = format!("{}/api/user/device/link", base_url());
    let req = LinkDeviceRequest { device_key };
    let resp = client.inner.post(&url).json(&req).send()?;
    extract(resp)
}

// ─── 세션 생성 ────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct CreateSessionRequest {
    #[serde(rename = "deviceId")]
    device_id: u64,
}

#[derive(Deserialize, Debug, Clone)]
pub struct SessionInfo {
    #[serde(rename = "sessionId")]
    pub session_id: u64,
    #[serde(rename = "sessionKey")]
    pub session_key: String,
    pub status: String,
}

pub fn create_session(client: &ViewerClient, device_id: u64) -> Result<SessionInfo> {
    let url = format!("{}/api/remote/sessions", base_url());
    let req = CreateSessionRequest { device_id };
    let resp = client.inner.post(&url).json(&req).send()?;
    extract(resp)
}

// ─── Relay 정보 조회 (세션이 RUNNING이 될 때까지 폴링) ──────────────────────

#[derive(Deserialize, Debug, Clone)]
pub struct RelayInfo {
    #[serde(rename = "relayIp")]
    pub relay_ip: String,
    #[serde(rename = "relayPort")]
    pub relay_port: u16,
    #[serde(rename = "sessionKey")]
    pub session_key: String,
    pub status: String,
}

pub fn get_relay(client: &ViewerClient, session_key: &str) -> Result<RelayInfo> {
    let url = format!(
        "{}/api/agent/sessions/relay?sessionKey={}",
        base_url(),
        session_key
    );
    let resp = client.inner.get(&url).send()?;
    extract(resp)
}

/// RUNNING 상태가 될 때까지 반복 폴링 (최대 60초)
pub fn wait_for_running(client: &ViewerClient, session_key: &str) -> Result<RelayInfo> {
    let max_attempts = 60;
    for attempt in 1..=max_attempts {
        match get_relay(client, session_key) {
            Ok(relay) if relay.status == "RUNNING" => return Ok(relay),
            Ok(relay) => {
                log::info!(
                    "[api] 세션 대기 중 ({}/{}): status={}",
                    attempt,
                    max_attempts,
                    relay.status
                );
            }
            Err(e) => {
                log::debug!("[api] relay 조회 오류: {:?}", e);
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    bail!("세션이 60초 내에 RUNNING 상태가 되지 않았습니다")
}

// ─── 세션 종료 ────────────────────────────────────────────────────────────────

pub fn end_session(client: &ViewerClient, session_id: u64) -> Result<()> {
    let url = format!("{}/api/remote/sessions/{}/end", base_url(), session_id);
    let resp = client.inner.post(&url).send()?;
    let status = resp.status();
    if !status.is_success() {
        bail!("End session failed: {}", status);
    }
    Ok(())
}
