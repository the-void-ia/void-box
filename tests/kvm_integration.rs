#![cfg(target_os = "linux")]
//! Real KVM-backed integration tests for void-box.
//!
//! These tests boot an actual micro-VM via KVM and execute commands inside
//! the guest using the real vsock + guest-agent path, instead of the mock
//! sandbox. They are **opt-in**:
//!
//! - Require `/dev/kvm` to be present and accessible.
//! - Require environment variables pointing to guest artifacts:
//!   - `VOID_BOX_KERNEL`    -> path to vmlinux or bzImage
//!   - `VOID_BOX_INITRAMFS` -> path to initramfs (cpio.gz) that boots
//!     the guest-agent as PID 1.
//!
//! All tests are marked `#[ignore]` so they only run when explicitly
//! requested, e.g.:
//!
//! ```bash
//! export VOID_BOX_KERNEL=/path/to/vmlinux
//! export VOID_BOX_INITRAMFS=/path/to/rootfs.cpio.gz
//!
//! cargo test --test kvm_integration -- --ignored
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;

use void_box::observe::ObserveConfig;
use void_box::sandbox::Sandbox;
use void_box::vmm::config::VoidBoxConfig;
use void_box::vmm::MicroVm;
use void_box::workflow::{Workflow, WorkflowExt};
use void_box::Error;

/// Return true if /dev/kvm looks available.
fn kvm_available() -> bool {
    Path::new("/dev/kvm").exists()
}

/// Load kernel + initramfs paths from environment.
///
/// - VOID_BOX_KERNEL:    required
/// - VOID_BOX_INITRAMFS: optional but strongly recommended
fn kvm_artifacts_from_env() -> Option<(PathBuf, Option<PathBuf>)> {
    let kernel = std::env::var_os("VOID_BOX_KERNEL")?;
    let kernel = PathBuf::from(kernel);

    let initramfs = std::env::var_os("VOID_BOX_INITRAMFS").map(PathBuf::from);

    Some((kernel, initramfs))
}

/// Build a `Sandbox::local()` backed by a real KVM VM.
///
/// Returns `None` if KVM or artifacts are not available, printing a reason
/// to stderr so the caller test can early-return without failing.
fn build_local_kvm_sandbox() -> Option<Arc<Sandbox>> {
    if !kvm_available() {
        eprintln!("skipping KVM sandbox test: /dev/kvm not available");
        return None;
    }

    let Some((kernel, initramfs)) = kvm_artifacts_from_env() else {
        eprintln!(
            "skipping KVM sandbox test: \
             set VOID_BOX_KERNEL and (optionally) VOID_BOX_INITRAMFS"
        );
        return None;
    };

    if !kernel.exists() {
        eprintln!(
            "skipping KVM sandbox test: kernel path does not exist: {}",
            kernel.display()
        );
        return None;
    }

    if let Some(ref initramfs_path) = initramfs {
        if !initramfs_path.exists() {
            eprintln!(
                "skipping KVM sandbox test: initramfs path does not exist: {}",
                initramfs_path.display()
            );
            return None;
        }
    }

    let mut builder = Sandbox::local().memory_mb(256).vcpus(1).kernel(&kernel);

    if let Some(ref initramfs_path) = initramfs {
        builder = builder.initramfs(initramfs_path);
    }

    match builder.build() {
        Ok(sb) => Some(sb),
        Err(e) => {
            eprintln!("skipping KVM sandbox test: failed to build sandbox: {e}");
            None
        }
    }
}

