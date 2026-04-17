# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Repository Layout

This is a Cargo workspace with two crates:

| Path | Stack | Role |
|------|-------|------|
| `vdesk_agent/` | Rust (async Tokio) | Screen-streaming agent — TCP listener, VP9 capture, input injection |
| `vdesk_viewer/` | Rust (sync + Tokio thread) | Remote viewer — winit window, VP9 decode, input capture |

The `.vs/` directory is a Visual Studio workspace artifact. No VS project is wired up.

## Build Commands

```bash
# From workspace root (VDeskAgentViewer/)
cargo build --release --package vdesk_agent
cargo build --release --package vdesk_viewer
cargo check          # whole workspace
```

VP9 requires **libvpx via vcpkg** (`x64-windows-static` triplet). Set `VCPKG_ROOT` before building if vcpkg is not at `C:\vcpkg`.

### Build-time API URL injection

Both `build.rs` files emit `cargo:rustc-env=VDESK_API_URL=<value>` so `env!("VDESK_API_URL")` in source is baked into the binary at compile time. Runtime `VDESK_API_URL` env var takes priority (useful for dev).

```powershell
# 배포용 빌드 (URL을 바이너리에 고정)
.\deploy.ps1 -ApiUrl "https://your-server.com"
```

`deploy.ps1` builds both packages with `$env:VDESK_API_URL` set, then copies `vdesk_viewer.exe` to `../vdesk/src/main/resources/static/downloads/` for Spring Boot static serving.

## Running — Direct Mode (no backend required)

```powershell
# Terminal 1 — agent (same PC)
.\run_agent_direct.ps1                          # default: port 20020, key "direct"
.\run_agent_direct.ps1 -Port 20021 -Key mykey  # custom port/key

# Terminal 2 — viewer
.\run_viewer_direct.ps1                              # 127.0.0.1:20020
.\run_viewer_direct.ps1 -AgentHost "192.168.1.100"  # remote agent
.\run_viewer_direct.ps1 -AgentHost "192.168.1.100" -Port 20021 -Key mykey
```

Script params: `run_agent_direct.ps1 [-Key str] [-Port str] [-LogLevel str]`; `run_viewer_direct.ps1 [-AgentHost str] [-Port str] [-Key str] [-LogLevel str]`

Relevant env vars:
- `VDESK_DIRECT=1` — skip backend, use direct TCP
- `VDESK_DIRECT_KEY` — shared session key (default: `"direct"`)
- `VDESK_DIRECT_HOST` / `VDESK_DIRECT_PORT` — viewer-side target
- `AGENT_PORT` — agent listen port (default: 20020)
- `AGENT_NO_INJECT=1` — disable `SendInput` (streaming-only test)
- `VDESK_MOUSE_GLOBAL=1` — send mouse as virtual-screen absolute coords (0x07) even in backend mode
- `RUST_LOG` — log verbosity (e.g. `info`, `debug`, `vdesk_agent=trace`)

## Running — Backend Mode

```powershell
# Agent
.\run_agent_local.ps1           # sets AGENT_RELAY_IP=127.0.0.1

# Viewer
.\run_viewer_local.ps1 -Email "user@example.com" -Password "pass" -Device 42
# or manually:
$env:VDESK_API_URL="http://localhost:8080"
$env:VDESK_EMAIL="user@example.com"
$env:VDESK_PASSWORD="pass"
.\target\release\vdesk_viewer.exe --device 42
```

Script params: `run_agent_local.ps1 [-ApiUrl str] [-Port str] [-LogLevel str]`; `run_viewer_local.ps1 [-ApiUrl str] [-Email str] [-Password str] [-Device str] [-LogLevel str]`

**Viewer interactive device selection** (when `--device` is omitted): viewer lists linked devices → prompts for ID. If no linked devices, it calls the discover endpoint, lists unlinked agents, and calls `link_device` before connecting. Credentials can also be entered interactively if env vars are not set.

## Deployment Scripts

```powershell
# VM에 에이전트 설치 (관리자 권한 — UAC 자동 처리)
.\install_agent.ps1
# → C:\VDesk\vdesk_agent.exe 복사, 방화벽 TCP 20020, 작업 스케줄러 AtLogOn+Highest, 즉시 시작

.\uninstall_agent.ps1   # 스케줄러 해제, 방화벽 제거, 프로세스 종료, C:\VDesk\ 삭제
```

`install_agent.ps1`과 `vdesk_agent.exe`를 **같은 폴더**에 복사한 뒤 실행. 로그는 `C:\VDesk\logs\vdesk_agent.log` (append).

## Distribution (viewer only)

`target/release/vdesk_viewer.exe` is a single-file distribution (~7 MB). Depends on `VCRUNTIME140.dll` (present on most Windows 10/11 systems). No other files needed.

