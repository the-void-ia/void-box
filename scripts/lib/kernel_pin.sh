#!/usr/bin/env bash
# Single source of truth for the pinned Ubuntu guest kernel version.
#
# The VM kernel image (download_kernel.sh) and the kernel modules baked into
# the initramfs (guest_macos.sh / guest_linux.sh) must come from the same
# kernel release, or vsock/9p/overlay modules fail to load against a
# mismatched kernel. Defining the version once here keeps them in lockstep;
# every consumer sources this file and uses these as defaults.
#
# Ubuntu package versions look like 6.8.0-53.55 (base.upload):
#   VOIDBOX_KERNEL_VER     base version, e.g. "6.8.0-53"
#   VOIDBOX_KERNEL_UPLOAD  upload number, e.g. "55"
#
# When bumping: update these two values, then refresh the SHA-256 checksums
# in download_kernel.sh (they are version-specific and verified only there).
VOIDBOX_KERNEL_VER="6.8.0-53"
VOIDBOX_KERNEL_UPLOAD="55"
