# VM0 network implementation (crates/sandbox-fc/network)

How [vm0-ai/vm0](https://github.com/vm0-ai/vm0) does networking for Firecracker VMs: namespace pool, TAP, veth, routing, and iptables.

## Overview

- Each VM runs in its **own network namespace**.
- Inside the namespace: a **TAP** device (guest-facing) and a **veth** pair (host-facing).
- Guest uses a **fixed IP** in every VM (`192.168.241.2`); isolation comes from namespaces, so no conflict.
- Outbound traffic: guest → TAP → NAT in namespace → veth peer → host veth → **iptables MASQUERADE** → default interface → internet.
- Optional **HTTP/HTTPS proxy**: iptables PREROUTING redirects port 80/443 from guest to a proxy port.

## Layout (from pool.rs comments)

```
┌─────────────────────┐  ┌─────────────────────┐
│     Namespace 1     │  │     Namespace 2     │
│ ┌─────────────────┐ │  │ ┌─────────────────┐ │
│ │       VM        │ │  │ │       VM        │ │
│ │  192.168.241.2  │ │  │ │  192.168.241.2  │ │  ← Same fixed IP
│ └────────┬────────┘ │  │ └────────┬────────┘ │
│          │ TAP      │  │          │ TAP      │
│    192.168.241.1    │  │    192.168.241.1    │
│          │          │  │          │          │
│      NAT/MASQ       │  │      NAT/MASQ       │
│          │ veth0    │  │          │ veth0    │
│      10.200.0.2     │  │      10.200.0.6     │  ← Unique veth IP
└──────────┼──────────┘  └──────────┼──────────┘
           │ veth-host              │ veth-host
       10.200.0.1               10.200.0.5
           │                        │
           └──────────┬─────────────┘
                      │ NAT/MASQ
                      ↓
                External Network
```

## Guest-facing config (`network/guest.rs`)

Fixed for all VMs (same in every namespace):

| Field        | Value              | Meaning                          |
|-------------|--------------------|----------------------------------|
| `tap_name`  | `"vm0-tap"`        | TAP device name in the namespace |
| `guest_mac` | `"02:00:00:00:00:01"` | Guest MAC (Firecracker config) |
| `guest_ip`  | `"192.168.241.2"`  | Guest IP inside VM               |
| `gateway_ip`| `"192.168.241.1"`  | TAP IP in namespace (guest’s gw) |
| `netmask`   | `"255.255.255.248"`| /29                              |
| `prefix_len`| `29`               | For `ip` commands                |

Kernel boot args set the guest’s IP at boot (no DHCP):

```text
ip=192.168.241.2::192.168.241.1:255.255.255.248:vm0-guest:eth0:off
```

Firecracker network config references the same TAP and MAC:

```json
"network-interfaces": [
  {
    "iface_id": "eth0",
    "guest_mac": "02:00:00:00:00:01",
    "host_dev_name": "vm0-tap"
  }
]
```

So: one TAP per namespace, same name and same guest IP in every VM; isolation is by namespace.

## Namespace pool (`network/pool.rs`)

### Pool index (0–63)

- **Flock** on files under a lock dir: `vm0-netns-pool-{index}.lock`.
- At factory create, they take the first available index (0..64). That index is held for the lifetime of the pool.
- Prevents two runner processes from reusing the same pool index and IP range.

### Naming

- Namespace: `vm0-ns-{pool_idx:02x}-{ns_idx:02x}` (e.g. `vm0-ns-00-0a`).
- Host veth: `vm0-ve-{pool_idx:02x}-{ns_idx:02x}`.
- Peer inside namespace: fixed name `veth0`.

### IP allocation (veth, /30 per namespace)

From `10.200.0.0/16`:

- `octet3 = pool_idx * 4 + ns_idx / 64`
- `octet4_base = (ns_idx % 64) * 4`
- Host side: `10.200.{octet3}.{octet4_base + 1}`
- Peer (in namespace): `10.200.{octet3}.{octet4_base + 2}`

Examples: (0,0) → host `10.200.0.1`, peer `10.200.0.2`; (0,1) → `10.200.0.5`, `10.200.0.6`; (1,0) → `10.200.4.1`, `10.200.4.2`. So each namespace gets a distinct /30.

### NetnsPool API

- **`NetnsPool::create(config)`**: acquires pool index via flock, enables host `net.ipv4.ip_forward=1`, cleans orphaned namespaces for that index, detects default interface, pre-creates `config.size` namespaces in parallel.
- **`acquire()`**: pops a namespace from the queue, or creates one on-demand (until `MAX_NAMESPACES` per pool).
- **`release(ns)`**: puts the namespace back in the queue (or deletes it if the pool is already inactive).
- **`cleanup()`**: marks pool inactive and deletes all namespaces still in the queue (acquired-but-not-released are left; next `create` with same index will clean orphans).

## Per-namespace setup (order of operations)

1. **Create namespace and TAP**  
   - `ip netns add {ns_name}`  
   - Inside ns: `ip tuntap add vm0-tap mode tap`  
   - Inside ns: assign `192.168.241.1/29` to `vm0-tap`, bring tap and `lo` up.

2. **Veth pair (host ↔ namespace)**  
   - `ip link add {host_device} type veth peer name veth0 netns {ns_name}`  
   - In ns: assign peer’s /30 to `veth0`, set up.  
   - On host: assign host’s /30 to `{host_device}`, set up.

3. **Routing and NAT inside namespace**  
   - In ns: default route `via {host_ip}` (host end of veth).  
   - In ns: iptables NAT POSTROUTING: `-s 192.168.241.0/29 -o veth0 -j MASQUERADE`.  
   - In ns: `sysctl -w net.ipv4.ip_forward=1`.

4. **Host iptables**  
   - NAT POSTROUTING: `-s {peer_ip}/30 -o {default_iface} -j MASQUERADE` (comment = ns name).  
   - FORWARD: allow `-i {host_device} -o {default_iface}` and `-i {default_iface} -o {host_device} -m state --state RELATED,ESTABLISHED` (comment = ns name).  
   - If `proxy_port` is set: PREROUTING (in nat) for `-s {peer_ip}/30 -p tcp --dport 80` and `--dport 443` → REDIRECT to proxy port (comment = ns name).

All commands run via **sudo** (e.g. `exec("ip", args, Privilege::Sudo)`). Default interface is from `ip route get 8.8.8.8` and then “dev” field.

## Firecracker process placement

Firecracker is run **inside** the namespace so it sees the TAP:

```bash
sudo ip netns exec {ns_name} sudo -u {user} firecracker --config-file {path} --no-api
```

So the process is in the same network namespace where `vm0-tap` and `veth0` live; the VM’s virtio-net uses that TAP.

## Cleanup

- **Per-namespace**: delete iptables rules whose comment equals the namespace name (both nat and filter), then `ip link del {host_device}` and `ip netns del {ns_name}`.
- **Orphan cleanup**: when creating a pool with a given index, they first delete all iptables rules whose comment contains the pool prefix (`vm0-ns-{idx}-`), then list `ip netns list`, delete matching namespaces and their host veth devices.

## Optional proxy

- `FirecrackerConfig::proxy_port: Option<u16>`.
- When set, `setup_host_iptables` adds nat PREROUTING rules to redirect TCP 80 and 443 from the guest’s veth /30 to that port (on the host). Used to force guest HTTP(S) through a proxy.

## Comparison with void-box

| Aspect            | VM0 (Firecracker)                    | void-box (KVM + SLIRP)           |
|------------------|--------------------------------------|-----------------------------------|
| Isolation         | One netns per VM                     | One process, no netns             |
| Guest IP          | Fixed 192.168.241.2 (kernel cmdline) | SLIRP 10.0.2.15 (user-mode NAT)  |
| Host networking   | TAP + veth + iptables MASQUERADE     | SLIRP in process, no TAP/iptables |
| Root/sudo         | Required (ip, iptables, netns)       | Not required for network          |
| Outbound          | Full NAT to default route            | SLIRP NAT to host                 |
| Proxy             | iptables REDIRECT 80/443 → port      | Not built-in                      |

VM0’s model gives strong isolation and real TAP/veth/iptables; void-box keeps everything in-process with SLIRP and no host netns or TAP. If void-box ever adds a “real” NIC (e.g. TAP) or multiple VMs, VM0’s pool + namespace + iptables pattern is a good reference.