Port 20020 TCP inbound must be open on the agent PC (handled automatically by `install_agent.ps1`):
```powershell
New-NetFirewallRule -DisplayName "VDesk Agent" -Direction Inbound -Protocol TCP -LocalPort 20020 -Action Allow
```

## Architecture

### State Machine (agent)

`vdesk_agent/src/state.rs` defines `AgentState` shared between the polling loop (`main.rs`) and the TCP listener (`server.rs`) via `Arc<Mutex<AgentState>>`:

```
Idle ──[activate]──► Pending ──[handshake OK]──► Streaming ──[session end]──► Idle
                              └─[handshake fail]──► Pending (stays, allows viewer retry)
```

- Poll/heartbeat only runs when `Idle`
- `listen_loop` only accepts connections when `Pending`
- Handshake failure keeps agent in `Pending` — the viewer retries with the same or correct credentials
- `listen_loop_direct` runs sessions inline (no spawn) so the accept loop blocks until the current session fully terminates — this prevents DXGI handle conflicts on reconnect

### Session Flow (backend mode)

```
Agent → POST /api/agent/register          → deviceKey  (persists localBox in %TEMP%\vdesk_agent_id)
Agent → POST /api/agent/heartbeat         (10s loop)
Agent → POST /api/agent/session/poll      (3s Idle / 1s Pending loop)
Browser → POST /api/remote/session/create → {sessionKey, connectToken, relayIp, relayPort}
Agent poll → POST /api/agent/session/activate/{sessionKey} → status=PENDING
User clicks "뷰어 실행하기" → vdesk_viewer.exe launched via vdesk:// URI
Viewer → TCP connect to relayIp:20020
Viewer → JSON handshake {sessionKey, connectToken, viewerNonce}
Agent → POST /api/agent/sessions/verify-connect/{sessionKey} → status=RUNNING
Browser poll detects RUNNING, heartbeat loop starts
```

**Poll interval**: `poll_idle = 3s` (Idle 상태 — ngrok free tier 40 req/min 이내 유지), `poll_pending = 1s` (Pending 상태 — 세션 취소 즉시 감지).

**Viewer heartbeat**: URI 모드 뷰어는 JWT 없이 `POST /api/remote/session/viewer/heartbeat-by-key/{sessionId}` 호출 (sessionKey를 capability token으로 사용). 기존 `/viewer/heartbeat/{sessionId}`는 JWT 필요.

**TCP handshake format** (viewer → agent, sent as FramedStream bytes):
```json
{"sessionKey":"...","connectToken":"ct_...","viewerNonce":"nonce-uuid"}
```
Agent responds: `0x01` = OK (proceed to stream), `0x00` = rejected.

### Video Pipeline (agent)

**Capture state machine** (`services/video.rs`):

```
DXGI 초기 시도 (3회, 200ms 간격)
  ├─ 성공 → DXGI 모드
  │    캡처 오류 5회 연속 → DXGI 핸들 drop → 500ms 대기 → DXGI 재초기화
  │                        ├─ 성공 → DXGI 모드 복귀
  │                        └─ 실패 → GDI 모드 (30초마다 DXGI 복귀 시도)
  └─ 실패 → GDI 모드 (30초마다 DXGI 복귀 시도)
```

**DXGI 모드** (primary):
```
DXGI Desktop Duplication (BGRA, GPU→CPU)
  → DirtyRects filter: only changed regions copied to CPU buffer
  → bgra_to_i420 / bgra_to_i420_rects (BT.601, partial update)
  → VP9 encode (libvpx, C wrapper vpx_wrap.c)
  → try_send → session loop → FramedStream
```

**GDI 모드** (fallback — RDP 세션, VM 드라이버 불안정 환경):
```
GDI BitBlt: GetDC → CreateCompatibleBitmap → BitBlt(SRCCOPY) → GetDIBits (32bpp top-down)
  → FNV hash frame skip (정적 화면 스킵)
  → 동일 VP9/JPEG 인코딩 파이프라인
```

GDI는 RDP 세션(Session 1 Active인 경우 포함)과 콘솔 세션 모두에서 동작. DXGI는 해당 세션이 Active인 경우에만 동작.

VP9 bitrate: `VDESK_VP9_BITRATE_KBPS` env var (default 8000 kbps). Falls back to JPEG if VP9 init fails.

**VP9 encoder settings** (`vpx_wrap.c`): `cpu_used=5` (quality/speed balance), `rc_max_quantizer=48` (minimum quality floor), `VP8E_SET_SCREEN_CONTENT_MODE=1` (desktop UI optimized), `kf_max_dist=fps*3`, CBR mode. `AcquireNextFrame` timeout is 16ms (~60fps cap).

**DirtyRects optimization**: `DxgiCapture::capture()` calls `GetFrameDirtyRects` after each `AcquireNextFrame`. If dirty area < 50% of screen, uses `CopySubresourceRegion` per rect instead of full `CopyResource`, and `bgra_to_i420_rects()` converts only those regions. The I420 buffer persists across frames.