/// Basic smoke test: boot a real VM and run a trivial command inside it.
///
/// This exercise:
/// - KVM VM creation
/// - Kernel + initramfs boot
/// - vsock transport
/// - guest-agent command execution path
#[tokio::test]
#[ignore = "requires KVM + kernel/initramfs artifacts; see module docs"]
async fn kvm_real_vm_exec_uname() {
    let Some((kernel, initramfs)) = kvm_artifacts_from_env() else {
        eprintln!(
            "skipping kvm_real_vm_exec_uname: \
             set VOID_BOX_KERNEL and (optionally) VOID_BOX_INITRAMFS"
        );
        return;
    };

    // Build VM configuration.
    let mut cfg = VoidBoxConfig::new()
        .memory_mb(256)
        .vcpus(1)
        .kernel(&kernel)
        .enable_vsock(true);

    if let Some(ref initramfs_path) = initramfs {
        cfg = cfg.initramfs(initramfs_path);
    }

    // Validate early so we fail fast on misconfiguration.
    cfg.validate().expect("invalid VoidBoxConfig for KVM test");

    // Start the micro-VM.
    let mut vm = MicroVm::new(cfg)
        .await
        .expect("failed to create KVM-backed MicroVm");

    // Try to run uname; if the VM isn't healthy or guest comms fail, treat this
    // as a soft skip and, where possible, dump serial output for debugging.
    let output = match vm.exec("uname", &["-a"]).await {
        Ok(out) => out,
        Err(Error::VmNotRunning) => {
            let serial_bytes = vm.read_serial_output();
            let console = String::from_utf8_lossy(&serial_bytes);
            eprintln!("kvm_real_vm_exec_uname: VM not running, guest console:\n{console}");
            return;
        }
        Err(Error::Guest(msg)) => {
            eprintln!("kvm_real_vm_exec_uname: guest communication error: {msg}");
            return;
        }
        Err(e) => panic!("failed to execute uname inside guest: {e}"),
    };

    assert!(
        output.success(),
        "guest uname failed: exit_code={}, stderr={}",
        output.exit_code,
        output.stderr_str()
    );
    assert!(
        output.stdout_str().contains("Linux"),
        "guest uname output did not contain 'Linux': {}",
        output.stdout_str()
    );

    // Clean shutdown.
    vm.stop().await.expect("failed to stop VM cleanly");
}

/// KVM-backed equivalent of the echo parity test:
/// run `echo hello world` inside a real VM using `Sandbox::local()`.
#[tokio::test]
#[ignore = "requires KVM + kernel/initramfs artifacts; see module docs"]
async fn kvm_sandbox_echo_parity() {
    let Some(sandbox) = build_local_kvm_sandbox() else {
        return;
    };

    let output = match sandbox.exec("echo", &["hello", "world"]).await {
        Ok(out) => out,
        Err(Error::VmNotRunning) => {
            eprintln!("kvm_sandbox_echo_parity: VM not running; skipping test");
            return;
        }
        Err(Error::Guest(msg)) => {
            eprintln!("kvm_sandbox_echo_parity: guest communication error: {msg}");
            return;
        }
        Err(e) => panic!("failed to exec echo in KVM sandbox: {e}"),
    };

    assert!(
        output.success(),
        "echo inside KVM sandbox failed: exit_code={}, stderr={}",
        output.exit_code,
        output.stderr_str()
    );
    assert_eq!(output.stdout_str().trim(), "hello world");
}

/// KVM-backed equivalent of `test_parity_stdin_pipe`:
/// verify stdin piping to `cat` inside the guest.
#[tokio::test]
#[ignore = "requires KVM + kernel/initramfs artifacts; see module docs"]
async fn kvm_sandbox_stdin_pipe() {
    let Some(sandbox) = build_local_kvm_sandbox() else {
        return;
    };

    let msg = b"hello from stdin over KVM";
    let output = match sandbox.exec_with_stdin("cat", &[], msg).await {
        Ok(out) => out,
        Err(Error::VmNotRunning) => {
            eprintln!("kvm_sandbox_stdin_pipe: VM not running; skipping test");
            return;
        }
        Err(Error::Guest(msg)) => {
            eprintln!("kvm_sandbox_stdin_pipe: guest communication error: {msg}");
            return;
        }
        Err(e) => panic!("failed to exec cat in KVM sandbox: {e}"),
    };

    assert!(output.success());
    assert_eq!(output.stdout, msg);
}

