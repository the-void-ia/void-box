#!/bin/sh
# tools/perf-harness/qemu-init.sh — /init for the SLIRP-vs-SLIRP comparison guest.
#
# Used by tools/perf-harness/bench-qemu-slirp.sh.  Read /proc/cmdline for:
#   crr_target=HOST:PORT:N      target server + iteration count
#   crr_net=ADDR/MASK,GW        static network config
#   crr_concurrency=M           number of concurrent crr-client processes
#                               (default 1; >1 forks M backgrounded
#                               clients and concatenates their summary
#                               lines as `<flow_id> n p50 p99 mean`)
#
# Bring up eth0 with the static IP, run /tmp/crr-client, and halt.
# The script is paranoid about busybox-vs-distro variations: virtio-net
# is loaded as a module if present (Fedora-style), or assumed built-in
# (voidbox's slim kernel).

set +e
mount -t proc proc /proc 2>/dev/null
mount -t sysfs sysfs /sys 2>/dev/null

cmdline="$(cat /proc/cmdline)"
target=""
net=""
concurrency=1
for tok in $cmdline; do
  case "$tok" in
    crr_target=*)      target="${tok#crr_target=}" ;;
    crr_net=*)         net="${tok#crr_net=}" ;;
    crr_concurrency=*) concurrency="${tok#crr_concurrency=}" ;;
  esac
done

if [ -z "$target" ] || [ -z "$net" ]; then
  echo "ERROR: missing crr_target or crr_net on cmdline"
  echo "cmdline: $cmdline"
  poweroff -f
fi

addr_mask="${net%,*}"
gw="${net#*,}"
host="${target%%:*}"
rest="${target#*:}"
port="${rest%%:*}"
n="${rest#*:}"

busybox ifconfig lo up

# Load virtio modules if shipped in the rootfs (distro-kernel case).
# Voidbox's slim kernel has them built-in so insmod fails harmlessly.
for mod in failover net_failover virtio_net; do
  busybox find /lib/modules -name "${mod}.ko*" -exec busybox insmod {} \; 2>/dev/null
done

i=0
while [ $i -lt 30 ] && ! busybox ifconfig eth0 >/dev/null 2>&1; do
  sleep 0.1
  i=$((i+1))
done

# Derive the netmask from the /N suffix instead of hard-coding /24:
# crr_net is documented as ADDR/MASK,GW and a future call site might
# reasonably use /16 or /29.  Falls back to /24 if the suffix isn't
# parseable so existing setups keep working.
addr="${addr_mask%/*}"
prefix="${addr_mask#*/}"
case "$prefix" in
  8)  mask=255.0.0.0 ;;
  16) mask=255.255.0.0 ;;
  24) mask=255.255.255.0 ;;
  29) mask=255.255.255.248 ;;
  30) mask=255.255.255.252 ;;
  *)  mask=255.255.255.0 ;;
esac
busybox ifconfig eth0 "$addr" netmask "$mask" up
busybox route add default gw "$gw"

echo "===CRR-START==="
echo "addr=${addr_mask} gw=${gw} target=${host}:${port} n=${n} M=${concurrency}"

# Reject obviously-bad concurrency values upfront so the shell
# loops below don't expand into invalid scripts (e.g. a `while`
# bounded by `0` or non-numeric input).
case "$concurrency" in
  ''|*[!0-9]*|0)
    rc=2
    echo "ERROR: invalid crr_concurrency=$concurrency (must be a positive integer)"
    echo "===CRR-END (rc=$rc)==="
    poweroff -f
    ;;
esac

if [ "$concurrency" = "1" ]; then
  # Backwards-compatible single-flow path: emit the same one-line
  # `n p50 p99 mean` shape bench-qemu-slirp.sh already parses.
  /tmp/crr-client "$host" "$port" "$n"
  rc=$?
else
  # Multi-flow: spawn M background clients, capture each PID,
  # and `wait $PID` individually.  busybox's `wait $PID` returns
  # the child's exit status, unlike a bare `wait` which only
  # waits and discards.  This way a crashing crr-client surfaces
  # as a non-zero `rc` even if it printed a partial summary line.
  rm -rf /tmp/crr_results
  mkdir -p /tmp/crr_results
  pids=""
  i=1
  while [ "$i" -le "$concurrency" ]; do
    /tmp/crr-client "$host" "$port" "$n" > "/tmp/crr_results/$i.txt" &
    pids="$pids $!"
    i=$((i + 1))
  done
  rc=0
  for pid in $pids; do
    if ! wait "$pid"; then
      rc=2
    fi
  done
  i=1
  while [ "$i" -le "$concurrency" ]; do
    line="$(cat /tmp/crr_results/$i.txt 2>/dev/null)"
    if [ -z "$line" ]; then
      echo "ERROR: empty result for flow $i"
      rc=2
    else
      echo "$i $line"
    fi
    i=$((i + 1))
  done
fi
echo "===CRR-END (rc=$rc)==="

poweroff -f