**DXGI COM 해제 순서**: `DxgiCapture`는 명시적 `Drop` impl로 자식→부모 순서(`staging → duplication → context → device`) 해제. 그 전에 `ClearState() + Flush()`로 GPU 커맨드 큐를 비움. Rust 기본 drop 순서(선언 순서)는 부모 먼저 해제되므로 반드시 명시적 Drop이 필요.

### Wire Protocol

**Agent → Viewer:**
- `0x10` Init: `[width(4BE), height(4BE), fps(1), codec(1)]` — first message; codec `1`=VP9, `0`=JPEG
- `0x11` Frame: `[is_key(1), data_len(4BE), vp9_or_jpeg_data]` — per frame
- `0x12` Pong: `[timestamp(8BE)]`
- `0x13` CursorShape: `[cursor_type(1)]` — 0=Arrow 1=IBeam 2=SizeWE 3=SizeNS 4=SizeNWSE 5=SizeNESW 6=SizeAll 7=Hand 8=Wait 9=No; sent every 50ms when changed

**Viewer → Agent:**
- `0x01` MouseMove: `[x(4BE), y(4BE), win_w(2BE), win_h(2BE)]` — viewer window coords
- `0x02` MouseButton: `[button(1), pressed(1)]` — Left=0, Right=2, Middle=4
- `0x03` KeyPress: `[keycode(4BE), pressed(1)]` — winit 0.30 `KeyCode` discriminant
- `0x04` Scroll: `[dx(2BE), dy(2BE)]` — Windows notch units (120 = one click)
- `0x06` Ping: `[timestamp(8BE)]`
- `0x07` MouseGlobal: `[gx(4BE), gy(4BE)]` — virtual screen absolute pixels
- `0x08` KeyVk: `[vk(4BE), scan(2BE), pressed(1), extended(1)]` — Windows VK code path

`FramedStream` (from `hbb_common`) length-prefixes every message automatically; callers just call `send_bytes` / `next`.

### Mouse Coordinate Paths

- `0x01` used when viewer and agent are on **different PCs** — viewer sends client-area coords + window size; agent scales via `GetSystemMetrics`
- `0x07` used when `VDESK_DIRECT=1` or `VDESK_MOUSE_GLOBAL=1` — viewer computes `gx = window_inner_position.x + client_x`; agent maps via `SM_XVIRTUALSCREEN`/`SM_CXVIRTUALSCREEN`

### Remote Control Mode (viewer)

- **Off**: no input sent; title shows "클릭하여 원격 제어"
- **On** (left-click to enter): `CursorGrabMode::Confined` (`ClipCursor`); green 4px border; all mouse/keyboard forwarded
- **Release**: Escape key or window close
- **F11**: fullscreen toggle (always local, never forwarded)
- `WDA_EXCLUDEFROMCAPTURE` applied on window creation — prevents viewer window from appearing in agent's screen capture
- **Cursor mirroring**: agent polls `GetCursorInfo` every 50ms, sends `0x13` when type changes; viewer stores in `display::REMOTE_CURSOR_TYPE` (`AtomicU8`) and applies via `SetCursor(LoadCursorW(...))` directly (winit `set_cursor` is overridden by `WM_SETCURSOR` after grab)
- **Viewer edge resize**: cursor at 12px edge zone → `at_edge=true`; mouse move not forwarded to agent; left-click triggers `ReleaseCapture + ClipCursor(NULL) + WM_NCLBUTTONDOWN` for OS resize loop; `Resized` event restores `CursorGrabMode::Confined`. Grab is **never** released in `CursorMoved` — only in `trigger_edge_resize`.
- **Mouse button tracking**: `mouse_btns: u8` bitmask (bit0=Left, bit1=Right, bit2=Middle); `release_all_inputs` only sends UP events for buttons that are actually down — prevents spurious right-click on ESC
- **Global keyboard hook**: `SetWindowsHookExA(WH_KEYBOARD_LL)` installed on control mode enter; captures all keys regardless of focus; ESC and F11 pass through to local handler; removed on mode exit
- **Korean IME (한/영 key)**: viewer disables local IME via `ImmAssociateContextEx(hwnd, 0, 0)` on control enter (re-enables on exit); hook suppresses 한/영 locally and sends `KeyVk(VK_HANGUL=0x15)` to agent; agent's `inject_key_vk` strips `KEYEVENTF_EXTENDEDKEY` for VK_HANGUL/VK_HANJA so remote Windows Korean IME recognizes the toggle event. Korean character composition is handled by the remote IME from raw VK codes.
- **Korean IME Raw Input fallback**: `device_event` handler also listens for `DeviceEvent::Key` with `KeyCode::Lang1` (→ VK_HANGUL 0x15) and `KeyCode::Lang2` (→ VK_HANJA 0x19). This handles cases where `WH_KEYBOARD_LL` fails to intercept these keys before the Korean IME processes them — the Raw Input path receives the scan code regardless of IME state.

