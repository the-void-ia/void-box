# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- `voidbox --log-dir` and `VOIDBOX_LOG_DIR` overrides for file-based runtime logs
- **Codex CLI as first-class agent peer** — `llm.provider: codex` in YAML specs exec's the bundled OpenAI Codex CLI inside the guest VM with full structured observability
- `scripts/build_codex_rootfs.sh` — production Codex-capable initramfs with auto-download from GitHub releases (musl-static, no glibc shipping)
- `scripts/build_agents_rootfs.sh` — combined claude+codex flavor produced in a single build
- `scripts/lib/agent_rootfs_common.sh` — shared rootfs helpers (sandbox user, CA certs, finalize, claude/codex binary resolution + auto-download) extracted from the Claude flavor for reuse across agent flavors
- `src/observe/codex.rs` — structured stream parser for Codex's `exec --json` JSONL events (tool calls, token counts, error handling)
- `ObserverKind` enum on `LlmProvider` — typed dispatch replacing the binary-name string comparison for stream observer selection
- `LlmProvider::Codex` variant with `binary_name()`, `supports_claude_settings()`, `build_exec_args()`, `observer_kind()`, `image_flavor()` methods
- Auth via host `~/.codex/auth.json` mount (ChatGPT OAuth) alongside `OPENAI_API_KEY` env var fallback
- Codex MCP discovery — writes `~/.codex/config.toml` with `[mcp_servers]` streamable-HTTP entries pointing at the existing void-mcp server
- Per-agent docs at `docs/agents/claude.md` and `docs/agents/codex.md` with `@` discovery imports in AGENTS.md
- `examples/specs/codex_smoke.yaml` (`kind: agent`) and `examples/specs/codex_workflow_smoke.yaml` (`kind: workflow`)
- **VZ native snapshot/restore** using Apple's `saveMachineStateToURL:` / `restoreMachineStateFromURL:` APIs (macOS 14+) with a JSON sidecar (`VzSnapshotMeta`) carrying `session_secret`, `memory_mb`, `vcpus`, `network`, `boot_clock_secs`, `config_hash`, and `VZGenericMachineIdentifier.dataRepresentation`
- VZ restore **device-set drift reconciliation**: when caller-supplied `memory_mb` / `vcpus` / `network` drift from the sidecar's saved values, the saved values are used so Apple's strict configuration-match check does not fail the restore
- `SandboxBuilder::enable_snapshots(…)` / `SandboxConfig` / `BackendConfig` opt-in plumbing that gates VZ's `validateSaveRestoreSupportWithError` check (cold boots that do not opt in skip the check — some device sets make Apple reject snapshot-capability validation even when the VM itself is healthy)
- `snapshot_store::resolve_snapshot_argument` returning a `SnapshotResolution` enum (`Hash` / `Literal` / `NotFound`), unifying three duplicate hash-vs-literal resolution paths

### Changed
- Rust MSRV bumped to 1.88
- Interactive `voidbox shell` sessions now route runtime logs to the daily log file and route guest console output away from the active terminal to avoid TUI corruption
- `voidbox shell` now prefers Claude Personal when personal OAuth credentials are available via the host's cross-platform credential discovery path
- **Renamed** `ClaudeExecOpts` / `ClaudeExecResult` / `ClaudeStreamEvent` → `AgentExecOpts` / `AgentExecResult` / `AgentStreamEvent` (flat rename, no wrapper enum — both providers populate the same struct)
- **Renamed** `Sandbox::exec_claude()` / `exec_claude_streaming()` → `exec_agent()` / `exec_agent_streaming()` with `&LlmProvider` parameter threading
- **Renamed** `StageResult.claude_result` → `agent_result`
- **Renamed** `e2e_claude_mcp` test → `e2e_agent_mcp` (MCP infrastructure is agent-agnostic)
- `build_claude_rootfs.sh` refactored to source shared `agent_rootfs_common.sh` helpers
- `build_claude_rootfs.sh` and `build_codex_rootfs.sh` auto-detect or download the respective Linux binaries when invoked from an Apple Silicon host
- Claude-specific `--settings` and `--mcp-config` flags gated behind `provider.supports_claude_settings()`
- **Guest network deny list** is now applied once at guest init (right after `setup_network()`) instead of lazily on the first `exec` — closes the race window between network bring-up and first exec and makes the deny list visible on the serial console at boot
- **VZ auto-snapshot** uses `save_state_paused` followed by a direct `stop()` from the paused state, avoiding an unnecessary resume/pause round-trip
- `host_metrics.rs` on macOS now uses the `mach2` crate instead of hand-rolled Mach FFI (`IntegerT`, `TaskFlavorT`, `extern "C"` block)
- Service-agent and output-monitor progress messages in `agent_box.rs` routed through `tracing` (`info!` / `warn!` / `error!` / `debug!`) instead of `eprintln!`

### Fixed
- Interactive PTY shell handling on macOS/VZ: poll-based host relay, resize forwarding, and cleaner terminal lifecycle for Claude and other TUI-style programs
- Guest console routing semantics are now consistent across macOS/VZ and Linux/KVM
- Snapshot restore: capture/restore `IA32_XSS` MSR to prevent XRSTORS #GP on CET-enabled kernels (6.x+)
- Silent `chown` failure in `provision_claude_bootstrap` now emits a `warn!` with actionable remediation
- Codex downloader's `EXIT` trap is scoped to a subshell so it cannot clobber the caller's cleanup trap
- Deterministic MAC-address bit manipulation in `deterministic_mac_address` (`(x | 0x02) & 0xfe`) now carries a bit-level comment explaining the IEEE 802 "locally administered, unicast" transform

