# RFC: Interactive Terminal Support for Claude Code in VoidBox

**Status:** Draft
**Author:** @cspinetta
**Date:** 2026-03-29

## Context

VoidBox currently supports batch command execution via a request/response protocol over vsock. To run Claude Code's interactive TUI inside a guest VM, we need bidirectional, low-latency terminal I/O — something fundamentally different from the current `ExecRequest`/`ExecResponse` model.

### Goal

The user runs:
```bash
cd /path/to/my-project
voidbox run --file coding-claude-agent.yaml --interactive -- --dangerously-skip-permissions
```

This boots a VoidBox VM with the project directory mounted RW, starts Claude Code on a guest PTY, and provides a fully interactive terminal on the host — keystrokes forwarded in, TUI output streamed back, all sandboxed inside the micro-VM.

---

## 1. Terminal I/O Transport: host <-> guest

### Options Evaluated

| Option | Pros | Cons | Verdict |
|--------|------|------|---------|
| **A: Dedicated vsock PTY relay** | Clean separation, simple framing | Duplicates auth handshake, second listener thread | Viable but redundant |
| **B: Serial console** | Already exists (`src/devices/serial.rs`) | Unidirectional currently; no multiplexing with kernel logs; KVM-only (VZ uses `hvc0`); low bandwidth | **Reject** |
| **C: SSH over vsock** | Native PTY/resize/signals; battle-tested | Adds dropbear to initramfs (~200KB+); key management; extra attack surface | Overkill |
| **D: Extend control channel** | Reuses vsock infra, auth, platform abstraction; each request already gets a fresh connection | Slightly overloads the protocol | **Recommended** |

### Recommended: Option D — Extend the existing control channel

**Why:** The `ControlChannel` already creates a fresh vsock connection per request (`GuestConnector` closure), performs Ping/Pong auth, and abstracts KVM vs VZ. Adding a new message type that transitions the connection into interactive mode follows the same pattern as `SubscribeTelemetry` (type 10), which also takes over a connection for streaming. No new ports, no new listeners, no duplicate auth.

**Framing after handshake:** After `InteractiveStarted` acknowledgment, both sides exchange messages using the existing 5-byte header (4B length + 1B type). `InteractiveData` payloads are raw bytes (not JSON). `InteractiveResize` uses JSON for its small struct. This reuses `Message::read_from_sync`/`Message::serialize` with zero changes to framing code.

**Latency:** The vsock connection is established once. All subsequent I/O is on the established fd — typical round-trip is under 100us on both AF_VSOCK (KVM) and VZ fds. No per-keystroke connection overhead.

**TUI compatibility:** Raw byte relay preserves all escape sequences, alternate screen buffer, mouse events, 256-color/truecolor — the transport is opaque to content.

**Platform parity:** Both KVM and VZ return a `Box<dyn GuestStream>` (a plain fd). The interactive relay works identically on both.

---

## 2. Guest-Side PTY & Process Management

### Current state
- Guest-agent uses `std::process::Command` with pipe-based stdout/stderr (`guest-agent/src/main.rs`)
- No PTY allocation anywhere in the codebase
- `nix` crate already in `guest-agent/Cargo.toml` dependencies
- Privilege drop to uid 1000 via `pre_exec` block

### Recommendation

**PTY allocation:** Use POSIX `posix_openpt` + `grantpt` + `unlockpt` + `ptsname` (or direct `open("/dev/ptmx")` + ioctls). The guest-agent targets musl, where `openpty()` may not be available. Direct `/dev/ptmx` is ~20 lines and works on all Linux targets. Alternatively, `nix::pty::openpty` with the `pty` feature — verify musl support.

**Process setup on slave side:**
1. `setsid()` — new session
2. `TIOCSCTTY` ioctl — make PTY the controlling terminal
3. `dup2` slave fd to stdin/stdout/stderr
4. Same privilege drops as existing exec: `setgid(1000)`, `setuid(1000)`, `setpgid(0,0)`, resource limits

