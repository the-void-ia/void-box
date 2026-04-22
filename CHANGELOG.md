# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **Slim microVM kernel build** (`scripts/build_slim_kernel.sh`) — upstream Linux v6.12.30 LTS + Firecracker `microvm-kernel-ci-{x86_64,aarch64}-6.1.config` base + void-box additions (9p, virtiofs, overlayfs, `VIRTIO_MMIO_CMDLINE_DEVICES`). Ships uncompressed `vmlinux` (~30 MB) unifying artifact shape with macOS/VZ. Disables `CONFIG_MODULE_SIG*` so builds work on OpenSSL 3 hosts (Fedora 40+). `KERNEL_VER` / `FC_COMMIT` / `FC_CONFIG_MAJMIN` env overrides for reproducible builds.
- **macOS slim-kernel cross-build** — `scripts/build_slim_kernel.sh` now dispatches into an `ubuntu:24.04` container on Darwin hosts (`--platform` pinned from host arch, host-side cache check skips Docker on re-runs). `UBUNTU_IMAGE` env var lets callers pin a digest for reproducibility. Enables `CONFIG_PCI{,_HOST_GENERIC,_HOST_COMMON}` + `CONFIG_VIRTIO_PCI{,_LEGACY}` so the slim kernel boots on Apple Virtualization.framework (which uses virtio-PCI on arm64, unlike Firecracker's virtio-MMIO).
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
- **Startup latency** — cold-boot p50 cut from ~4.9 s to **252 ms** (−95%) and warm-restore p50 from ~607 ms to **138 ms** (−77%) on KVM. Delivered in three steps: (1) remove three hardcoded blind waits (cold 4.9 s → 3.5 s, warm 607 ms → 433 ms); (2) add `initcall_blacklist=cmos_init,i8042_init` to the default kernel cmdline, skipping host-distro RTC/i8042 probe timeouts (cold 3.5 s → 1.7 s); (3) ship the slim kernel (cold 1.7 s → 252 ms). Backed by `voidbox-startup-bench --iters 20 --breakdown` on Fedora 43 host.
- `vmm::vsock_irq_thread` epoll timeout tightened 200 ms → 20 ms so `stop()` reclaims the thread within one poll window instead of up to a full interval — drops `stop()` phase from ~230 ms to ~50 ms on both cold and warm paths
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
- **Userspace vsock: host→guest writes larger than 4 KiB** (`VsockConnectionMap::queue_host_data`) — the function silently truncated any buffer longer than 4 KiB to a single 4 KiB packet and dropped the rest, so any Message whose wire size exceeded the per-packet cap hung the guest-agent's `read_exact(payload)` indefinitely and the host timed out after 30 s with `Protocol error: IO error: Resource temporarily unavailable`. Loop now produces one Rw packet per 4 KiB chunk, respects peer credit, and buffers the remainder into `conn.tx_buf` + flushes on `peer_fwd_cnt` advance. Small RPCs unchanged. Latent since the userspace backend landed (`f4f14ab`); first user-visible trigger was the 6 KiB `hackernews-api.md` skill file (~21 KiB on the wire after `WriteFile` JSON framing).
- **x86_64 boot loader: raw `vmlinux` ELF support** (`src/vmm/arch/x86_64/boot.rs`) — `load_elf_kernel` now reads `e_entry` from ELF header offset `0x18` instead of returning `kernel_end` (which was the end of loaded memory, not the entry point), and `read_bzimage_init_size` / `read_bzimage_initrd_addr_max` are gated on `is_bzimage()` so raw ELFs don't read garbage from offsets `0x260` / `0x22c` (which collapsed the initramfs placement window to `0x0` and surfaced as `initramfs too large for placement window end=0x0`)
- **OCI rootfs mount on macOS/VZ**: `setup_oci_rootfs()` on the non-block-device path now mounts the virtiofs share (parsed from `/proc/cmdline` `voidbox.mount*` entries) before the `is_dir` check, and the in-overlay shared-dir loop skips the same entry to avoid a double mount. Unblocks every `sandbox.image`-based spec on Apple Silicon; previously failed with `OCI_FAIL_ROOTFS_MISSING`. New `OCI_FAIL_LOWER_MOUNT` status code for diagnostic granularity.
- **Orphan-run reconciliation on daemon restart**: `reconcile_orphan_runs_on_startup()` flips persisted non-terminal runs (`Pending`/`Starting`/`Running`) to `Failed` with `terminal_reason = "daemon_restarted"` and emits a `run.failed` event. Previously these runs were returned from `GET /v1/runs` forever after a kill/restart. Reuses `RunStatus::Failed` to keep the serde wire format stable.
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
