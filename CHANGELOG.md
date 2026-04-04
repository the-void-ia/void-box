# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed
- Rust MSRV bumped to 1.88

### Fixed
- Snapshot restore: capture/restore `IA32_XSS` MSR to prevent XRSTORS #GP on CET-enabled kernels (6.x+)

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
- `exec_claude_streaming()` sandbox API
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