**Environment:**
- `TERM=xterm-256color` (Claude Code requires this)
- `ROWS`/`COLUMNS` from the request
- All env vars from `InteractiveStartRequest`

**Window resize:** On receiving `InteractiveResize`, apply `TIOCSWINSZ` ioctl on PTY master fd, then `kill(child_pgid, SIGWINCH)`.

**Bidirectional relay:** Single-threaded `poll(2)` loop on two fds (vsock + PTY master):
- vsock readable: read `Message`, dispatch: `InteractiveData` -> write to PTY master; `InteractiveResize` -> apply ioctl + SIGWINCH
- PTY master readable: read bytes, wrap in `InteractiveData`, write to vsock
- PTY master HUP: child exited -> `waitpid()` -> send `InteractiveExit` with exit code

**Lifecycle:** When the child process exits, the PTY master returns EOF/HUP. The guest sends `InteractiveExit { exit_code }` and closes the connection. If the host disconnects first (vsock read returns 0), the guest sends `SIGHUP` to the child process group.

---

## 3. Host-Side Terminal Management

### Approach

Use `libc::tcgetattr`/`libc::tcsetattr` with `libc::cfmakeraw` directly — `libc` is already a dependency, no new crates needed.

**Raw mode setup:**
1. `tcgetattr(STDIN_FILENO)` -> save original `termios`
2. `cfmakeraw(&mut raw_termios)` -> disable echo, canonical mode, signal processing
3. `tcsetattr(STDIN_FILENO, TCSANOW, &raw_termios)`

**RAII restoration:** A `RawModeGuard` struct that restores original termios in `Drop`. Also register `atexit` handler for crash recovery.

**SIGWINCH handling:** Use `tokio::signal::unix::signal(SignalKind::window_change())` or a self-pipe pattern. On signal: `TIOCGWINSZ` on stdout -> send `InteractiveResize` to guest.

**Bidirectional relay:** Single `spawn_blocking` thread using `poll(2)` on stdin fd + vsock fd:
- stdin readable: read bytes, send `InteractiveData` to vsock
- vsock readable: read `Message`: `InteractiveData` -> write to stdout; `InteractiveExit` -> break, return exit code
- Also poll a self-pipe fd for SIGWINCH notifications

**Exit:** Restore terminal, print exit code if non-zero, propagate exit code to process.

### Reference implementations
- `ssh` client: raw mode + bidirectional relay + SIGWINCH forwarding (exactly this pattern)
- `docker exec -it`: same pattern with a different transport

---

## 4. Project Directory Mounting

### Current state
- Mount infrastructure fully functional: `MountSpec` -> `MountConfig` -> kernel cmdline -> guest-agent mount
- 9p on KVM (`src/devices/virtio_9p.rs`: full 9P2000.L implementation), virtiofs on VZ
- RW mounts supported

### Assessment for coding workloads

**Sufficient for Claude Code:** 9p/virtiofs handle file reads, writes, directory traversal, and git operations. Claude Code primarily does sequential file I/O.

**Performance concerns:**
- **9p on KVM:** Userspace virtio backend, single-queue. Adequate for coding workloads (file edits, git status), but `git clone` of large repos or `npm install` with thousands of small files will be noticeably slower than native.
- **virtiofs on VZ:** Kernel-level, faster than 9p. No concerns.

**File watching:** `inotify`/`fanotify` may not work over 9p. Claude Code doesn't rely on file watching (it does explicit reads). Known limitation for future tools.

### Recommendation

- Mount the project directory RW at `/workspace` (matches existing `ALLOWED_WRITE_ROOTS`)
- Set `working_dir` to `/workspace` in the interactive session
- No changes to mount infrastructure needed

---

## 5. Required Guest Image Contents

