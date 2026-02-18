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
- Linux with KVM support (for testing KVM features)
- `musl-tools` for building guest-agent
- `cpio` and `gzip` for initramfs creation

### Building the Project

```bash
# Build the library
cargo build

# Build the CLI
cargo build --bin voidbox

# Build guest-agent
cargo build -p guest-agent --target x86_64-unknown-linux-musl

# Build initramfs (requires Linux)
./scripts/build_guest_image.sh
```

### Running Tests

```bash
# Run all tests
cargo test --workspace

# Run tests with output
cargo test --workspace -- --nocapture

# Run specific test
cargo test test_name

# Run tests for a specific package
cargo test -p void-box
```

### Code Quality

Before submitting a PR, ensure your code passes all checks:

```bash
# Format code
cargo fmt --all

# Check formatting
cargo fmt --all -- --check

# Run clippy
cargo clippy --workspace --all-targets --all-features -- -D warnings

# Build documentation
cargo doc --no-deps --all-features
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
   cargo test --workspace
   cargo fmt --all
   cargo clippy --workspace --all-targets --all-features
   ```
4. **Write a clear PR description** explaining:
   - What problem does this solve?
   - How does it work?
   - Are there any breaking changes?
5. **Link related issues** in the PR description
6. **Respond to review feedback** promptly

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
│   │   └── voidbox.rs      # CLI tool
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
- [ ] aarch64 support and testing
- [ ] Improved error messages and diagnostics
- [ ] Performance benchmarks and optimizations
- [ ] More comprehensive integration tests

### Medium Priority
- [ ] Windows and macOS support (mock sandbox)
- [ ] Additional workflow composition patterns
- [ ] REST API server
- [ ] Language bindings (Python, Node.js)

### Documentation
- [ ] More examples and tutorials
- [ ] Architecture deep-dive docs
- [ ] Video walkthroughs
- [ ] Blog posts and use cases

## Code of Conduct

### Our Standards

- Be respectful and inclusive
- Provide constructive feedback
- Focus on what's best for the community
- Show empathy towards others

### Unacceptable Behavior

- Harassment or discriminatory language
- Personal attacks or trolling
- Publishing private information
- Other conduct inappropriate in a professional setting

## License

By contributing to void-box, you agree that your contributions will be licensed under the Apache-2.0 license.

## Questions?

Feel free to open an issue or start a discussion if you have questions about contributing!
