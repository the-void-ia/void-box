#!/bin/sh
# tools/qemu-init.sh — /init for the SLIRP-vs-SLIRP comparison guest.
#
# Used by scripts/bench-qemu-slirp.sh.  Read /proc/cmdline for:
#   crr_target=HOST:PORT:N      target server + iteration count
#   crr_net=ADDR/MASK,GW        static network config
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
for tok in $cmdline; do
  case "$tok" in
    crr_target=*) target="${tok#crr_target=}" ;;
    crr_net=*)    net="${tok#crr_net=}" ;;
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

busybox ifconfig eth0 "${addr_mask%/*}" netmask 255.255.255.0 up
busybox route add default gw "$gw"

echo "===CRR-START==="
echo "addr=${addr_mask} gw=${gw} target=${host}:${port} n=${n}"
/tmp/crr-client "$host" "$port" "$n"
rc=$?
echo "===CRR-END (rc=$rc)==="

poweroff -f