### Current state
`scripts/build_claude_rootfs.sh` already bundles:
- `claude-code` native binary (Bun/JSC single-executable)
- glibc shared libraries (auto-detected via ldd)
- SSL CA certificates (`/etc/ssl/certs/ca-certificates.crt`)
- `/etc/passwd` + `/etc/group` with sandbox user (uid 1000)
- BusyBox (if available) for `/bin/sh` and basic utilities

### What's needed for interactive Claude Code

**Already included:** claude-code binary, SSL certs, shell utilities (BusyBox), network support (SLIRP/NAT).

**May need adding:**
- `git` — Claude Code invokes git frequently. Verify presence in current image; if missing, add static `git` or use OCI base image.
- `node`/`python` — Project-specific. Best handled via `sandbox.image` (OCI base image).

**API key passing:**
- `ANTHROPIC_API_KEY` via `env` field in `InteractiveStartRequest`
- The credential staging system (`src/credentials.rs`) already mounts OAuth tokens at `/home/sandbox/.claude/`

**Network:** SLIRP (KVM) and VZ NAT both provide outbound TCP. `network: true` in the spec is required.

### Recommendation

Use `build_claude_rootfs.sh` for the guest image. For projects needing specific runtimes, use `sandbox.image` (OCI base image). No changes to image building needed for the core interactive feature.

---

## 6. Spec File Design

### Recommended spec: `coding-claude-agent.yaml`

```yaml
api_version: v1
kind: Agent
name: coding-claude-agent

sandbox:
  memory_mb: 2048        # Claude Code + dev tools need more RAM
  vcpus: 2
  network: true           # Required for Anthropic API
  mounts:
    - host: "."           # Current directory
      guest: /workspace
      mode: rw

agent:
  prompt: ""              # Unused in interactive mode but required by schema

interactive:
  program: claude         # Binary to run on the guest PTY
  args: []                # Default args (CLI --args append to these)
```

### CLI changes

```
voidbox run --file coding-claude-agent.yaml --interactive [-- extra-args...]
```

- `--interactive` flag on `Run` command triggers interactive mode
- `-- extra-args...` captured via clap's `trailing_var_arg` and forwarded as additional args to the guest program
- If `--interactive` without an `interactive` section in spec: default to running `/bin/sh`
- Host terminal size auto-detected via `TIOCGWINSZ`

### Spec changes (`src/spec.rs`)

Add optional `interactive` field to `RunSpec`:
```rust
pub struct InteractiveSpec {
    pub program: Option<String>,     // default: "claude" or "/bin/sh"
    pub args: Option<Vec<String>>,
    pub env: Option<HashMap<String, String>>,
}
```

---

## 7. Security Model

### Effective security boundary

| Aspect | Isolated | Accessible |
|--------|----------|------------|
| Host filesystem | Everything except mounted dirs | Only explicitly mounted dirs (e.g., `/workspace`) |
| Network (outbound) | Nothing blocked by default | Full outbound (needed for Anthropic API) |
| Network (inbound) | All inbound blocked | Only vsock control channel |
| Host processes | Completely isolated | None |
| Host kernel | Completely isolated | None (guest runs its own kernel) |
| Privileges | Guest root is NOT host root | uid 1000 sandbox user for all commands |

### Key points

- **Mount isolation:** Only directories listed in `mounts:` are visible inside the VM. The guest-agent's `ALLOWED_WRITE_ROOTS` (`/workspace`, `/home`, `/etc/voidbox`) restricts write paths. The host OS is completely invisible to the guest.
- **Network:** SLIRP/NAT provides outbound-only connectivity — no inbound connections possible. Network deny lists exist in `BackendSecurityConfig` but are not yet enforced on VZ. Unrestricted outbound is acceptable for initial implementation. Future: restrict to Anthropic API IPs.
- **API key:** Passed via `env` in the vsock protocol (host-local IPC, never crosses a network). Never written to disk in the guest.
- **`--dangerously-skip-permissions`:** Safe in this context — the VM boundary IS the safety net. Even with all permissions, Claude Code can only affect the mounted project directory and make outbound network calls. This is the entire value proposition of running Claude Code sandboxed in VoidBox.
- **Resource limits:** RLIMIT_NPROC, RLIMIT_FSIZE, RLIMIT_NOFILE prevent resource exhaustion. Process group isolation enables clean timeout/kill.

