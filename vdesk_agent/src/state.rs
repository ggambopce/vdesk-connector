//! 에이전트 공유 상태 — 백엔드 폴링과 원격 제어 세션을 분리합니다.
//!
//! 상태 전환 다이어그램:
//!
//! ┌─ 백엔드 폴링 (main.rs) ───────────────────────────────────────────┐
//! │  Idle ──[activate 성공]──► Pending                                │
//! │  Idle 상태일 때만 poll/heartbeat 실행                              │
//! └──────────────────────────────────────────────────────────────────┘
//! ┌─ 원격 제어 (server.rs / session.rs) ──────────────────────────────┐
//! │  Pending ──[TCP 핸드쉐이크 성공]──► Streaming                     │
//! │  Streaming ──[세션 정상 종료]──► Idle                             │
//! │  Pending ──[핸드쉐이크 실패]──► Idle  (재폴링 허용)               │
//! └──────────────────────────────────────────────────────────────────┘

use std::sync::{Arc, Mutex};

/// 에이전트 동작 상태
#[derive(Clone, Debug, PartialEq)]
pub enum AgentState {
    /// 백엔드 등록 완료, 새 세션 폴링 대기 중
    Idle,
    /// activate API 완료, 뷰어 TCP 연결 대기 중
    Pending { session_key: String },
    /// 뷰어 연결·핸드쉐이크 완료, 화면 스트리밍 중
    Streaming { session_key: String },
}

impl AgentState {
    /// 백엔드 poll을 허용하는 상태인지 확인
    pub fn is_idle(&self) -> bool {
        matches!(self, AgentState::Idle)
    }

    /// 현재 세션 키 (Pending/Streaming 상태에서만 Some)
    pub fn session_key(&self) -> Option<&str> {
        match self {
            AgentState::Pending { session_key }
            | AgentState::Streaming { session_key } => Some(session_key),
            AgentState::Idle => None,
        }
    }
}

/// 백엔드 폴링 태스크와 원격 제어 태스크가 공유하는 상태
pub type SharedState = Arc<Mutex<AgentState>>;

pub fn new_state() -> SharedState {
    Arc::new(Mutex::new(AgentState::Idle))
}
