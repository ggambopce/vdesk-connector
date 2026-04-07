# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Repository Layout

This is a Cargo workspace with two crates:

| Path | Stack | Role |
|------|-------|------|
| `vdesk_agent/` | Rust (async Tokio) | Screen-streaming agent — TCP listener, JPEG capture, input injection |
| `vdesk_viewer/` | Rust (sync + Tokio thread) | Remote viewer — winit window, JPEG decode, input capture |

The `.vs/` directory is a Visual Studio workspace artifact. No VS project is wired up.

## Build Commands

```bash
# From workspace root (VDeskAgentViewer/)
cargo build --release --package vdesk_agent
cargo build --release --package vdesk_viewer
cargo check          # whole workspace
```

## Running — Direct Mode (no backend required)

Two PowerShell scripts launch agent + viewer on the same PC for local UX testing:

```powershell
# Terminal 1 — agent
.\run_agent_direct.ps1          # default: port 20020, key "direct"

# Terminal 2 — viewer
.\run_viewer_direct.ps1         # connects to 127.0.0.1:20020
.\run_viewer_direct.ps1 -AgentHost "192.168.1.100"   # remote agent
```

Relevant env vars for direct mode:
- `VDESK_DIRECT=1` — skip backend, use direct TCP
- `VDESK_DIRECT_KEY` — shared session key (default: `"direct"`)
- `VDESK_DIRECT_HOST` / `VDESK_DIRECT_PORT` — viewer-side target
- `AGENT_PORT` — agent listen port (default: 20020)
- `AGENT_NO_INJECT=1` — disable `SendInput` (streaming-only test)

## Running — Backend Mode

```powershell
# Agent
.\run_agent_local.ps1           # sets AGENT_RELAY_IP=127.0.0.1

# Viewer
$env:VDESK_API_URL="http://localhost:8080"
$env:VDESK_EMAIL="user@example.com"
$env:VDESK_PASSWORD="pass"
.\target\release\vdesk_viewer.exe --device 42
```

## Architecture

### Session Flow (backend mode)

```
Agent → POST /api/host/register           → deviceKey
Agent → POST /api/host/heartbeat          (15s loop, relayIp = agent's own LAN IP)
Agent → POST /api/agent/sessions/poll     (3s loop, looks for REQUESTED status)
Viewer → POST /api/remote/sessions        → sessionKey (creates REQUESTED session)
Agent poll → POST /api/agent/sessions/activate → RUNNING
Viewer → GET /api/agent/sessions/relay    → relayIp:20020
Viewer → TCP connect → handshake → stream
```

### Wire Protocol

**Agent → Viewer (server pushes):**
- `0x10` Init: `[width(4BE), height(4BE), fps(1)]` — first message per session
- `0x11` Frame: `[jpeg_len(4BE), jpeg_data]` — per frame
- `0x12` Pong: `[timestamp(8BE)]` — RTT response

**Viewer → Agent (input events):**
- `0x01` MouseMove: `[x(4BE), y(4BE), win_w(2BE), win_h(2BE)]` — viewer window coords
- `0x02` MouseButton: `[button(1), pressed(1)]` — Left=0, Right=2, Middle=4
- `0x03` KeyPress: `[keycode(4BE), pressed(1)]` — winit 0.30 `KeyCode` discriminant
- `0x04` Scroll: `[dx(2BE), dy(2BE)]` — Windows notch units (120 = one click)
- `0x05` CharInput: `[len(2BE), utf8_bytes]` — IME-committed text (Korean/CJK)
- `0x06` Ping: `[timestamp(8BE)]`
- `0x07` MouseGlobal: `[gx(4BE), gy(4BE)]` — virtual screen absolute pixels (same-PC mode)

`0x07` is used when `VDESK_DIRECT=1` or `VDESK_MOUSE_GLOBAL=1`. The viewer computes `gx = window_inner_position.x + client_x` and the agent converts using `SM_XVIRTUALSCREEN`/`SM_CXVIRTUALSCREEN` to Windows absolute coords. This avoids coordinate-scaling errors when agent and viewer are on the same PC.

### Remote Control Mode (viewer)

- **Off**: no input sent; title shows "클릭하여 원격 제어"
- **On** (left-click): `CursorGrabMode::Confined` keeps cursor inside window; green 4px border; title changes; all mouse/keyboard forwarded as `0x01–0x07`
- **Release**: Escape key or window close
- `WDA_EXCLUDEFROMCAPTURE` applied on window creation — prevents the viewer window from appearing in the agent's screen capture (no infinite-mirror effect)

### Input Injection (agent)

`vdesk_agent/src/services/input.rs` wraps Windows `SendInput`. Non-Windows builds compile with no-op stubs. `set_no_inject(true)` disables all injection globally (used by `AGENT_NO_INJECT=1`).

Mouse coordinate paths:
- `inject_mouse_move(vx, vy, win_w, win_h)` — scales viewer window coords to screen coords via `GetSystemMetrics`
- `inject_mouse_move_global(gx, gy)` — maps virtual-screen absolute pixels to `MOUSEEVENTF_ABSOLUTE` using `SM_XVIRTUALSCREEN`/`SM_CXVIRTUALSCREEN` (handles multi-monitor)

### Agent Identity

Generates a persistent `localBox` ID on first run, stored at `$TMPDIR/vdesk_agent_id`. Single active session enforced via `Arc<AtomicBool> session_active`; 1-hour background timeout resets it if the session isn't cleanly closed.

### Key Files

| File | Purpose |
|------|---------|
| `vdesk_agent/src/server.rs` | TCP listener; `listen_loop` (backend mode) + `listen_loop_direct` (direct mode) |
| `vdesk_agent/src/session.rs` | Per-session loop: JPEG frame send + input receive/dispatch |
| `vdesk_agent/src/services/video.rs` | Screen capture (`screenshots`) + JPEG encode (`image`) |
| `vdesk_agent/src/services/input.rs` | `SendInput` injection; coordinate conversion; no-op stubs |
| `vdesk_agent/src/state.rs` | `AgentState` enum: `Idle / Pending / Streaming` |
| `vdesk_viewer/src/display.rs` | winit 0.30 event loop; remote control mode; softbuffer render |
| `vdesk_viewer/src/session.rs` | Frame receive/decode + input serialization |
| `vdesk_viewer/src/connection.rs` | TCP connect + sessionKey handshake |
| `vdesk_viewer/src/decoder.rs` | JPEG → XRGB pixel buffer |

## Key Conventions

- Source comments and many identifiers are in Korean — this is expected.
- `hbb_common` (from sibling `vdesk_client` workspace) provides `log`, `tcp::FramedStream`, and the Tokio re-export.
- Codec is `screenshots` + JPEG (not VP9/scrap), avoiding the vcpkg+LLVM bindgen requirement.
- `FramedStream` length-prefixes every message automatically; callers just call `send_bytes` / `next`.
- winit 0.30 requires `ApplicationHandler` trait; the event loop must run on the main thread. The session TCP loop runs in a spawned thread with its own `tokio::runtime::Runtime`.
- `reqwest::blocking` must not be dropped inside a `block_on` context — the agent drops `rt` before calling blocking API cleanup.
