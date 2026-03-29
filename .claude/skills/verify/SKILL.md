---
name: verify
description: Run the full quality gate for this repo — format check, clippy, tests, and security audit. Invoke before marking any implementation task done.
---

Run these checks in order. Stop and report at the first failure.

**1. Format check**
```
cargo fmt --all -- --check
```

**2. Clippy (platform-aware)**

On macOS (excludes guest-agent, which is Linux-only):
```
cargo clippy --workspace --exclude guest-agent --all-targets --all-features -- -D warnings
```

On Linux:
```
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

**3. Tests (platform-aware)**

On macOS (excludes guest-agent, which is Linux-only):
```
cargo test --workspace --exclude guest-agent --all-features --verbose
cargo test --doc --workspace --exclude guest-agent --all-features
```

On Linux:
```
cargo test --workspace --all-features --verbose
cargo test --doc --workspace --all-features
```

Note: Integration and E2E tests (conformance, snapshot, e2e_*) require `VOID_BOX_KERNEL` and `VOID_BOX_INITRAMFS` to be set and use `--ignored --test-threads=1`. Only run them if the user requests VM-level validation.

**4. Security audit**
```
cargo audit --deny warnings
```

Report each step's result. If everything passes, confirm the gate is green.
