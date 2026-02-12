# Guest image design for void-box

## Purpose

The guest rootfs/initramfs is the minimal Linux userspace that boots inside the micro-VM. It must provide:

1. **Init process**: `/init` that starts the void-box guest-agent (PID 1).
2. **Guest-agent**: `/sbin/guest-agent` — listens on vsock and runs commands.
3. **Optional Claude Code mock**: `/usr/local/bin/claude-code` for workflow demos.
4. **Shell and basic tools**: For the agent to run `sh`, `echo`, `cat`, `tr`, etc., the rootfs should include a minimal userland (e.g. BusyBox).

## Layout

```
/
├── init              # → exec /sbin/guest-agent
├── sbin/
│   └── guest-agent   # Rust binary
├── bin/              # optional: busybox or static sh
│   └── sh
├── usr/local/bin/
│   └── claude-code   # mock or real Claude CLI
├── proc, sys, dev, tmp  # created by guest-agent or init
```

## Reproducible build

- **Script**: [`scripts/build_guest_image.sh`](../scripts/build_guest_image.sh) builds the rootfs and produces an initramfs (cpio.gz).
- **Inputs**:
  - `guest-agent`: built with `cargo build --release -p guest-agent`.
  - `scripts/guest/claude-code-mock.sh`: copied as `/usr/local/bin/claude-code`.
  - Optional: set `BUSYBOX=/path/to/busybox` to include a static busybox binary so the guest has `/bin/sh` and basic utilities. Without it, the VM may boot but commands like `echo` will not be available unless provided by another means.

## Usage

```bash
# Build initramfs (output default: /tmp/void-box-rootfs.cpio.gz)
./scripts/build_guest_image.sh

# With custom output
OUT_DIR=/tmp/my-rootfs OUT_CPIO=/tmp/my.cpio.gz ./scripts/build_guest_image.sh

# With BusyBox for full shell support (download or build busybox first)
BUSYBOX=/path/to/busybox ./scripts/build_guest_image.sh
```

## Integration with run_kvm_tests.sh

`scripts/run_kvm_tests.sh` can call `build_guest_image.sh` to produce the initramfs, or use its own minimal rootfs. For tests that need `echo`/`cat`/`tr` inside the guest, the image must include a shell and those utilities (e.g. via BusyBox).