### Session Lifecycle & Reconnect

`session::run()` (agent) spawns a `spawn_blocking` capture task and keeps its `JoinHandle`. After the session loop exits:
1. `video_rx` is dropped → capture loop detects `TrySendError::Closed` and exits
2. `capture_handle.await` blocks until the task finishes — **DXGI `IDXGIOutputDuplication` handle is fully released before `run()` returns**
3. 1500ms sleep 후 복귀 — Windows GPU 드라이버가 `IDXGIOutputDuplication::Release()` 이후 비동기로 정리를 진행하므로, 즉시 재연결 시 `E_UNEXPECTED` 방지

이 대기 전 `capture_dxgi::reclaim_output()`을 호출해 이전 세션 고스트 상태를 `TakeOwnership + ReleaseOwnership`으로 강제 회수. `video_tx` (original) is dropped immediately after spawning so that if the capture task fails before the first frame, `video_rx.recv()` returns `None` instead of blocking forever.

## Key Files

| File | Purpose |
|------|---------|
| `vdesk_agent/src/api.rs` | All HTTP request/response types + async backend API calls (register, heartbeat, poll, activate, end) |
| `vdesk_viewer/src/api.rs` | Viewer-side HTTP calls: login, create session, fetch relay, end session |
| `vdesk_agent/src/main.rs` | Registration + heartbeat + poll/activate loop; direct-mode entry |
| `vdesk_agent/src/server.rs` | TCP listener; `listen_loop` (backend) + `listen_loop_direct` (direct) |
| `vdesk_agent/src/session.rs` | Per-session: VP9 frame send + input receive/dispatch + capture lifecycle |
| `vdesk_agent/src/state.rs` | `AgentState` enum + `SharedState = Arc<Mutex<AgentState>>` |
| `vdesk_agent/src/services/capture_dxgi.rs` | DXGI Desktop Duplication; `DxgiCapture` + `CaptureFrame`; DirtyRects partial copy; `reclaim_output()` |
| `vdesk_agent/src/services/capture_gdi.rs` | GDI BitBlt fallback; `GdiCapture`; always full-frame BGRA capture |
| `vdesk_agent/src/services/video.rs` | `CaptureState` 상태 머신 (DXGI↔GDI 전환); FNV hash frame skip; VP9/JPEG encode |
| `vdesk_agent/src/services/vpx_enc.rs` | VP9 encoder FFI wrapper (`vpx_wrap.c`) |
| `vdesk_agent/src/services/yuv.rs` | `bgra_to_i420` (full) + `bgra_to_i420_rects` (partial, dirty-rect aware) |
| `vdesk_agent/src/services/input.rs` | `SendInput` injection; coordinate conversion; no-op stubs on non-Windows |
| `vdesk_viewer/src/display.rs` | winit 0.30 event loop; remote control mode; softbuffer render |
| `vdesk_viewer/src/session.rs` | Frame receive/decode + input serialization |
| `vdesk_viewer/src/connection.rs` | TCP connect + sessionKey handshake |
| `vdesk_viewer/src/vpx_dec.rs` | VP9 decoder FFI wrapper |
| `vdesk_viewer/src/decoder.rs` | JPEG → XRGB pixel buffer (fallback path) |

## Key Conventions

- Source comments and many identifiers are in Korean — this is expected.
- `hbb_common` (from sibling `vdesk_client` workspace at `../vdesk_client/vdesk_client/libs/hbb_common`) provides `log`, `tcp::FramedStream`, and the Tokio re-export.
- winit 0.30 requires `ApplicationHandler` trait; the event loop must run on the main thread. The session TCP loop runs in a spawned thread with its own `tokio::runtime::Runtime`.
- `reqwest::blocking` must not be dropped inside a `block_on` context — the viewer drops `rt` before calling `end_session`.
- VP9 C wrappers (`vpx_wrap.c`) are compiled via `cc` crate in `build.rs`; both agent and viewer have their own copy.
- Agent's device UUID (`localBox`) persists across restarts in `%TEMP%\vdesk_agent_id`. Delete this file to force a fresh registration with the backend.
- **DualLogger**: `main.rs` uses a custom `log::Log` impl that writes simultaneously to stderr and `<exe_dir>/logs/vdesk_agent.log`. Initialized via `init_logger()` before anything else. File is appended, never rotated.
- **`vdesk_client/libs/virtual_display/dylib/`**: Contains `IddController.c` — a complete C implementation for creating/managing an IDD virtual display (used by RustDesk). Available for future integration to solve black-screen-on-minimize via virtual adapter.
