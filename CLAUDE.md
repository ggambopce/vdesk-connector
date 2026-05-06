# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Repository Layout

Cargo workspace with a single crate (`vdesk_viewer` was deleted — noVNC browser viewer replaced it):

| Path | Stack | Role |
|------|-------|------|
| `vdesk_agent/` | Rust (async Tokio) | VM agent — session management, transparent TCP proxy to TightVNC :5900 |

## Build Commands

```bash
# From workspace root (VdeskAgentViewer/)
cargo build --release --package vdesk_agent
cargo check                  # fast compile check
cargo clippy                 # linting
```

### Build-time API URL injection

`build.rs` emits `cargo:rustc-env=VDESK_API_URL=<value>` so `env!("VDESK_API_URL")` is baked into the binary. Default is `http://localhost:8080`. Runtime `VDESK_API_URL` env var takes priority over the baked value.

```powershell
# 배포용 빌드
$env:VDESK_API_URL = "https://your-server.com"
cargo build --release --package vdesk_agent
```

## Running

```powershell
# 로컬 개발 — 편의 스크립트 사용 (RELAY_IP=127.0.0.1 자동 설정)
.\run_agent_local.ps1 -ApiUrl "http://localhost:8080" -LogLevel "debug"

# 수동 실행 (VM 공인 IP 또는 ngrok 주소 지정)
$env:AGENT_RELAY_IP = "vm-public-ip"   # Spring이 이 IP:20020으로 TCP 연결
$env:VDESK_API_URL  = "http://localhost:8080"
.\target\release\vdesk_agent.exe
```

**환경변수:**
- `AGENT_RELAY_IP` — Spring에 등록할 릴레이 IP (미설정 시 로컬 사설 IP 자동 감지)
- `AGENT_PORT` — TCP 리스너 포트 (기본: 20020)
- `RUST_LOG` — 로그 레벨 (예: `info`, `vdesk_agent=debug`)

## Deployment

```powershell
# 배포 빌드 (API URL 바이너리 고정 + 산출물 복사)
.\deploy.ps1 -ApiUrl "https://vdesk.example.com"
# → target\release\vdesk_agent.exe 생성
# 주의: deploy.ps1은 삭제된 vdesk_viewer 패키지를 참조하는 스텁이 남아 있음 — 무시할 것

# VM에 에이전트 + TightVNC 설치 (관리자 권한)
.\install_agent.ps1
# → TightVNC 2.8.85 MSI 자동 설치 (port 5900, auth off)
# → C:\VDesk\vdesk_agent.exe 복사
# → 방화벽: TCP 20020 허용, TCP 5900 차단 (내부 전용)
# → 작업 스케줄러: AtLogOn + Highest privileges

.\uninstall_agent.ps1   # 스케줄러 해제, 방화벽 제거, 프로세스 종료, C:\VDesk\ 삭제
```

`install_agent.ps1`과 `vdesk_agent.exe`를 **같은 폴더**에 복사한 뒤 실행. 로그: `C:\VDesk\logs\vdesk_agent.log`.

**TightVNC 필수 레지스트리 설정** (install_agent.ps1이 자동 처리):
- `AllowLoopback=1` — 에이전트가 127.0.0.1:5900 접속 가능하도록
- `UseVncAuthentication=0` — 비밀번호 없음 (5900은 방화벽으로 외부 차단)

## Architecture

### State Machine

`vdesk_agent/src/state.rs`의 `AgentState`가 폴링 루프(`main.rs`)와 TCP 리스너(`server.rs`) 사이에서 `Arc<Mutex<AgentState>>`로 공유됨:

```
Idle ──[activate]──► Pending ──[Spring TCP 연결]──► Streaming ──[세션 종료]──► Idle
```

- **Idle**: 백엔드 폴링 (3초마다 `/api/agent/session/poll`)
- **Pending**: Spring TCP 연결 대기; 1초마다 `check-pending` 폴링 (세션 취소 즉시 감지); 5분 타임아웃 후 Idle 복귀
- **Streaming**: 활성 VNC 세션; 10초마다 세션 heartbeat, 5초마다 파일 전송 체크

### Session Flow

```
1. 에이전트 시작
   → TightVNC :5900 실행 중 (install_agent.ps1으로 설치)
   → TCP :20020 리스너 시작 (server.rs)
   → POST /api/agent/register → deviceKey 수신

2. 브라우저/앱 POST /api/remote/session/create → {sessionKey}

3. 에이전트 poll → activate → Pending 상태

4. noVNC WebSocket → Spring NoVncProxyHandler
   → Spring이 TCP connect(relayIp:20020)

5. server.rs: Pending 상태 확인 후 연결 수락 → Streaming
   → pipe_to_vnc(): 127.0.0.1:5900 연결
   → tokio::io::copy 양방향 파이프 (Spring ↔ TightVNC)

6. RFB 프로토콜 직접 스트리밍 (에이전트는 내용 파싱 안 함)

7. 세션 종료: Spring WS close → TCP close → /api/agent/session/end (최대 3회 재시도) → Idle
```

