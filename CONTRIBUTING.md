# Contributing to void-box

Thank you for your interest in contributing to void-box! This document provides guidelines for contributing to the project.

## Getting Started

1. **Fork the repository** on GitHub
2. **Clone your fork** locally:
   ```bash
   git clone git@github.com:YOUR_USERNAME/void-box.git
   cd void-box
   ```
3. **Create a branch** for your changes:
   ```bash
   git checkout -b feature/my-new-feature
   ```

## Development Setup

### Prerequisites

- Rust 1.83 or later
- `cpio` and `gzip` for initramfs creation

Linux (KVM/E2E):
- `/dev/kvm` available for VM-backed tests
- `musl-tools` for building guest binaries
- Optional but recommended for full E2E parity: `/dev/vhost-vsock`

macOS (Apple Silicon / VZ):
- `filosottile/musl-cross/musl-cross` toolchain
- Rust target: `aarch64-unknown-linux-musl`
- Codesign support for binaries that use Virtualization.framework

### Git Hooks

Enable the project's pre-commit hook to catch formatting issues before they reach CI:

```bash
git config core.hooksPath .githooks
```

This runs `cargo fmt --all -- --check` on every commit.

### Building the Project

```bash
# Build the library
cargo build

# Build the CLI
cargo build --bin voidbox

# Build guest-agent
cargo build -p guest-agent --target x86_64-unknown-linux-musl

# Build a general guest initramfs
./scripts/build_guest_image.sh

# Build a deterministic test initramfs (guest-agent + claudio mock)
./scripts/build_test_image.sh

# Build a claude-capable guest rootfs/initramfs
./scripts/build_claude_rootfs.sh
```

Use these scripts based on purpose:
- `build_guest_image.sh`: general runtime guest image.
- `build_test_image.sh`: test/E2E image with deterministic `claudio`.
- `build_claude_rootfs.sh`: includes native `claude-code`, CA certs, and sandbox user.
- `build_codex_rootfs.sh`: includes the OpenAI `codex` CLI (musl-static), CA certs, and sandbox user.
- `build_agents_rootfs.sh`: combined claude + codex flavor produced in one build.

OpenClaw note:
- `examples/openclaw/openclaw_telegram.yaml` should run with `build_claude_rootfs.sh` output (`target/void-box-rootfs.cpio.gz`).
- Using the test initramfs for OpenClaw can lead to boot/handshake failures in VM workflows.

### Guest image script differences

| Script | Primary use | Includes agent runtime | Kernel module policy |
| --- | --- | --- | --- |
| `scripts/build_guest_image.sh` | Base/initramfs for normal VM runs and OCI-rootfs workflows | Optional (`CLAUDE_CODE_BIN` if provided), otherwise no agent binary requirement | Host modules by default; downloads modules only when `VOID_BOX_KMOD_VERSION` is set |
| `scripts/build_claude_rootfs.sh` | Production Claude-capable image | Yes (native `claude-code` + `/usr/local/bin/claude` symlink + CA certs + sandbox user) | Local default uses host modules; pinned/downloaded modules only when `VOID_BOX_PINNED_KMODS=1` (or CI) |
| `scripts/build_codex_rootfs.sh` | Production Codex-capable image | Yes (OpenAI `codex` CLI, musl-static, auto-download by `CODEX_VERSION` + CA certs + sandbox user) | Local default uses host modules; pinned/downloaded modules only when `VOID_BOX_PINNED_KMODS=1` (or CI) |
| `scripts/build_agents_rootfs.sh` | Combined claude + codex production image | Yes (both `claude-code` and `codex` CLI in one initramfs) | Same as the individual agent flavors |
| `scripts/build_test_image.sh` | Deterministic tests | No real agent; bundles `claudio` mock | Host modules on Linux (test path) |

All agent-flavor scripts (`build_claude_rootfs.sh`, `build_codex_rootfs.sh`, `build_agents_rootfs.sh`) delegate to `scripts/lib/agent_rootfs_common.sh`, which centralizes rootfs staging, sandbox-user/CA-cert provisioning, cpio packaging, and Linux-binary resolution (with auto-download when invoked from an Apple Silicon host).

### Running Tests

```bash
# Keep rustc/link temp artifacts in repo-local tmp if /tmp is constrained
export TMPDIR=$PWD/target/tmp
mkdir -p "$TMPDIR"

# Fast local checks
cargo test --workspace --all-features
cargo test --doc --workspace --all-features

# macOS parity with CI (guest-agent excluded in some jobs)
cargo test --workspace --exclude guest-agent --all-features
cargo test --doc --workspace --exclude guest-agent --all-features

# Include ignored/VM tests (Linux or macOS with artifacts available)
export VOID_BOX_KERNEL=/path/to/vmlinuz
export VOID_BOX_INITRAMFS=/path/to/rootfs.cpio.gz
cargo test --workspace --all-targets -- --include-ignored

# Targeted VM suites (generic guest image)
cargo test --test conformance -- --ignored --test-threads=1
cargo test --test oci_integration -- --ignored --test-threads=1

# Claudio-based deterministic E2E suites (requires test initramfs)
./scripts/build_test_image.sh
export VOID_BOX_INITRAMFS=/tmp/void-box-test-rootfs.cpio.gz
cargo test --test e2e_telemetry -- --ignored --test-threads=1
cargo test --test e2e_skill_pipeline -- --ignored --test-threads=1
```

Note: `e2e_telemetry` and `e2e_skill_pipeline` are Linux-only (`cfg(target_os = "linux")`).

If an ignored VM suite reports `Kvm(Error(13))` (or `Permission denied`) it usually means KVM ioctls are blocked in the current execution context; run the same command in a host shell/session with usable `/dev/kvm` and `/dev/vhost-vsock`.