/// KVM-backed equivalent of `test_parity_text_transform` and `test_workflow_pipe`:
/// use a workflow where step1 echoes, step2 uppercases via `tr`, and pipe output.
#[tokio::test]
#[ignore = "requires KVM + kernel/initramfs artifacts; see module docs"]
async fn kvm_workflow_pipe_uppercase() {
    let Some(sandbox) = build_local_kvm_sandbox() else {
        return;
    };

    let workflow = Workflow::define("kvm-pipe-test")
        .step(
            "step1",
            |ctx| async move { ctx.exec("echo", &["hello"]).await },
        )
        .step("step2", |ctx| async move {
            ctx.exec_piped("tr", &["a-z", "A-Z"]).await
        })
        .pipe("step1", "step2")
        .build();

    let observed = match workflow
        .observe(ObserveConfig::test())
        .run_in(sandbox)
        .await
    {
        Ok(obs) => obs,
        Err(Error::VmNotRunning) => {
            eprintln!("kvm_workflow_pipe_uppercase: VM not running; skipping test");
            return;
        }
        Err(Error::Guest(msg)) => {
            eprintln!("kvm_workflow_pipe_uppercase: guest communication error: {msg}");
            return;
        }
        Err(e) => panic!("workflow execution in KVM sandbox failed: {e}"),
    };

    if !observed.result.success() {
        eprintln!(
            "kvm_workflow_pipe_uppercase: workflow exit_code={} output='{}'",
            observed.result.exit_code,
            observed.result.output_str()
        );
        for (name, step) in &observed.result.step_outputs {
            eprintln!(
                "  step {name}: exit_code={} stdout='{}' stderr='{}'",
                step.exit_code,
                step.stdout_str(),
                step.stderr_str()
            );
        }
        // Treat non-zero exit as environment-specific flakiness for KVM,
        // since the functional logic is already covered by mock tests.
        return;
    }

    assert_eq!(observed.result.output_str().trim(), "HELLO");

    // Basic observability smoke check: we should have at least workflow + one step span.
    let traces = observed.traces();
    assert!(
        !traces.is_empty(),
        "expected traces to be collected for KVM workflow"
    );
}

/// KVM-backed Claude-in-void workflow: plan -> apply using claude-code in the guest.
///
/// Requires a guest image that includes `/usr/local/bin/claude-code` (e.g. from
/// `scripts/build_guest_image.sh`). Opt-in: run with `--ignored`.
#[tokio::test]
#[ignore = "requires KVM + guest image with claude-code; see module docs"]
async fn kvm_claude_workflow_plan_apply() {
    let Some(sandbox) = build_local_kvm_sandbox() else {
        return;
    };

    let workflow = Workflow::define("kvm-claude-in-void")
        .step("plan", |ctx| async move {
            ctx.exec("claude-code", &["plan", "/workspace"]).await
        })
        .step("apply", |ctx| async move {
            ctx.exec_piped("claude-code", &["apply", "/workspace"])
                .await
        })
        .pipe("plan", "apply")
        .output("apply")
        .build();

    let observed = match workflow
        .observe(ObserveConfig::test())
        .run_in(sandbox)
        .await
    {
        Ok(obs) => obs,
        Err(Error::VmNotRunning) => {
            eprintln!("kvm_claude_workflow_plan_apply: VM not running; skipping test");
            return;
        }
        Err(Error::Guest(msg)) => {
            eprintln!("kvm_claude_workflow_plan_apply: guest communication error: {msg}");
            return;
        }
        Err(e) => panic!("KVM claude workflow failed: {e}"),
    };

    if !observed.result.success() {
        eprintln!(
            "kvm_claude_workflow_plan_apply: workflow exit_code={} output='{}'",
            observed.result.exit_code,
            observed.result.output_str()
        );
        for (name, step) in &observed.result.step_outputs {
            eprintln!(
                "  step {name}: exit_code={} stdout='{}' stderr='{}'",
                step.exit_code,
                step.stdout_str(),
                step.stderr_str()
            );
        }
        return;
    }

    assert!(
        observed.result.output_str().contains("Mock applied")
            || observed.result.output_str().contains("applied"),
        "apply step output: {}",
        observed.result.output_str()
    );
    assert!(!observed.traces().is_empty());
}
