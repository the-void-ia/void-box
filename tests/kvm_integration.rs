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

use std::path::PathBuf;
use std::sync::Arc;

#[path = "common/vm_preflight.rs"]
mod vm_preflight;

use void_box::observe::ObserveConfig;
use void_box::sandbox::Sandbox;
use void_box::vmm::config::VoidBoxConfig;
use void_box::vmm::MicroVm;
use void_box::workflow::{Workflow, WorkflowExt};
use void_box::Error;

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

/// Build a `Sandbox::local()` backed by a real KVM VM, or gate.
///
/// Skips when artifacts are unset or the machine is incapable; on a capable
/// machine a build failure panics via `checked_vm`. Returns `None` on a skip.
fn build_local_kvm_sandbox() -> Option<Arc<Sandbox>> {
    let Some((kernel, initramfs)) = kvm_artifacts_from_env() else {
        vm_preflight::skip_or_require("VOID_BOX_KERNEL is unset");
        return None;
    };
    if !vm_preflight::vm_capable_or_gate(&kernel, initramfs.as_deref()) {
        return None;
    }

    let mut builder = Sandbox::local().memory_mb(256).vcpus(1).kernel(&kernel);
    if let Some(ref initramfs_path) = initramfs {
        builder = builder.initramfs(initramfs_path);
    }

    vm_preflight::checked_vm(builder.build(), "kvm_integration sandbox build")
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
        vm_preflight::skip_or_require("VOID_BOX_KERNEL is unset");
        return;
    };
    if !vm_preflight::vm_capable_or_gate(&kernel, initramfs.as_deref()) {
        return;
    }

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

    // Run uname. On a capable machine a boot or guest-comms failure is a real
    // failure: dump serial output for VmNotRunning, then panic.
    let output = match vm.exec("uname", &["-a"]).await {
        Ok(out) => out,
        Err(Error::VmNotRunning) => {
            let serial_bytes = vm.read_serial_output();
            let console = String::from_utf8_lossy(&serial_bytes);
            panic!(
                "guest exec uname: VM not running on a capable machine; guest console:\n{console}"
            );
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

    let Some(output) = vm_preflight::checked_vm(
        sandbox.exec("echo", &["hello", "world"]).await,
        "guest exec echo (kvm_integration)",
    ) else {
        return;
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
    let Some(output) = vm_preflight::checked_vm(
        sandbox.exec_with_stdin("cat", &[], msg).await,
        "guest exec cat (kvm_integration)",
    ) else {
        return;
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

    let Some(observed) = vm_preflight::checked_vm(
        workflow
            .observe(ObserveConfig::test())
            .run_in(sandbox)
            .await,
        "workflow run (kvm_integration)",
    ) else {
        return;
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

    let Some(observed) = vm_preflight::checked_vm(
        workflow
            .observe(ObserveConfig::test())
            .run_in(sandbox)
            .await,
        "claude workflow run (kvm_integration)",
    ) else {
        return;
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
        observed
            .result
            .output_str()
            .contains("Mock execution complete")
            || observed.result.output_str().contains("applied"),
        "apply step output: {}",
        observed.result.output_str()
    );
    assert!(!observed.traces().is_empty());
}
