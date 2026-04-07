# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Repository Layout

This is a Cargo workspace with two crates:

| Path | Stack | Role |
|------|-------|------|
| `vdesk_agent/` | Rust (async Tokio) | VM agent — registers with backend, TCP 20020 listener, JPEG screen streaming |
| `vdesk_viewer/` | Rust (sync + Tokio thread) | Viewer — backend login, direct TCP connect, JPEG decode, winit render |

The `.vs/` directory is a Visual Studio workspace with CMake disabled (`enableCMake: false`). No VS project is wired up.

## Build Commands

```bash
# From workspace root (VDeskAgentViewer/)

cargo build --release --package vdesk_agent
cargo build --release --package vdesk_viewer

# Check whole workspace
cargo check

# Run tests
cargo test
```

## Run

```bash
# Agent (on VM)
VDESK_API_URL=http://backend:8080 AGENT_PORT=20020 RUST_LOG=info ./vdesk_agent

# Viewer (on user PC)
VDESK_API_URL=http://backend:8080 VDESK_EMAIL=user@example.com VDESK_PASSWORD=pass ./vdesk_viewer --device 42
```

If `--device` is omitted, the viewer interactively lists linked devices (or discovers unlinked ones and prompts to link).

## Architecture

### Session Flow

```
Agent → POST /api/host/register           → deviceKey
Agent → POST /api/host/heartbeat          (15s loop, carries relayIp = agent's own LAN IP)
Agent → POST /api/agent/sessions/poll     (3s loop)
Viewer → POST /api/remote/sessions        → sessionKey
Agent poll returns pending session → POST /api/agent/sessions/activate → RUNNING
Viewer → GET /api/agent/sessions/relay    → relayIp:20020
Viewer → TCP connect relayIp:20020
Viewer → send sessionKey (FramedStream handshake)
Agent → validates sessionKey via GET /api/agent/sessions/relay (checks status == RUNNING)
Agent → stream JPEG frames: [width(4) + height(4) + jpeg_len(4) + jpeg_data]
Viewer → decode JPEG → softbuffer render (1280×720 window)
Viewer → send input events: [type(1) + payload]
```

**Why no relay**: VMs have managed networking, so the agent's LAN IP is directly reachable. The backend's `relayIp` field stores the agent's own IP — "relay" naming is legacy.

### Wire Protocol

**Frame (agent → viewer):** big-endian `u32 width`, `u32 height`, `u32 jpeg_len`, then `jpeg_len` bytes of JPEG.

**Input events (viewer → agent):**
- `0x01` Mouse move: `[x(4BE), y(4BE)]` — viewer window coords (1280×720 basis)
- `0x02` Mouse button: `[button(1), pressed(1)]` — button values: Left=0, Right=2, Middle=4
- `0x03` Key: `[keycode(4BE), pressed(1)]` — winit 0.30 `KeyCode` discriminant as u32

Input injection uses Windows `SendInput` API. On non-Windows platforms the inject functions are no-ops.

Mouse coordinates are scaled from viewer window size (hardcoded 1280×720 in `input.rs`) to agent screen resolution using `GetSystemMetrics`.

### Agent Identity

The agent generates a persistent `localBox` ID on first run and stores it at `$TMPDIR/vdesk_agent_id`. This ID identifies the device across restarts.

### Key Files

| File | Purpose |
|------|---------|
| `vdesk_agent/src/api.rs` | Backend HTTP client: register / heartbeat / poll / activate / end |
| `vdesk_agent/src/server.rs` | TCP listener (port 20020), sessionKey handshake + relay validation |
| `vdesk_agent/src/session.rs` | Per-session loop: JPEG frame send + input receive/dispatch |
| `vdesk_agent/src/services/video.rs` | Screen capture (`screenshots` crate) + JPEG encode (`image` crate) |
| `vdesk_agent/src/services/input.rs` | Windows `SendInput` injection; no-ops on other platforms |
| `vdesk_viewer/src/api.rs` | Backend HTTP client: login / list devices / create session / relay / end |
| `vdesk_viewer/src/connection.rs` | TCP connect + sessionKey handshake |
| `vdesk_viewer/src/session.rs` | Frame receive + decode + input send |
| `vdesk_viewer/src/display.rs` | winit 0.30 window + softbuffer render; produces `InputEvent` |
| `vdesk_viewer/src/decoder.rs` | JPEG → XRGB `u32` pixel buffer |

## Key Conventions

- Source comments and many identifiers are in Korean — this is expected.
- `hbb_common` (from the sibling `vdesk_client` workspace) provides `log`, `tcp::FramedStream`, and the Tokio re-export used by the agent.
- **Codec**: `scrap` (VP9/AV1) requires vcpkg + LLVM/clang bindgen setup. These crates use `screenshots` + JPEG instead. VP9 can be added later.
- The agent enforces a single active session at a time via `Arc<AtomicBool> session_active`; a 1-hour background timeout resets this flag if the session isn't closed normally.
