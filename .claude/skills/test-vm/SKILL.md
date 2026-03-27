---
name: test-vm
description: Run a minimal VoidBox VM smoke test locally. Verifies the VM boots and the LLM responds correctly.
disable-model-invocation: true
---

Runs `examples/smoke/smoke.yaml` — the agent replies with `"vm-ok 2+2=4"`. No tool use required; any model that generates text will pass. Success is `success: true` with output containing `vm-ok`.

**All commands run from the repository root.**

Usage: `/test-vm [ollama [model] | claude | claude-personal]`
- `ollama [model]` — local Ollama (default model: `phi4-mini`)
- `claude` — Anthropic API key (`ANTHROPIC_API_KEY` must be set)
- `claude-personal` — personal Claude account (macOS: extracted from Keychain; Linux: staged from `~/.claude/`)
- no argument — defaults to `ollama phi4-mini`

Recommended Ollama models (2–4 GB RAM, reliable with claude-code): `phi4-mini`, `gemma3:4b`, `llama3.2:3b`. Avoid `*-coder` variants — they don't follow the agentic protocol well.

---

## Step 1 — Prerequisites

### Kernel

Check for an existing kernel file. macOS needs an uncompressed kernel (`vmlinux-*`); Linux can use the host kernel (`vmlinuz-*`):

```bash
ls target/vmlinux-arm64 2>/dev/null \
  || ls target/vmlinux-amd64 2>/dev/null \
  || ls /boot/vmlinuz-$(uname -r) 2>/dev/null \
  || echo "MISSING"
```

If missing, download it (the script caches the result in `target/`):

```bash
scripts/download_kernel.sh
```

After downloading, run the check again to confirm and record the path — it is needed in Step 4.

### Rootfs

```bash
ls target/void-box-rootfs.cpio.gz 2>/dev/null || echo "MISSING"
```

If missing, build it (auto-detects the `claude` binary on PATH or `~/.local/bin/claude`):

```bash
scripts/build_claude_rootfs.sh
```

Run the check again to confirm. If it still fails, tell the user to set `CLAUDE_BIN=/path/to/claude` and retry.

---

## Step 2 — Build and sign

```bash
cargo build --release --bin voidbox
```

**macOS only** — Apple Virtualization.framework requires the entitlement:

```bash
codesign --force --sign - --entitlements voidbox.entitlements target/release/voidbox
```

---

## Step 3 — Provider setup

### Ollama

**Check Ollama is installed:**

```bash
which ollama || echo "NOT INSTALLED"
```

If not installed: macOS → `brew install ollama`; Linux → `curl -fsSL https://ollama.com/install.sh | sh`. Stop until installed.

**Check Ollama is running and bound to `0.0.0.0`** (required so the guest VM can reach it):

```bash
curl -sf http://localhost:11434/api/tags > /dev/null && echo "running" || echo "not running"
```

If not running, start it in the background:

```bash
OLLAMA_HOST=0.0.0.0:11434 ollama serve > /tmp/ollama-serve.log 2>&1 &
for i in $(seq 1 10); do curl -sf http://localhost:11434/api/tags > /dev/null && break || sleep 1; done
curl -sf http://localhost:11434/api/tags > /dev/null || { echo "Ollama failed to start:"; cat /tmp/ollama-serve.log; }
```

If it was already running: warn the user that Ollama must be listening on `0.0.0.0:11434` (not `127.0.0.1`) so the guest VM can reach it through the NAT gateway. If unsure, stop Ollama and restart it with `OLLAMA_HOST=0.0.0.0:11434 ollama serve`.

**Check the model is pulled; pull it if not:**

```bash
ollama list | grep -F "MODEL_NAME" || ollama pull MODEL_NAME
```

(substitute `MODEL_NAME` with the actual model, e.g. `phi4-mini`)

Set the Ollama base URL for use in Step 4:
- macOS: `http://192.168.64.1:11434` (VZ NAT gateway)
- Linux: `http://10.0.2.2:11434` (SLIRP gateway)

### Claude (API key)

```bash
[ -n "$ANTHROPIC_API_KEY" ] && echo "key set" || echo "MISSING — export ANTHROPIC_API_KEY=sk-ant-..."
```

If not set, stop and ask the user to export it.

### Claude (personal account)

Check authentication status using the CLI (exits 0 if logged in, 1 if not):

```bash
claude auth status
```

If not authenticated, stop and tell the user to run `claude auth login` first, then retry.

Stage credentials so the guest VM can find the OAuth token. The method differs by platform:

**macOS** — credentials live in Keychain; extract and write them as a file:

```bash
mkdir -p target/claude-home
CREDS_JSON=$(security find-generic-password -s "Claude Code-credentials" -a "$USER" -w 2>/dev/null) \
  || { echo "ERROR: credentials not found in Keychain — run 'claude auth login' first"; exit 1; }
echo "$CREDS_JSON" > target/claude-home/.credentials.json
echo "Credentials staged from Keychain"
```

**Linux** — credentials live in `~/.claude/`; rsync them:

```bash
mkdir -p target/claude-home
rsync -a --exclude 'settings.json' ~/.claude/ target/claude-home/
```

Generate a temporary spec by injecting the credentials mount into `smoke.yaml`:

```bash
python3 << 'EOF'
mount = '  mounts:\n    - host: "target/claude-home"\n      guest: "/home/sandbox/.claude"\n      mode: "rw"\n'
content = open('examples/smoke/smoke.yaml').read()
open('/tmp/voidbox-smoke-personal.yaml', 'w').write(
    content.replace('  network: true\n', '  network: true\n' + mount)
)
print("Generated /tmp/voidbox-smoke-personal.yaml")
EOF
```

---

## Step 4 — Run

Use the kernel path found in Step 1 and the URL from Step 3 in the commands below.

**Ollama:**
```bash
VOIDBOX_LLM_PROVIDER=ollama \
VOIDBOX_LLM_MODEL=<model> \
VOIDBOX_LLM_BASE_URL=<ollama-url> \
VOID_BOX_KERNEL=<kernel-path> \
VOID_BOX_INITRAMFS=target/void-box-rootfs.cpio.gz \
./target/release/voidbox run --file examples/smoke/smoke.yaml
```

**Claude (API key):**
```bash
VOID_BOX_KERNEL=<kernel-path> \
VOID_BOX_INITRAMFS=target/void-box-rootfs.cpio.gz \
./target/release/voidbox run --file examples/smoke/smoke.yaml
```

**Claude (personal account):**
```bash
VOID_BOX_KERNEL=<kernel-path> \
VOID_BOX_INITRAMFS=target/void-box-rootfs.cpio.gz \
./target/release/voidbox run --file /tmp/voidbox-smoke-personal.yaml
```

---

## Step 5 — Verify

The run output includes a `success:` field and an `output:` section. Check:
- `success: true` — the VM ran the agent without error
- output contains `vm-ok` — the LLM responded correctly

If `success: false` or output is missing `vm-ok`, report the full output for diagnosis.
