# OpenClaw Examples

This folder contains the OpenClaw-focused workflow specs.

## Files

- `openclaw_telegram.yaml`: Runs OpenClaw as a Telegram gateway using `sandbox.image: alpine/openclaw`.
- `node_version.yaml`: Minimal OCI rootfs sanity check using `sandbox.image: node:22` and `node --version`.

## Prerequisites

Set guest artifacts:

```bash
export VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r)
export VOID_BOX_INITRAMFS=/home/diego/github/agent-infra/void-box/target/void-box-rootfs.cpio.gz
```

Build the correct image first:

```bash
TMPDIR=$PWD/target/tmp scripts/build_claude_rootfs.sh
```

Important:
- `openclaw_telegram.yaml` is a production gateway flow and must use the production image from `build_claude_rootfs.sh`.
- Do **not** use `/tmp/void-box-test-rootfs.cpio.gz` (`build_test_image.sh`) for this workflow.
- The test image is for deterministic `claudio` test suites only (`e2e_telemetry`, `e2e_skill_pipeline`).

Telegram gateway env vars:

```bash
export TELEGRAM_BOT_TOKEN=...
export TELEGRAM_CHAT_ID=...
export ANTHROPIC_API_KEY=...
```

Notes:

- You must start the bot in Telegram (`/start`) before expecting replies.
- Get your `chat_id` from Telegram Bot API `getUpdates`.

## Run

Node OCI sanity check:

```bash
cargo run --bin voidbox -- run --file examples/openclaw/node_version.yaml
```

Telegram gateway:

```bash
cargo run --bin voidbox -- run --file examples/openclaw/openclaw_telegram.yaml
```

Expected startup signal in Telegram:
- `OpenClaw prebuilt gateway started (...)`
- Then OpenClaw runtime/status messages from the bot.

## Long-running gateway operation

Run detached and capture logs:

```bash
nohup env \
  VOID_BOX_KERNEL="$VOID_BOX_KERNEL" \
  VOID_BOX_INITRAMFS="$VOID_BOX_INITRAMFS" \
  TELEGRAM_BOT_TOKEN="$TELEGRAM_BOT_TOKEN" \
  TELEGRAM_CHAT_ID="$TELEGRAM_CHAT_ID" \
  ANTHROPIC_API_KEY="$ANTHROPIC_API_KEY" \
  cargo run --bin voidbox -- run --file examples/openclaw/openclaw_telegram.yaml \
  > target/tmp/openclaw_telegram.log 2>&1 &
echo $! > target/tmp/openclaw_telegram.pid
```

Monitor:

```bash
tail -f target/tmp/openclaw_telegram.log
ps -fp "$(cat target/tmp/openclaw_telegram.pid)"
curl -sS "https://api.telegram.org/bot${TELEGRAM_BOT_TOKEN}/getUpdates" | head -c 400
```

Stop:

```bash
kill "$(cat target/tmp/openclaw_telegram.pid)"
```

## Troubleshooting

- `Kvm(Error(13))`: current user/process lacks `/dev/kvm` access.
- Boot log contains `Initramfs unpacking failed: read error` or `/dev/root: Can't open blockdev`:
  wrong/truncated initramfs was used. Rebuild with `scripts/build_claude_rootfs.sh` and point `VOID_BOX_INITRAMFS` to `target/void-box-rootfs.cpio.gz`.
- OCI unpack/cache issues: inspect `~/.voidbox/oci/{rootfs,disks}` and clear only the failing image key.
- Telegram startup message appears but no replies: verify `TELEGRAM_CHAT_ID`, bot `/start`, and `ANTHROPIC_API_KEY`.
- Network-bound behavior may fail in restricted environments (for example CI sandboxes).
