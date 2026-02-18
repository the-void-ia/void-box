# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2025-02-18

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

[Unreleased]: https://github.com/the-void-ia/void-box/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/the-void-ia/void-box/releases/tag/v0.1.0
