# Sandbox and VoidBox capabilities

## Environment variables

- **VoidBox**: `exec_with_env(program, args, stdin, env, working_dir)` sends `env` and `working_dir` to the guest-agent. Use for e.g. `ANTHROPIC_API_KEY`, `WORKSPACE`, or project-specific config.
- **Sandbox**: `SandboxConfig::env` is a list of `(key, value)` pairs. Every `exec` / `exec_with_stdin` from a `Sandbox::local()` forwards this env into the guest. Set via builder: `.env("KEY", "value")`.

## Shared directory (planned)

- **Config**: `VoidBoxConfig::shared_dir` and `SandboxConfig::shared_dir` accept an optional host path.
- **Intended semantics** (when implemented): the host directory is exposed inside the guest at a fixed path, e.g. `/workspace`. Read-write unless specified otherwise. Used so that workflows (e.g. Claude Code) can edit host project files from inside the VM.
- **Status**: Not yet implemented. The VMM does not currently wire virtio-9p or similar; shared_dir is validated in config but has no runtime effect.

## Networking

- **Config**: `VoidBoxConfig::network`, `tap_name`; `SandboxConfig::network`.
- **Status**: When `network` is true, the VMM attaches a **virtio-net MMIO device** at `0xd000_0000` backed by **SLIRP** (smoltcp-based user-mode NAT). The virtqueue **TX path** is implemented: guest packets are read from the TX queue and passed to SLIRP. The **RX path** is implemented: SLIRP output is injected into the guest RX queue and the device raises an interrupt. The guest can use DHCP (10.0.2.2 gateway, 10.0.2.3 DNS) and reach the host network (e.g. `curl`, API calls). No root or TAP required. A separate `TapDevice` exists in `src/network/mod.rs` for future TAP/virtio-net attachment if needed.

## Host–guest vsock (guest exec)

- **Config**: `VoidBoxConfig::enable_vsock`, `cid`; the host connects to the guest agent on vsock port 1234.
- **Status**: The VMM now attaches a **virtio-vsock MMIO device** to the VM at `0xd080_0000` and wires it to the kernel vhost-vsock backend (SET_OWNER, SET_MEM_TABLE, SET_VRING_* when the guest driver configures the queues). The guest kernel can use the virtio_vsock driver; host `connect(CID, 1234)` should reach the guest agent when it is listening. If `/dev/vhost-vsock` is unavailable, vsock is disabled and the host will still time out—use the **mock sandbox** for demos without KVM/vhost.
