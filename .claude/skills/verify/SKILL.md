---
name: verify
description: Run the full quality gate for this repo — format check, clippy, tests, security audit, startup bench regression, and real-workload smoke (HN agent + openclaw gateway) when secrets are available. Invoke before marking any implementation task done or pushing a branch.
---

Run these checks in order. Stop and report at the first failure.

**1. Format check**
```
cargo fmt --all -- --check
```

**2. Clippy (platform-aware)**

On macOS (excludes guest-agent, which is Linux-only):
```
cargo clippy --workspace --exclude guest-agent --all-targets --all-features -- -D warnings
```

On Linux:
```
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

**3. Tests (platform-aware)**

On macOS (excludes guest-agent, which is Linux-only):
```
cargo test --workspace --exclude guest-agent --all-features --verbose
cargo test --doc --workspace --exclude guest-agent --all-features
```

On Linux:
```
cargo test --workspace --all-features --verbose
cargo test --doc --workspace --all-features
```

Note: Integration and E2E tests (conformance, snapshot, e2e_*) require `VOID_BOX_KERNEL` and `VOID_BOX_INITRAMFS` to be set and use `--ignored --test-threads=1`. Only run them if the user requests VM-level validation.

**4. Security audit**
```
cargo audit --deny warnings
```

**5. Startup bench regression gate** (required before push; thresholds differ by host)

Guards against regressions in the subsecond startup path. Thresholds are
host-specific because Linux/KVM and macOS/VZ have different floors —
VZ cold time is dominated by Hypervisor.framework setup, not kernel init,
so the slim kernel helps much less on macOS.

On Linux (fail if cold p50 > 400 ms or warm p50 > 200 ms):

```
cargo build --release --bin voidbox-startup-bench
export VOID_BOX_KERNEL=$PWD/target/vmlinux-slim-x86_64
export VOID_BOX_INITRAMFS=/tmp/void-box-test-rootfs.cpio.gz
./target/release/voidbox-startup-bench --iters 20 --breakdown 2>&1 | \
  tee target/tmp/verify_bench.log | grep -E "^(cold|warm)\.total"
```

On macOS/arm64 — M-series (fail if cold p50 > 2.2 s or warm p50 > 320 ms).
Thresholds are provisional, derived from a single n=10 baseline; re-measure
at n=20 on the target host before treating them as hard gates. Intel
Mac / VZ has no baseline yet; skip this step or measure locally first.
Use `cargo run` so the `.cargo/config.toml` runner codesigns the bench
binary automatically — direct invocation of `target/release/...`
skips the runner and fails with a `com.apple.security.virtualization`
entitlement error:

```
export VOID_BOX_KERNEL=$PWD/target/vmlinux-slim-aarch64
export VOID_BOX_INITRAMFS=/tmp/void-box-test-rootfs.cpio.gz
cargo run --release --bin voidbox-startup-bench -- --iters 20 --breakdown 2>&1 | \
  tee target/tmp/verify_bench.log | grep -E "^(cold|warm)\.total"
```

- If `vmlinux-slim-<arch>` is missing, run `scripts/build_slim_kernel.sh`
  first (10 min cold; cached thereafter). On macOS the script
  auto-re-execs inside an `ubuntu:24.04` container — requires Docker
  Desktop running.
- If the test rootfs is missing, run `scripts/build_test_image.sh`.
- Reference numbers:
  - Fedora 43 / KVM / slim x86_64: cold p50 **≈ 252 ms / p95 ≈ 260 ms**,
    warm p50 **≈ 138 ms / p95 ≈ 144 ms**.
  - M-series / VZ / slim aarch64: cold p50 **≈ 1.9 s**, warm p50 **≈ 282 ms**
    (n=10 baseline; re-measure at n=20 and tune thresholds when you have a
    stable sample).
- If the bench hangs or produces EAGAIN within 30 s, skip to
  `superpowers:systematic-debugging` — do not push until diagnosed.

**6. Real-workload smoke** (Linux only, required before push when secrets are available)

Small RPCs dominate day-to-day testing, so regressions in the host→guest
path for payloads >4 KiB have slipped through before (see
`fix/vsock-host-to-guest-packetize`). These two specs exercise the
full production path end-to-end:

```
# HN researcher (Claude agent + real HN API via curl+jq, writes output.md)
ANTHROPIC_API_KEY=… \
VOID_BOX_KERNEL=$PWD/target/vmlinux-slim-x86_64 \
VOID_BOX_INITRAMFS=$PWD/target/void-box-claude.cpio.gz \
timeout 300 ./target/release/voidbox run \
  --file examples/hackernews/hackernews_agent.yaml \
  > target/tmp/verify_hn.log 2>&1

# OpenClaw Telegram gateway (verify + configure + smoke_message posting to Telegram)
ANTHROPIC_API_KEY=… TELEGRAM_BOT_TOKEN=… TELEGRAM_CHAT_ID=… \
VOID_BOX_KERNEL=$PWD/target/vmlinux-slim-x86_64 \
VOID_BOX_INITRAMFS=$PWD/target/void-box-claude.cpio.gz \
timeout 180 ./target/release/voidbox run \
  --file examples/openclaw/openclaw_telegram.yaml \
  > target/tmp/verify_openclaw.log 2>&1
```

Pass criteria:
- HN — log contains at least one `tool: Bash` invocation and one
  `tool: Write` targeting `/workspace/output.md` (agent completed the
  research round-trip).
- OpenClaw — log contains `step 3/4: "smoke_message" ok` (the
  "OpenClaw prebuilt gateway started" Telegram message was posted).
- Neither log should contain `control_channel: deadline reached` or
  `Resource temporarily unavailable` past the first handshake retry.
- Production initramfs must be present (`scripts/build_claude_rootfs.sh`);
  if missing, mark this step as skipped with the reason.
- Secrets must come from the user's shell env (e.g. via `! export …` or
  a `~/.anthropic-key`-style file) — never paste them inline.

If `ANTHROPIC_API_KEY` / `TELEGRAM_*` are unset, report step 6 as
**skipped (no secrets)** rather than failing the gate.

---

Note: Integration and E2E tests (conformance, snapshot, e2e_*) require
`VOID_BOX_KERNEL` and `VOID_BOX_INITRAMFS` to be set and use
`--ignored --test-threads=1`. Run them only if the user requests
VM-level validation.

Report each step's result. If steps 1–5 pass (and step 6 passes or is
justified-skipped), confirm the gate is green.
