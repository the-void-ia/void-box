# Examples

## boot_diag

Boot diagnostics for VM bring-up.

```bash
cargo run --example boot_diag
```

## quick_demo

Two-stage pipeline (`analyst -> strategist`) with mock or KVM.

```bash
cargo run --example quick_demo
```

## trading_pipeline

Four-stage trading pipeline using local skills under `examples/trading_pipeline/skills/`.

```bash
cargo run --example trading_pipeline
```

With Ollama:

```bash
OLLAMA_MODEL=phi4-mini \
VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
VOID_BOX_INITRAMFS=/tmp/void-box-rootfs.cpio.gz \
cargo run --example trading_pipeline
```

## ollama_local

Single AgentBox configured to use Ollama.

```bash
cargo run --example ollama_local
```

## remote_skills

Fetch and preview remote skills from public repositories.

```bash
cargo run --example remote_skills
```

## claude_workflow

Plan/apply workflow-style example in sandbox.

```bash
cargo run --example claude_workflow
```

## claude_in_voidbox_example

Interactive/demo style Claude-compatible session.

```bash
cargo run --example claude_in_voidbox_example
```

## hackernews

HackerNews research agent with a real procedural-knowledge skill (`hackernews-api.md`)
that teaches the agent HOW to use the HN API via curl + jq.

Build a production initramfs (real `claude-code`):

```bash
CLAUDE_CODE_BIN=/path/to/claude scripts/build_guest_image.sh
```

Then set runtime image env:

```bash
export VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r)
export VOID_BOX_INITRAMFS=target/void-box-rootfs.cpio.gz
```

Run the HackerNews agent spec (Claude default):

```bash
cargo run --bin voidbox -- run --file examples/hackernews/hackernews_agent.yaml
```

Run with Ollama (no spec edits needed):

```bash
VOIDBOX_LLM_PROVIDER=ollama \
VOIDBOX_LLM_MODEL=qwen2.5-coder:7b \
cargo run --bin voidbox -- run --file examples/hackernews/hackernews_agent.yaml
```

## parallel_pipeline

Fan-out / fan-in diamond topology: multiple Boxes run in parallel on the same
input and their outputs are merged as a JSON array for the next stage.

```bash
cargo run --example parallel_pipeline
```

## code_review

Two-stage YAML pipeline: `analyzer` clones a repo and identifies improvements,
then `proposer` applies the fixes and opens a GitHub PR.

```bash
cargo run --bin voidbox -- run --file examples/code_review/code_review_agent.yaml
```

## openclaw

OpenClaw workflow specs and runtime notes live in:

- `examples/openclaw/README.md`

Quick commands:

```bash
cargo run --bin voidbox -- run --file examples/openclaw/node_version.yaml
cargo run --bin voidbox -- run --file examples/openclaw/openclaw_telegram.yaml
```

## specs/

Reference spec files for the `voidbox run --file` CLI:

| File | Kind | Description |
|------|------|-------------|
| `agent.json` | agent | Minimal single-agent spec (JSON) |
| `pipeline.yaml` | pipeline | Two-stage analyst → writer pipeline |
| `workflow.yaml` | workflow | Shell-step workflow with `depends_on` |
| `smoke_test.yaml` | agent | Quick "Hello from void-box!" smoke test |

```bash
cargo run --bin voidbox -- run --file examples/specs/smoke_test.yaml
```