### TCP Proxy (server.rs)

에이전트는 **완전한 투명 프록시** — RFB 프로토콜을 파싱하지 않고 바이트 그대로 중계:

```rust
tokio::select! {
    _ = tokio::io::copy(&mut spring_r, &mut vnc_w) => {}
    _ = tokio::io::copy(&mut vnc_r, &mut spring_w) => {}
}
```

Pending 상태가 아닌 연결은 즉시 거절 (보안 게이트). 연결 수락 시 `spring_tcp.set_nodelay(true)` 적용.

### File Transfer (Streaming 상태)

```
브라우저 드래그앤드롭 → POST /api/remote/files/upload/{sessionKey}
  → Spring RemoteFileService (인메모리 저장)
  → 에이전트 5초마다 GET /api/agent/files/pending/{deviceKey}
  → GET /api/agent/files/download/{fileId}
  → %USERPROFILE%\Desktop\{filename} 저장
  → POST /api/agent/files/confirm/{fileId}
```

### Relay IP 결정 방식

```rust
let relay_ip = std::env::var("AGENT_RELAY_IP")
    .unwrap_or_else(|_| get_local_ip());
// get_local_ip(): UDP → 8.8.8.8:80 → local_addr() → 사설 IP (192.168.x.x)
// VM 소유 환경: AGENT_RELAY_IP에 VM 공인 IP 설정 필요
```

### Backend API Pattern

모든 백엔드 응답은 `ApiResponse<T>` 래퍼 구조:
```json
{ "code": 200, "message": "...", "result": { ... } }
```
`api.rs`의 `extract<T>()` 함수가 `code != 200`일 때 에러 처리. 모든 요청에 `ngrok-skip-browser-warning: true` 헤더 포함 (ngrok 무료 플랜 인터스티셜 우회).

## Key Files

| File | Purpose |
|------|---------|
| `vdesk_agent/src/main.rs` | 등록 + heartbeat + poll/activate 루프; relay IP 결정; DualLogger 초기화 |
| `vdesk_agent/src/server.rs` | TCP :20020 리스너; Pending 검증 후 TightVNC :5900으로 파이프 |
| `vdesk_agent/src/api.rs` | 백엔드 HTTP 요청/응답 타입 (register, heartbeat, poll, activate, files) |
| `vdesk_agent/src/state.rs` | `AgentState` enum + `SharedState = Arc<Mutex<AgentState>>` |
| `vdesk_agent/build.rs` | 빌드 시 VDESK_API_URL 바이너리 고정 |
| `run_agent_local.ps1` | 로컬 개발용 실행 편의 스크립트 (RELAY_IP=127.0.0.1 자동 설정) |
| `install_agent.ps1` | VM 자동 설치 (TightVNC + 에이전트 + 방화벽 + 스케줄러) |
| `deploy.ps1` | 배포 빌드 통합 스크립트 (API URL 고정 빌드) |

## System Info Collection (Registration)

On first register (`POST /api/agent/register`), the agent auto-collects VM specs and sends them as optional fields. Spring applies **first-write-wins** policy — existing values are never overwritten on re-registration.

| Field | Source |
|-------|--------|
| `cpu` | `sysinfo::System::cpus()[0].brand()` |
| `ram` | `sysinfo::System::total_memory()` / 1 GB |
| `disk` | `sysinfo::Disks` total space sum / 1 GB |
| `os` | `System::name()` + `System::os_version()` |
| `osBit` | `cfg!(target_pointer_width="64")` → "64bit" |
| `gpu` | `wmic path win32_VideoController get name` (Windows only) |
| `internalIp` | `get_local_ip()` (UDP trick → local interface) |
| `publicIp` | `GET https://api.ipify.org` (3s timeout, fallback: omit) |

All fields wrapped in `Option<>` — collection failure → `None` → field omitted from JSON (`skip_serializing_if`).

## Key Conventions

- 소스 주석과 식별자 일부는 한국어 (의도된 것).
- `localBox` UUID는 `%TEMP%\vdesk_agent_id`에 저장 — 재시작해도 동일 deviceKey 유지. 삭제 시 새 등록.
- **DualLogger**: stderr + `<exe_dir>/logs/vdesk_agent.log` 동시 출력. 파일은 append, 로테이션 없음.
- heartbeat 10초, 백엔드 타임아웃 15초 — heartbeat 주기 변경 시 `SessionTimeoutScheduler` 연동 확인.
- 테스트 없음 — 테스트 코드는 삭제됨. 변경 검증은 직접 실행으로 수행.
