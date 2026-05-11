//! 에이전트 제어 채널 WebSocket 클라이언트
//!
//! 서버의 /api/agent/ws/{deviceKey} 에 상시 연결하여
//! clipboard / file_ready 메시지를 수신하고 즉시 처리합니다.
//! HTTP 폴링 없이 < 50ms 지연으로 클립보드/파일 이벤트를 처리합니다.

use std::time::Duration;
use anyhow::Result;
use futures_util::StreamExt;
use tokio_tungstenite::tungstenite::Message;

use crate::api;

/// 제어 채널 무한 재연결 루프 (태스크로 spawn)
pub async fn run(device_key: String) {
    let mut backoff = Duration::from_secs(1);
    loop {
        log::info!("[ctrl-ws] 연결 시도: deviceKey={}", device_key);
        match connect_and_handle(&device_key).await {
            Ok(()) => {
                log::info!("[ctrl-ws] 정상 종료");
                backoff = Duration::from_secs(1);
            }
            Err(e) => {
                log::warn!("[ctrl-ws] 연결 실패: {:?} — {}초 후 재시도", e, backoff.as_secs());
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

async fn connect_and_handle(device_key: &str) -> Result<()> {
    let base = api::base_url();
    // http(s):// → ws(s)://
    let ws_base = base
        .replacen("https://", "wss://", 1)
        .replacen("http://",  "ws://",  1);
    let url = format!("{}/api/agent/ws/{}", ws_base, device_key);

    let (ws_stream, _) = tokio_tungstenite::connect_async(&url).await?;
    log::info!("[ctrl-ws] 연결 성공: {}", url);

    let (_, mut read) = ws_stream.split();

    while let Some(msg) = read.next().await {
        match msg? {
            Message::Text(json) => {
                if let Err(e) = handle_message(&json).await {
                    log::warn!("[ctrl-ws] 메시지 처리 실패: {:?}", e);
                }
            }
            Message::Close(_) => {
                log::info!("[ctrl-ws] 서버 연결 종료");
                break;
            }
            _ => {}
        }
    }
    Ok(())
}

async fn handle_message(json: &str) -> Result<()> {
    let v: serde_json::Value = serde_json::from_str(json)?;
    match v["type"].as_str() {
        Some("clipboard") => {
            let text = v["text"].as_str().unwrap_or("");
            if text.is_empty() { return Ok(()); }
            log::info!("[ctrl-ws] clipboard 수신: {} chars", text.len());
            #[cfg(windows)]
            unsafe {
                win32::set_clipboard_text(text);
                // 짧은 딜레이 후 Ctrl+V — 클립보드 설정이 완료된 후 전송
                std::thread::sleep(std::time::Duration::from_millis(50));
                win32::send_ctrl_v();
            }
        }
        Some("file_ready") => {
            let file_id  = v["fileId"].as_str().unwrap_or("").to_string();
            let filename = v["filename"].as_str().unwrap_or("file").to_string();
            if file_id.is_empty() { return Ok(()); }
            log::info!("[ctrl-ws] file_ready 수신: {} ({})", filename, file_id);
            // 바이너리는 기존 HTTP 다운로드 경로 재사용
            match api::download_file(&file_id).await {
                Ok(data) => {
                    let dest = desktop_path(&filename);
                    if std::fs::write(&dest, &data).is_ok() {
                        log::info!("[ctrl-ws] 파일 저장: {:?}", dest);
                        let _ = api::confirm_file(&file_id).await;
                    } else {
                        log::warn!("[ctrl-ws] 파일 저장 실패: {:?}", dest);
                    }
                }
                Err(e) => log::warn!("[ctrl-ws] 파일 다운로드 실패: {:?}", e),
            }
        }
        other => log::debug!("[ctrl-ws] 알 수 없는 메시지 타입: {:?}", other),
    }
    Ok(())
}

fn desktop_path(filename: &str) -> std::path::PathBuf {
    let base = std::env::var("USERPROFILE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("."));
    base.join("Desktop").join(filename)
}

// ── Win32 FFI (외부 crate 없이 raw extern) ────────────────────────────────────

#[cfg(windows)]
mod win32 {
    #[link(name = "user32")]
    extern "system" {
        fn OpenClipboard(hWnd: *mut std::ffi::c_void) -> i32;
        fn EmptyClipboard() -> i32;
        fn SetClipboardData(uFormat: u32, hMem: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
        fn CloseClipboard() -> i32;
        fn keybd_event(bVk: u8, bScan: u8, dwFlags: u32, dwExtraInfo: usize);
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GlobalAlloc(uFlags: u32, dwBytes: usize) -> *mut std::ffi::c_void;
        fn GlobalLock(hMem: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
        fn GlobalUnlock(hMem: *mut std::ffi::c_void) -> i32;
    }

    const CF_UNICODETEXT:  u32 = 13;
    const GMEM_MOVEABLE:   u32 = 0x0002;
    const VK_CONTROL:       u8 = 0x11;
    const VK_V:             u8 = 0x56;
    const KEYEVENTF_KEYUP: u32 = 0x0002;

    pub unsafe fn set_clipboard_text(text: &str) {
        let utf16: Vec<u16> = text.encode_utf16().chain(Some(0u16)).collect();
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return;
        }
        EmptyClipboard();
        let hmem = GlobalAlloc(GMEM_MOVEABLE, utf16.len() * 2);
        if !hmem.is_null() {
            let ptr = GlobalLock(hmem) as *mut u16;
            if !ptr.is_null() {
                std::ptr::copy_nonoverlapping(utf16.as_ptr(), ptr, utf16.len());
                GlobalUnlock(hmem);
                SetClipboardData(CF_UNICODETEXT, hmem);
            }
        }
        CloseClipboard();
    }

    pub unsafe fn send_ctrl_v() {
        keybd_event(VK_CONTROL, 0, 0, 0);
        keybd_event(VK_V, 0, 0, 0);
        keybd_event(VK_V, 0, KEYEVENTF_KEYUP, 0);
        keybd_event(VK_CONTROL, 0, KEYEVENTF_KEYUP, 0);
    }
}
