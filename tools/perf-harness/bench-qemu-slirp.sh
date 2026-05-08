#!/usr/bin/env bash
# bench-qemu-slirp.sh — qemu-side of the proper SLIRP-vs-SLIRP head-to-head.
#
# Boots a minimal qemu guest with the static crr-client baked in, runs N
# TCP CRRs against a host TCP server, and prints `n p50_ns p99_ns mean_ns`.
#
# Two backends:
#   --backend libslirp    qemu's built-in -netdev user (libslirp)
#   --backend passt       qemu -netdev stream + a passt(1) instance over UNIX socket
#
# Both produce a number directly comparable to tools/perf-harness/bench-pasta.py's
# pasta-side number AND to examples/crr_singleproc_bench.rs's voidbox-side
# number — same workload, same C client, same iteration count.
#
# Why this exists:  voidbox-vs-pasta comparisons mix two different
# architectures (a real VM vs a netns).  The right SLIRP-vs-SLIRP comparison
# is voidbox+voidbox-SLIRP vs qemu+passt vs qemu+libslirp — all VM-attached.
# See docs/passt-comparison.md.

set -euo pipefail

BACKEND=libslirp
ITERATIONS=30
CONCURRENCY=1
KERNEL=${KERNEL:-/boot/vmlinuz-$(uname -r)}
# NB: must be the `passt` binary (VM/socket mode), NOT the `pasta` symlink
# (namespace mode).  The two modes are the same code keyed on argv[0].
# Default discovery order: $PASST env var → `passt` on $PATH → /usr/bin/passt.
default_passt() {
  if command -v passt >/dev/null 2>&1; then
    command -v passt
  else
    echo /usr/bin/passt
  fi
}
PASST=${PASST:-$(default_passt)}
HOST_PORT=${HOST_PORT:-18877}
GUEST_ADDR=${GUEST_ADDR:-10.0.2.15}
GUEST_GATEWAY=${GUEST_GATEWAY:-10.0.2.2}
CRR_CLIENT_BIN=${CRR_CLIENT_BIN:-/tmp/crr-client}
ROOTFS_DIR=${ROOTFS_DIR:-}
KEEP_ROOTFS=${KEEP_ROOTFS:-0}