For runtime setup examples and platform-specific details, see:
- `README.md` (KVM zero-setup, macOS/VZ, OCI examples)
- `docs/architecture.md` (backend, OCI, and security model)
- `AGENTS.md` (operations checklist, conformance suites, and guest image script differences)
- `AGENTS.md#validation-contract` (required command order and pass/skip exit gates)

### Code Quality

Before submitting a PR, ensure your code passes all checks:

```bash
# Check formatting
cargo fmt --all -- --check

# Run clippy (Linux / CI parity)
cargo clippy --workspace --all-targets --all-features -- -D warnings

# Run clippy (macOS / CI parity)
cargo clippy --workspace --exclude guest-agent --all-targets --all-features -- -D warnings

# Build documentation
cargo doc --no-deps --all-features

# Build docs on macOS with CI parity
cargo doc --workspace --no-deps --all-features --exclude guest-agent
```

## Coding Standards

### Rust Style Guide

- Follow the [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/)
- Use `cargo fmt` for code formatting
- Address all `cargo clippy` warnings
- Write documentation for public APIs
- Add tests for new functionality

### Documentation

- All public items should have documentation comments (`///`)
- Include examples in documentation where appropriate
- Update README.md if adding user-facing features
- Add entries to CHANGELOG.md for notable changes

### Testing

- Write unit tests for new functionality
- Add integration tests for workflows and complex features
- Ensure tests are deterministic and don't require external resources
- Use mock sandboxes for tests that don't require KVM

## Pull Request Process

1. **Update documentation** if you're changing user-facing features
2. **Add tests** for new functionality
3. **Run all checks** locally before pushing:
   ```bash
   cargo fmt --all -- --check
   cargo clippy --workspace --all-targets --all-features -- -D warnings
   cargo test --workspace --all-features
   ```
4. **Write a clear PR description** explaining:
   - What problem does this solve?
   - How does it work?
   - Are there any breaking changes?
5. **Link related issues** in the PR description
6. **Run relevant ignored suites** when touching VM/OCI/runtime behavior, and mention results in the PR
7. **Respond to review feedback** promptly

## AI Agent Skills

The repo ships agent skills for common development tasks in `.claude/skills/`. Each skill is a plain markdown file (`SKILL.md`) with self-contained instructions — readable and executable by any AI coding agent.

**Claude Code** loads them automatically; invoke with `/skill-name`. **Other agents** (Codex, Gemini CLI, etc.) can read the `SKILL.md` file directly and follow its instructions.

| Skill | File | Purpose |
|---|---|---|
| `/verify` | [`.claude/skills/verify/SKILL.md`](.claude/skills/verify/SKILL.md) | Full quality gate: format check → clippy → tests → audit. Run before marking any task done. |
| `/test-vm` | [`.claude/skills/test-vm/SKILL.md`](.claude/skills/test-vm/SKILL.md) | Boot a real micro-VM and verify the LLM responds. Supports Ollama, Claude API key, and personal account. |

---

## Commit Messages

Follow conventional commit format:

- `feat:` - New feature
- `fix:` - Bug fix
- `docs:` - Documentation changes
- `test:` - Test additions or changes
- `refactor:` - Code refactoring
- `perf:` - Performance improvements
- `chore:` - Maintenance tasks

Example:
```
feat: add support for custom kernel parameters

- Allow users to pass custom kernel command line args
- Add KernelConfig builder for configuration
- Update documentation with examples
```

## Release Process

Releases are automated via GitHub Actions:

1. Update version in `Cargo.toml`
2. Update `CHANGELOG.md`
3. Create and push a version tag:
   ```bash
   git tag v0.2.0
   git push origin v0.2.0
   ```
4. GitHub Actions will automatically:
   - Build release artifacts
   - Create GitHub release
   - Upload pre-built binaries

## Project Structure

```
void-box/
├── src/
│   ├── lib.rs              # Main library entry point
│   ├── artifacts.rs        # Artifact management
│   ├── bin/
│   │   └── voidbox/        # CLI tool
│   ├── sandbox/            # Sandbox abstraction
│   ├── workflow/           # Workflow engine
│   ├── observe/            # Observability layer
│   ├── vmm/                # VMM implementation
│   ├── devices/            # Virtual devices
│   ├── network/            # Networking (SLIRP)
│   └── guest/              # Guest communication
├── guest-agent/            # Guest agent binary
├── examples/               # Usage examples
├── scripts/                # Build and utility scripts
├── docs/                   # Documentation
└── tests/                  # Integration tests
```

## Getting Help

- **GitHub Discussions**: Ask questions and discuss ideas
- **GitHub Issues**: Report bugs and request features
- **Code of Conduct**: Be respectful and constructive

## Areas for Contribution

Looking for ideas? Here are some areas that need work:

### High Priority
- [ ] Improved error messages and diagnostics
- [ ] Performance benchmarks and optimizations
- [ ] More comprehensive integration tests

### Medium Priority
- [ ] Additional workflow composition patterns
- [ ] REST API server
- [ ] Language bindings (Python, Node.js)

### Documentation
- [ ] More examples and tutorials
- [ ] Architecture deep-dive docs
- [ ] Video walkthroughs
- [ ] Blog posts and use cases

## Code of Conduct

This project follows the [Contributor Covenant v2.1](CODE_OF_CONDUCT.md). Report concerns privately to **contact@voidplatform.ai**.

## License

By contributing to void-box, you agree that your contributions will be licensed under the Apache-2.0 license.

## Questions?

Feel free to open an issue or start a discussion if you have questions about contributing!
