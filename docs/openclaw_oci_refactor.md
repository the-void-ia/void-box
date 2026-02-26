# OpenClaw + OCI Refactor Notes

## Scope

This refactor keeps OpenClaw examples working while making OCI rootfs boot more deterministic on Linux/KVM and preserving the existing security model.

## Changes kept

1. OCI unpack hardening (`voidbox-oci/src/unpack.rs`)
- Disable xattr unpack to avoid host capability/xattr failures.
- Skip special filesystem nodes (`block`, `char`, `fifo`) during host extraction.
- Keep whiteout handling strict (fail on real extraction errors).
- Hardlink fallback: copy regular file when hardlink creation is denied/unsupported.

2. OCI disk-rootfs path on Linux/KVM
- New backend config fields:
  - `oci_rootfs_dev` (guest block device, `/dev/vda`)
  - `oci_rootfs_disk` (host ext4 artifact path)
- Runtime builds and caches ext4 OCI disk artifacts keyed from image/rootfs identity.
- KVM wires virtio-blk and exposes MMIO device at `0xd1800000`, IRQ `13`.
- Kernel cmdline emits:
  - `virtio_mmio.device=512@0xd1800000:13`
  - `voidbox.oci_rootfs_dev=/dev/vda`

3. Guest boot/pivot behavior
- Guest-agent supports block-device lowerdir mount for OCI rootfs.
- Overlay setup remains tmpfs upper/work + read-only lowerdir.
- Root switch keeps `pivot_root` first; fallback to switch-root only on `EINVAL`.

## Changes intentionally reverted or minimized

1. Debug/noise changes
- Removed extra queue-notify logging noise in virtio-9p.
- Reverted SLIRP changes that were not part of OCI/OpenClaw objectives.

2. Over-permissive extraction behavior
- Removed broad `PermissionDenied => continue` unpack behavior.
- Extraction now fails on real whiteout/unpack filesystem errors.

## 9p vs virtio-blk policy

1. Use `virtio-blk` for `sandbox.image` rootfs on Linux/KVM.
2. Keep 9p/virtiofs for regular host mounts and skill mounts.
3. On macOS/VZ, keep mount-based path (virtiofs) for OCI rootfs.

## Security invariants preserved

1. `pivot_root`-based root switch remains enabled for OCI images.
2. Authenticated vsock control plane is still required before command execution.
3. Command allowlist and resource limits remain enforced by guest-agent.
4. OCI lowerdir is read-only; writable state is isolated in tmpfs overlay.

## Regression tests to run

```bash
export TMPDIR=$PWD/target/tmp
mkdir -p "$TMPDIR"
cargo test --test oci_integration
cargo test --test conformance
```

Linux VM path (requires artifacts + `/dev/kvm`):

```bash
export TMPDIR=$PWD/target/tmp
mkdir -p "$TMPDIR"
export VOID_BOX_KERNEL=/path/to/vmlinuz
export VOID_BOX_INITRAMFS=/path/to/rootfs.cpio.gz

cargo test --test oci_integration vm_oci_rootfs_mount_visible -- --ignored --exact --nocapture
cargo test --test oci_integration vm_oci_rootfs_readonly -- --ignored --exact --nocapture
cargo test --test oci_integration vm_oci_alpine_os_release -- --ignored --exact --nocapture
```
