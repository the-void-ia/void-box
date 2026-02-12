# VM0 crates exploration

Summary of how [vm0-ai/vm0](https://github.com/vm0-ai/vm0) organizes its Rust crates (`crates/`). Useful for comparing with void-box’s sandbox, vsock, and guest layout.

## Workspace layout

```
crates/
├── Cargo.toml          # workspace, members, shared deps, lints
├── README.md           # crate table + architecture diagram
├── sandbox/            # Sandbox trait + shared types
├── sandbox-fc/         # Firecracker implementation of Sandbox
├── runner/             # Firecracker orchestrator (polls API, VM lifecycle)
├── vsock-proto/        # Binary protocol (encode/decode) host↔guest
├── vsock-host/         # Host-side async client (tokio, UDS in FC case)
├── vsock-guest/        # Guest-side agent (exec, write_file, spawn_watch)
├── vsock-test/         # E2E tests (host + guest over Unix sockets)
├── guest-init/         # PID 1 for Firecracker (mounts, then vsock-guest)
├── guest-agent/        # In-VM orchestrator (CLI, heartbeat, telemetry, checkpoint)
├── guest-common/       # Shared guest utilities
├── guest-download/     # Download/extract storage archives
├── guest-mock-claude/  # Mock Claude CLI for tests
└── ably-subscriber/    # Ably Pub/Sub (realtime, WebSocket/MessagePack)
```

## Architecture (from their README)

- **Guest**: `guest-agent` uses `guest-download`; `guest-init` (PID 1) runs `vsock-guest`. Vsock: CID=2 (host), port=1000.
- **Host**: `runner` → `sandbox-fc` → `vsock-host`; `sandbox` is the trait.

Firecracker forwards vsock to **Unix domain sockets**: host listens on `{vsock_path}_{port}`, guest connects to CID 2; FC bridges to that UDS.

## Sandbox trait (`sandbox`)

```rust
#[async_trait]
pub trait Sandbox: Send + Sync + Any {
    async fn start(&mut self) -> Result<()>;
    async fn exec(&self, request: &ExecRequest<'_>) -> Result<ExecResult>;
    async fn write_file(&self, path: &str, content: &[u8]) -> Result<()>;
    async fn spawn_watch(&self, request: &ExecRequest<'_>) -> Result<SpawnHandle>;
    async fn wait_exit(&self, handle: SpawnHandle, timeout: Duration) -> Result<ProcessExit>;
    async fn stop(&mut self) -> Result<()>;
    async fn kill(&mut self) -> Result<()>;
    fn id(&self) -> &str;
}
```

- `ExecRequest`: `cmd`, `timeout`, `env`.
- `ExecResult`: `exit_code`, `stdout`, `stderr`.
- `SpawnHandle` / `ProcessExit`: for long-lived processes and exit events.

## Vsock protocol (`vsock-proto`)

- **Wire format**: `[4B length BE][1B type][4B seq BE][payload]`. Length = type + seq + payload.
- **Message types**: ready, ping/pong, exec, exec_result, write_file / write_file_result, spawn_watch / spawn_watch_result, process_exit, shutdown / shutdown_ack, error.
- **Default port**: 1000.
- **Encoder/decoder**: `encode(msg_type, seq, payload)`, payload-specific helpers (`encode_exec`, `encode_exec_result`, …), and a buffered `Decoder` for streaming.
- **Exec payload**: `[4B timeout_ms][4B cmd_len][command]([4B env_count]([4B key_len][key][4B val_len][value])*)`.
- **Exec result**: `[4B exit_code][4B stdout_len][stdout][4B stderr_len][stderr]`.

All multi-byte fields are **big-endian** (vs void-box’s current JSON + little-endian length prefix).

## Vsock host (`vsock-host`)

- **Firecracker mode**: Connects over **Unix domain socket** (FC’s vsock proxy). `VsockHost::wait_for_connection(vsock_path, timeout)` binds `{vsock_path}_{port}`, accepts one connection, then does ready/ping/pong handshake.
- **API**: `exec`, `write_file`, `spawn_watch`, `wait_for_exit`, `shutdown`; uses `Decoder` and sequence numbers for request/response pairing; caches `process_exit` events for `wait_for_exit`.

## Vsock guest (`vsock-guest`)

- **In VM**: Uses `connect_vsock()` (AF_VSOCK, CID=2, port 1000) or `connect_unix(path)` for tests.
- **Loop**: Sends `MSG_READY`, then reads messages; handles exec (shell with optional `su - user`), write_file (mkdir + cat or sudo tee), spawn_watch (background process + process_exit), shutdown (sync + ack).
- **Reconnect**: After disconnect (e.g. snapshot restore), retries connect up to 50 times with 10 ms delay; stops retrying after `MSG_SHUTDOWN`.

## Sandbox-FC (`sandbox-fc`)

- **FirecrackerSandbox**: Implements `Sandbox`; manages VM lifecycle, network namespace pool, overlay FS, snapshots.
- **Paths**: Factory paths, per-sandbox paths, snapshot outputs.
- **Network**: Guest networking (e.g. pool of netns).
- **Overlay**: Overlay filesystem for rootfs/cow.

## Runner (`runner`)

- CLI: `start`, `setup`, `build-rootfs`.
- **Start**: Takes firecracker binary, kernel, rootfs, API URL, token, group, base_dir, optional snapshot_dir. Polls vm0 API for jobs; uses `sandbox-fc` to create/destroy VMs and `vsock-host` to talk to the guest.

## Guest init (`guest-init`)

- PID 1: mounts (proc, sys, dev, etc.), then execs/spawns vsock-guest (or the guest-agent that uses vsock-guest).

## Guest agent (`guest-agent`)

- Runs inside the VM: CLI execution, heartbeat, telemetry upload, checkpoint creation; depends on vsock-guest (or equivalent) for host communication.

## Takeaways for void-box

1. **Single protocol crate** (`vsock-proto`): shared encoding/decoding and message types for both host and guest; no JSON, binary only with length + type + seq + payload.
2. **Split host/guest crates** (`vsock-host`, `vsock-guest`): host is async (tokio), guest is sync with a single connection loop and optional reconnect.
3. **Sandbox as trait**: `Sandbox` in a small `sandbox` crate; `sandbox-fc` implements it; runner only depends on the trait and the FC implementation.
4. **Firecracker vsock → UDS**: On the host they don’t use raw AF_VSOCK; they use the FC vsock proxy, which appears as UDS. void-box uses real vhost-vsock + AF_VSOCK on the host.
5. **Ready/ping/pong**: Explicit handshake after connection before sending commands.
6. **Sequence numbers**: Every message has a seq for matching requests to responses and for unsolicited messages (e.g. process_exit) with seq=0.
7. **Rich exec**: timeout_ms, env list, and structured exec_result (exit_code, stdout, stderr) map cleanly onto their `ExecRequest`/`ExecResult` and could inspire void-box’s guest protocol (e.g. optional binary encoding alongside or instead of JSON).

## Applied in void-box

- **Ping/pong handshake** (VM0-style): After the host connects to the guest over vsock, the host sends a **Ping** and waits for **Pong** (5s read timeout) before sending the first `ExecRequest`. If the handshake fails, the host closes the connection and retries connect+handshake with backoff. This ensures the guest agent is ready before any exec. See `src/devices/virtio_vsock.rs` (`send_exec_request`). The initial delay before first connect was reduced from 8s to 3s since readiness is now verified by the handshake.