---

## Implementation Plan (ordered by dependency)

### Phase 1: Protocol Extension
**File:** `void-box-protocol/src/lib.rs`
- Add message types: `InteractiveStart=18`, `InteractiveStarted=19`, `InteractiveResize=20`, `InteractiveData=21`, `InteractiveExit=22`
- Add structs: `InteractiveStartRequest`, `InteractiveStarted`, `InteractiveResize`, `InteractiveExit`
- Update `TryFrom<u8>` impl for `MessageType`
- Bump `PROTOCOL_VERSION` to 2

### Phase 2: Guest-Agent PTY Handler
**File:** `guest-agent/src/main.rs`
- Add `handle_interactive_session()` function
- PTY allocation via `posix_openpt`/`grantpt`/`unlockpt` or `/dev/ptmx` ioctls
- Child fork with `setsid` + `TIOCSCTTY` + privilege drop
- `poll(2)` relay loop: vsock <-> PTY master
- Window resize handling (`TIOCSWINSZ` + `SIGWINCH`)
- Exit detection via PTY HUP + `waitpid`

### Phase 3: Host ControlChannel Method
**File:** `src/backend/control_channel.rs`
- Add `start_interactive_session()`: connect, handshake, send `InteractiveStart`, read `InteractiveStarted`, return `Box<dyn GuestStream>`

### Phase 4: VmmBackend Trait Extension
**Files:** `src/backend/mod.rs`, `src/backend/kvm.rs`, `src/backend/vz/backend.rs`
- Add `start_interactive()` method to `VmmBackend` trait
- Implement in both backends

### Phase 5: Host Terminal Relay Module
**File (new):** `src/terminal.rs`
- `RawModeGuard`: RAII struct for termios save/restore
- `get_terminal_size()`: `TIOCGWINSZ` ioctl wrapper
- `run_interactive_relay()`: `poll(2)` loop over stdin + vsock + SIGWINCH self-pipe

### Phase 6: CLI Integration
**Files:** `src/bin/voidbox/main.rs`, `src/spec.rs`
- Add `--interactive` flag and trailing args to `Run` command
- Add `InteractiveSpec` to spec system
- Interactive boot path: boot VM -> detect terminal size -> `start_interactive` -> relay -> restore -> exit

### Phase 7: Example Spec & Documentation
**File (new):** `examples/specs/coding-claude-agent.yaml`

---

## Risk Areas

| Risk | Impact | Mitigation |
|------|--------|------------|
| PTY data loss under heavy TUI output | Screen corruption | `O_NONBLOCK` on PTY master; buffer up to 256KB; drop oldest on overflow |
| Terminal state corruption on host crash | User stuck in raw mode | `RawModeGuard` Drop impl; `atexit` handler; document `reset`/`stty sane` recovery |
| `openpty` unavailable on musl | Build failure | Use direct `/dev/ptmx` + ioctls (~20 lines) |
| VZ `connectToPort` latency | Keystroke lag | Connection established once; all subsequent I/O on established fd |

---

## Verification

1. **Unit tests:** Protocol serialization/deserialization for new message types
2. **Integration test:** Boot VM -> start interactive session -> send keystrokes -> verify echo -> send resize -> verify SIGWINCH -> exit -> verify exit code
3. **Manual E2E:** `voidbox run --file examples/specs/coding-claude-agent.yaml --interactive -- --dangerously-skip-permissions` -> interact with Claude Code TUI
4. **Platform:** Test on both macOS (VZ) and Linux (KVM)
5. **Cleanup:** Verify terminal restored after normal exit, Ctrl-C, and SIGKILL