usage() {
  cat <<EOF
Usage: $0 [--backend libslirp|passt] [--iterations N] [--concurrency M]
          [--kernel PATH] [--port PORT]

Options:
  --backend       libslirp | passt                 (default: libslirp)
  --iterations    CRRs per flow                    (default: 30)
  --concurrency   number of concurrent crr-client  (default: 1)
                  flows in the guest
  --kernel        path to a Linux bzImage          (default: host distro kernel)
  --port          host TCP listener port           (default: 18877)
  --rootfs-dir    pre-built rootfs dir to reuse    (default: mktemp)
  --keep          keep the rootfs after the run    (default: cleanup)

Env vars:
  KERNEL          path to a Linux bzImage (default: host distro kernel)
  PASST           path to the passt binary (default: \`passt\` on \$PATH, falling back to /usr/bin/passt)
  CRR_CLIENT_BIN  path to the static crr-client binary (default: /tmp/crr-client)
  HOST_PORT       TCP port for the host listener (default: 18877)
  GUEST_ADDR      IPv4 to assign to the guest (default: 10.0.2.15)
  GUEST_GATEWAY   IPv4 the guest treats as host loopback (default: 10.0.2.2)

Output:
  --concurrency 1: one line "n p50_ns p99_ns mean_ns" on stdout.
  --concurrency M (M>1): one line "<flow_id> n p50_ns p99_ns mean_ns" per flow
                  on stdout, plus the aggregate summary on stderr.
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --backend)     BACKEND="$2"; shift 2 ;;
    --iterations)  ITERATIONS="$2"; shift 2 ;;
    --concurrency) CONCURRENCY="$2"; shift 2 ;;
    --kernel)      KERNEL="$2"; shift 2 ;;
    --port)        HOST_PORT="$2"; shift 2 ;;
    --rootfs-dir)  ROOTFS_DIR="$2"; shift 2 ;;
    --keep)        KEEP_ROOTFS=1; shift ;;
    -h|--help)     usage; exit 0 ;;
    *)             echo "unknown arg: $1" >&2; usage; exit 1 ;;
  esac
done

case "$BACKEND" in
  libslirp|passt) : ;;
  *) echo "unknown backend: $BACKEND" >&2; exit 1 ;;
esac

[[ -x "$CRR_CLIENT_BIN" ]] || {
  echo "ERROR: crr-client not found at $CRR_CLIENT_BIN" >&2
  echo "       compile it with: gcc -O2 -static -o $CRR_CLIENT_BIN tools/perf-harness/crr-client.c" >&2
  exit 2
}

[[ -r "$KERNEL" ]] || { echo "ERROR: kernel not readable: $KERNEL" >&2; exit 2; }

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
INIT_TEMPLATE="$SCRIPT_DIR/qemu-init.sh"
[[ -r "$INIT_TEMPLATE" ]] || { echo "ERROR: missing $INIT_TEMPLATE" >&2; exit 2; }

# ---------------------------------------------------------------------------
# Build the initramfs.  Keep it on tmpfs so it doesn't pollute the workspace.
# ---------------------------------------------------------------------------
if [[ -z "$ROOTFS_DIR" ]]; then
  ROOTFS_DIR=$(mktemp -d -t voidbox-qemu-rootfs.XXXXXX)
  cleanup_rootfs() {
    if [[ "$KEEP_ROOTFS" -eq 0 ]]; then rm -rf "$ROOTFS_DIR"; fi
  }
  trap cleanup_rootfs EXIT
fi

mkdir -p "$ROOTFS_DIR"/{bin,sbin,proc,sys,dev,tmp}

# Static busybox: prefer host /usr/bin/busybox (Fedora ships static); fall back
# to extracting from voidbox's claude rootfs if needed.
if [[ -x /usr/bin/busybox ]] && file /usr/bin/busybox 2>/dev/null | grep -q "statically linked"; then
  cp /usr/bin/busybox "$ROOTFS_DIR/bin/busybox"
elif [[ -r "$SCRIPT_DIR/../../target/void-box-claude.cpio.gz" ]]; then
  (cd "$ROOTFS_DIR" && zcat "$SCRIPT_DIR/../../target/void-box-claude.cpio.gz" | cpio -idm bin/busybox 2>/dev/null)
else
  echo "ERROR: no static busybox found; install busybox-static or build target/void-box-claude.cpio.gz" >&2
  exit 2
fi

cp "$INIT_TEMPLATE" "$ROOTFS_DIR/init"
chmod +x "$ROOTFS_DIR/init"
cp "$CRR_CLIENT_BIN" "$ROOTFS_DIR/tmp/crr-client"

for cmd in sh ifconfig route poweroff cat sleep echo mount find ls insmod rm mkdir; do
  ln -sf busybox "$ROOTFS_DIR/bin/$cmd"
done

# Stage virtio_net + failover modules from the host kernel so the distro-kernel
# path can probe the qemu virtio-net-pci device.  Voidbox's slim kernel has
# them built-in and ignores these.
KMOD_DIR="/lib/modules/$(uname -r)/kernel"
if [[ -d "$KMOD_DIR" ]]; then
  KGUEST_DIR="$ROOTFS_DIR/lib/modules/$(uname -r)"
  mkdir -p "$KGUEST_DIR"
  for mod in net/core/failover.ko.xz net/core/failover.ko \
             drivers/net/net_failover.ko.xz drivers/net/net_failover.ko \
             drivers/net/virtio_net.ko.xz drivers/net/virtio_net.ko; do
    [[ -r "$KMOD_DIR/$mod" ]] && cp "$KMOD_DIR/$mod" "$KGUEST_DIR/"
  done
fi

INITRD=$(mktemp -t voidbox-qemu-initrd.XXXXXX.cpio.gz)
trap "rm -f $INITRD; ${cleanup_rootfs:-true}" EXIT
(cd "$ROOTFS_DIR" && find . | cpio -H newc -o 2>/dev/null | gzip > "$INITRD")

# ---------------------------------------------------------------------------
# Host-side echo server.  The script's outer EXIT trap kills it, so the
# server stays alive for the entire qemu run rather than racing against a
# fixed-duration sleep.  HOST_PORT must be free; the script fails fast if
# bind() refuses (no fallback to ephemeral — the guest's kernel cmdline
# carries the port and changing it after launch isn't useful).
# ---------------------------------------------------------------------------
SERVER_PIDFILE=$(mktemp)
python3 - <<PY &
import os, signal, socket, sys, threading
port = int(os.environ.get("HOST_PORT", "$HOST_PORT"))
s = socket.socket()
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
try:
    s.bind(("127.0.0.1", port))
except OSError as e:
    sys.stderr.write(f"echo-server: bind 127.0.0.1:{port} failed: {e}\n")
    sys.exit(2)
s.listen(64)
sys.stderr.write(f"echo-server: bound 127.0.0.1:{port}\n"); sys.stderr.flush()
def loop():
    while True:
        try: c, _ = s.accept()
        except OSError: return
        try:
            c.recv(1); c.sendall(b"x")
        except OSError: pass
        finally: c.close()
threading.Thread(target=loop, daemon=True).start()
# Block on an event that nothing ever sets — the parent script's EXIT
# trap kills us when qemu finishes (or when SIGTERM fires on outer
# timeout).  Before this fix the server exited after 60s while qemu's
# own boot+run could approach that limit, racing the harness.
threading.Event().wait()
PY
SERVER_PID=$!
echo "$SERVER_PID" > "$SERVER_PIDFILE"
trap "kill $SERVER_PID 2>/dev/null; rm -f $INITRD $SERVER_PIDFILE; ${cleanup_rootfs:-true}" EXIT
sleep 0.3

# ---------------------------------------------------------------------------
# Backend: spin up passt if requested.
# ---------------------------------------------------------------------------
PASST_PID=""
PASST_SOCK=""
NETDEV_ARGS=""
case "$BACKEND" in
  libslirp)
    NETDEV_ARGS="-netdev user,id=n0 -device virtio-net-pci,netdev=n0"
    ;;
  passt)
    [[ -x "$PASST" ]] || { echo "ERROR: passt not executable: $PASST" >&2; exit 2; }
    # Force /tmp regardless of $TMPDIR — passt's SELinux policy on
    # Fedora (`passt_exec_t`) only allows it to bind UNIX sockets
    # under standard system tmp paths.  When $TMPDIR points
    # elsewhere (the void-box validation contract sets it to
    # $PWD/target/tmp), passt fails with "Permission denied" on
    # bind and the harness times out with no result.
    PASST_SOCK=$(mktemp -u -p /tmp voidbox-passt.XXXXXX.sock)
    rm -f "$PASST_SOCK"
    "$PASST" -f -s "$PASST_SOCK" \
      -a "$GUEST_ADDR" -n 24 -g "$GUEST_GATEWAY" \
      --map-host-loopback "$GUEST_GATEWAY" \
      -q >/tmp/passt.log 2>&1 &
    PASST_PID=$!
    sleep 0.4
    [[ -S "$PASST_SOCK" ]] || { echo "ERROR: passt socket not created" >&2; exit 3; }
    NETDEV_ARGS="-netdev stream,id=n0,addr.type=unix,addr.path=$PASST_SOCK -device virtio-net-pci,netdev=n0"
    trap "kill $SERVER_PID $PASST_PID 2>/dev/null; rm -f $INITRD $SERVER_PIDFILE $PASST_SOCK; ${cleanup_rootfs:-true}" EXIT
    ;;
esac

# ---------------------------------------------------------------------------
# Boot qemu, capture serial output.
# ---------------------------------------------------------------------------
QEMU_LOG=$(mktemp -t voidbox-qemu.XXXXXX.log)
trap "kill ${SERVER_PID} ${PASST_PID:-} 2>/dev/null; rm -f $INITRD $SERVER_PIDFILE $QEMU_LOG ${PASST_SOCK:-}; ${cleanup_rootfs:-true}" EXIT

# shellcheck disable=SC2086
HOST_PORT="$HOST_PORT" timeout 600 qemu-system-x86_64 \
  -enable-kvm -cpu host -m 512 -smp 1 \
  -kernel "$KERNEL" \
  -initrd "$INITRD" \
  -nographic -no-reboot \
  -append "console=ttyS0 reboot=t panic=1 quiet crr_target=${GUEST_GATEWAY}:${HOST_PORT}:${ITERATIONS} crr_net=${GUEST_ADDR}/24,${GUEST_GATEWAY} crr_concurrency=${CONCURRENCY}" \
  $NETDEV_ARGS \
  > "$QEMU_LOG" 2>&1 || true

if [[ "$CONCURRENCY" -eq 1 ]]; then
  # Single-flow path emits exactly one summary line `n p50 p99 mean`.
  RESULT=$(sed -n '/===CRR-START===/,/===CRR-END/p' "$QEMU_LOG" | grep -E '^[0-9]+ [0-9]+ [0-9]+ [0-9]+$' | head -1 || true)
else
  # Multi-flow path emits M lines `<flow_id> n p50 p99 mean` between
  # the sentinels.  Capture all of them; aggregation happens below.
  RESULT=$(sed -n '/===CRR-START===/,/===CRR-END/p' "$QEMU_LOG" | grep -E '^[0-9]+ [0-9]+ [0-9]+ [0-9]+ [0-9]+$' || true)
fi

if [[ -z "$RESULT" ]]; then
  echo "ERROR: no result from guest (qemu log tail follows):" >&2
  tail -20 "$QEMU_LOG" >&2
  exit 4
fi

if [[ "$CONCURRENCY" -eq 1 ]]; then
  read -r N P50_NS P99_NS MEAN_NS <<<"$RESULT"
  P50_US=$((P50_NS / 1000))
  P99_US=$((P99_NS / 1000))
  MEAN_US=$((MEAN_NS / 1000))
  echo "qemu+${BACKEND} CRR over $N iterations: p50=${P50_US} µs, p99=${P99_US} µs, mean=${MEAN_US} µs" >&2
  echo "$RESULT"
  exit 0
fi

# Multi-flow aggregation: report median-of-p50s, max-p99,
# mean-of-means, aggregate qps — same shape as
# `examples/crr_concurrent_bench` and `bench-pasta-concurrent.py`.
echo "qemu+${BACKEND} concurrent CRR: $CONCURRENCY flows × $ITERATIONS iterations" >&2
flow_count=0
total_iters=0
total_mean_ns=0
echo "$RESULT" | sort -n | while read -r flow_id n p50_ns p99_ns mean_ns; do
  echo "  flow $flow_id ($n iters): p50=$((p50_ns / 1000)) µs  p99=$((p99_ns / 1000)) µs  mean=$((mean_ns / 1000)) µs" >&2
done
median_p50_us=$(echo "$RESULT" | awk '{print $3}' | sort -n | awk '
  { lines[NR] = $1 }
  # awk arrays are 1-indexed; matches the Rust/Python convention of
  # `len // 2` (upper middle on even-length sequences) so the
  # cross-stack comparison reports the same statistic.
  END { print int(lines[int(NR / 2) + 1] / 1000) }
')
max_p99_us=$(echo "$RESULT" | awk '{print $4}' | sort -n | tail -1 | awk '{print int($1 / 1000)}')
mean_of_means_us=$(echo "$RESULT" | awk '{ sum += $5; n += 1 } END { if (n > 0) print int(sum / n / 1000); else print 0 }')
total_completed=$(echo "$RESULT" | awk '{ sum += $2 } END { print sum }')

# Note: aggregate qps is approximated by total_completed / qemu_wall.
# We don't capture qemu_wall precisely here (the timeout wraps the
# whole run including initramfs build + boot), so the per-flow p50
# is the more reliable comparison metric.
echo "  median-of-p50s:  ${median_p50_us} µs" >&2
echo "  max p99:         ${max_p99_us} µs" >&2
echo "  mean-of-means:   ${mean_of_means_us} µs" >&2
echo "  total CRRs:      ${total_completed}" >&2
# Stdout: machine-readable per-flow lines (already in $RESULT).
echo "$RESULT"