## [0.1.2] - 2026-03-16

### Added
- **Snapshot/Restore** for KVM: base and diff snapshots with multi-vCPU support, userspace virtio-vsock backend
- **Snapshot/Restore** for Apple Virtualization.framework (macOS VZ)
- **aarch64 architecture support** for snapshots via `Arch` trait refactor
- **Guest telemetry** buffering and host metrics collection
- **Daemon lifecycle events**: `StageQueued`, `StageStarted`, `StageSucceeded`, `StageFailed`, `StageSkipped`
- **Persist stage `file_output`** artifacts to disk after completion
- `GET /v1/runs/{run_id}/stages/{stage_name}/output-file` daemon endpoint for retrieving stage output files
- **Pipeline I/O wiring** with mount-based inputs/outputs
- **Host directory mounts** via 9p (Linux) and virtiofs (macOS) with RW/RO support
- Shell installer (`scripts/install.sh`)
- DEB and RPM packaging via nfpm
- Homebrew tap distribution (macOS)
- **Structured logging** via `tracing` with `StructuredLogger`
- Startup banner
- `snapshot_store` module centralizing snapshot utilities
- Snapshot CLI: `create`, `list`, `delete`, diff snapshots
- Virtio-net snapshot and restore
- OCI guest image distribution via GHCR (multi-arch: amd64 + arm64)
- macOS native support via Virtualization.framework
- LM Studio provider support and OpenClaw Telegram example

### Changed
- Unified pipeline execution loop (`run_pipeline_core`)
- Daemon `route_request` returns `(status, content_type, body)` for binary responses
- Rust MSRV bumped to 1.85
- Quinn-proto updated to v0.11.14
- Renamed `e2e_mount_9p` to `e2e_mount` with expanded virtiofs support
- Refactored module loading logic with optional 9p kernel modules
- Replaced `info!` logging with `debug!` for reduced noise

### Fixed
- Snapshot restore: XCR0 / LAPIC timer / CID mismatch issues
- EPERM-resilient OCI layer unpack
- macOS VZ examples and Apple Silicon support
- Duplicate directory creation in artifact management
- aarch64 musl cross-linker path in guest-image workflow
- Diamond dependency conflict with virtio and vm-memory crates
- BusyBox inclusion in CI guest image

## [0.1.0] - 2026-02-19

### Added
- Initial release of void-box
- KVM-based micro-VM sandbox implementation
- Mock sandbox for testing and development
- Workflow composition engine with DAG support
- Native observability layer (traces, metrics, logs)
- SLIRP user-mode networking (no root required)
- Guest-agent for VM communication
- CLI tool (`voidbox`) for command-line usage
- Pre-built artifact distribution via GitHub releases
- Streaming tool events — real-time `[vm:NAME] tool: Bash <cmd>` output during execution
- Descriptive tool logging with `tool_summary()` (shows command/file_path/pattern instead of tool ID)
- Incremental JSONL parser (`parse_jsonl_line`) for stream processing
- `exec_agent_streaming()` sandbox API (renamed from `exec_claude_streaming()` in the Codex flavor effort)
- HackerNews agent example (`examples/hackernews/`)
- Code review agent example with two-stage pipeline and remote skills
- Comprehensive CI/CD pipeline with GitHub Actions
- Comprehensive documentation:
  - README with quick start guide
  - Getting Started guide
  - Architecture documentation
  - API documentation
- Build scripts for release artifacts
- GitHub Actions workflow for automated releases

### Features
- **Sandbox Execution**:
  - Local KVM-based sandboxes
  - Mock sandboxes for testing
  - Command execution with stdin/stdout/stderr
  - File operations (read/write)
  - Environment variable support

- **Workflow Engine**:
  - Step definition and composition
  - Pipeline support (pipe steps together)
  - Parallel execution
  - Retry logic with configurable backoff
  - Context isolation between steps

- **Observability**:
  - OpenTelemetry-compatible tracing
  - Metrics collection (counters, gauges)
  - Structured logging
  - Span inspection and analysis

- **CLI Tool**:
  - `voidbox exec` - Execute commands
  - `voidbox workflow` - Run workflows
  - Auto-detection of KVM availability
  - Fallback to mock sandbox

- **Artifact Management**:
  - Download pre-built artifacts from releases
  - Environment variable configuration
  - Auto-detection of host kernel
  - Artifact caching

### Changed
- Switch from Node.js + npm claude-code to native claude-code binary (Bun SEA)
- SLIRP networking: DNS caching, host resolv.conf forwarding, reduced timeouts
- Guest clock sync via kernel cmdline (`voidbox.clock=<epoch_secs>`)
- Net-poll background thread for improved network throughput
- HLT sleep reduced from 10ms to 1ms for lower latency
- NPROC limit raised to 512 (from 256) for Bun worker threads
- Memory bump to 2048MB in HackerNews example (OOM fix)

### Fixed
- RLIMIT_AS re-enabled at 1GB — Bun/JSC needs only ~640MB virtual (vs V8's 10GB+)
- `file_output` fallback when claude-code output file is missing
- `skipWebFetchPreflight` added to agent config defaults

### Infrastructure
- Rust workspace with library and guest-agent
- Multi-architecture support (x86_64, aarch64 ready)
- Static linking for guest-agent (musl)
- Automated release builds
- Documentation generation

[Unreleased]: https://github.com/the-void-ia/void-box/compare/v0.1.2...HEAD
[0.1.2]: https://github.com/the-void-ia/void-box/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/the-void-ia/void-box/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/the-void-ia/void-box/releases/tag/v0.1.0
