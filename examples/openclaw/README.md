# OpenClaw Examples

This folder contains the OpenClaw-focused workflow specs.

## Files

- `openclaw_telegram.yaml`: Runs OpenClaw as a Telegram gateway using `sandbox.image: alpine/openclaw`.
- `openclaw_telegram_ollama.yaml`: Runs OpenClaw as a Telegram gateway using host Ollama (`OLLAMA_BASE_URL`) as model backend.
- `node_version.yaml`: Minimal OCI rootfs sanity check using `sandbox.image: node:22` and `node --version`.

## Prerequisites

Build the correct image first:

```bash
TMPDIR=$PWD/target/tmp scripts/build_claude_rootfs.sh
```

Set guest artifacts:

**Linux (KVM):**

```bash
export VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r)
export VOID_BOX_INITRAMFS=$PWD/target/void-box-rootfs.cpio.gz
```

**macOS (VZ):**

```bash
scripts/download_kernel.sh
export VOID_BOX_KERNEL=$PWD/target/vmlinux-arm64
export VOID_BOX_INITRAMFS=$PWD/target/void-box-rootfs.cpio.gz
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

Telegram gateway (Ollama backend) env vars:

```bash
export TELEGRAM_BOT_TOKEN=...
export TELEGRAM_CHAT_ID=...
export OLLAMA_BASE_URL=http://10.0.2.2:11434   # Linux (KVM/SLIRP); use http://192.168.64.1:11434 on macOS (VZ)
export OLLAMA_API_KEY=ollama-local
export OLLAMA_MODEL=qwen2.5-coder:7b
```

Host prerequisites for Ollama workflow:

```bash
ollama serve
ollama pull qwen2.5-coder:7b
```

Notes:

- You must start the bot in Telegram (`/start`) before expecting replies.
- Get your `chat_id` from Telegram Bot API `getUpdates`.
- The Ollama workflow fails fast if Ollama is unreachable or model is missing.

## Run

Node OCI sanity check:

```bash
cargo run --bin voidbox -- run --file examples/openclaw/node_version.yaml
```

Telegram gateway:

```bash
cargo run --bin voidbox -- run --file examples/openclaw/openclaw_telegram.yaml
```

Telegram gateway (Ollama backend):

```bash
cargo run --bin voidbox -- run --file examples/openclaw/openclaw_telegram_ollama.yaml
```

Expected startup signal in Telegram:
- `OpenClaw prebuilt gateway started (...)`
- `OpenClaw Ollama gateway started (...)`
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

Run detached for the Ollama-backed gateway:

```bash
nohup env \
  VOID_BOX_KERNEL="$VOID_BOX_KERNEL" \
  VOID_BOX_INITRAMFS="$VOID_BOX_INITRAMFS" \
  TELEGRAM_BOT_TOKEN="$TELEGRAM_BOT_TOKEN" \
  TELEGRAM_CHAT_ID="$TELEGRAM_CHAT_ID" \
  OLLAMA_BASE_URL="$OLLAMA_BASE_URL" \
  OLLAMA_API_KEY="$OLLAMA_API_KEY" \
  OLLAMA_MODEL="$OLLAMA_MODEL" \
  cargo run --bin voidbox -- run --file examples/openclaw/openclaw_telegram_ollama.yaml \
  > target/tmp/openclaw_telegram_ollama.log 2>&1 &
echo $! > target/tmp/openclaw_telegram_ollama.pid
```

Monitor:

```bash
tail -f target/tmp/openclaw_telegram.log
ps -fp "$(cat target/tmp/openclaw_telegram.pid)"
curl -sS "https://api.telegram.org/bot${TELEGRAM_BOT_TOKEN}/getUpdates" | head -c 400
```

Monitor Ollama-backed gateway:

```bash
tail -f target/tmp/openclaw_telegram_ollama.log
ps -fp "$(cat target/tmp/openclaw_telegram_ollama.pid)"
curl -sS "https://api.telegram.org/bot${TELEGRAM_BOT_TOKEN}/getUpdates" | head -c 400
```

Stop:

```bash
kill "$(cat target/tmp/openclaw_telegram.pid)"
```

Stop Ollama-backed gateway:

```bash
kill "$(cat target/tmp/openclaw_telegram_ollama.pid)"
```

## Troubleshooting

- `Kvm(Error(13))`: current user/process lacks `/dev/kvm` access (Linux only).
- Boot log contains `Initramfs unpacking failed: read error` or `/dev/root: Can't open blockdev`:
  wrong/truncated initramfs was used. Rebuild with `scripts/build_claude_rootfs.sh` and point `VOID_BOX_INITRAMFS` to `target/void-box-rootfs.cpio.gz`.
- OCI unpack/cache issues: inspect `~/.voidbox/oci/{rootfs,disks}` and clear only the failing image key.
- Telegram startup message appears but no replies: verify `TELEGRAM_CHAT_ID`, bot `/start`, and `ANTHROPIC_API_KEY`.
- Telegram shows `fetch failed` on Ollama flow:
  - On host, verify Ollama API is live: `curl -sS http://127.0.0.1:11434/api/tags | head`
  - Check model presence: `ollama ps` and `ollama list` should include `qwen2.5-coder:7b`
  - Probe generation on host:
    `curl -sS http://127.0.0.1:11434/api/generate -H 'Content-Type: application/json' -d '{"model":"qwen2.5-coder:7b","prompt":"say hi","stream":false}' | head`
  - Use `OLLAMA_BASE_URL=http://10.0.2.2:11434` on Linux; `http://192.168.64.1:11434` on macOS (not `127.0.0.1`)
  - If still unstable, lower model/context or switch to a smaller model.
- Network-bound behavior may fail in restricted environments (for example CI sandboxes).
