#![cfg(target_os = "linux")]
//! Real KVM-backed integration tests for void-box.
//!
//! These tests boot an actual micro-VM via KVM and execute commands inside
//! the guest using the real vsock + guest-agent path, instead of the mock
//! sandbox.
//!
//! Kernel and initramfs are auto-provisioned by [`test_artifacts`] under
//! `--ignored`; `VOID_BOX_KERNEL` / `VOID_BOX_INITRAMFS` are optional overrides
//! that skip the build. All tests are `#[ignore]`, so a plain `cargo test`
//! never provisions or boots a VM.
//!
//! ```bash
//! cargo test --test kvm_integration -- --ignored
//! ```

use std::sync::Arc;

#[path = "common/test_artifacts.rs"]
mod test_artifacts;

use void_box::observe::ObserveConfig;
use void_box::sandbox::Sandbox;
use void_box::vmm::config::VoidBoxConfig;
use void_box::vmm::MicroVm;
use void_box::workflow::{Workflow, WorkflowExt};
use void_box::Error;

/// Build a `Sandbox::local()` backed by a real KVM VM, auto-provisioning the
/// kernel and test initramfs. A build failure panics — artifacts are present,
/// so it is a real failure rather than a skip.
fn build_local_kvm_sandbox() -> Arc<Sandbox> {
    let (kernel, initramfs) = test_artifacts::artifacts();

    let builder = Sandbox::local()
        // The test image is a few MB compressed and uncompressed, so 256 MB
        // clears the AGENTS.md "VM memory sizing" minimum (compressed +
        // uncompressed + 208 MB overhead) with room to spare. Keep it small:
        // this suite boots its VMs in parallel, so every MB is multiplied.
        .memory_mb(256)
        .vcpus(1)
        .kernel(&kernel)
        .initramfs(&initramfs);

    test_artifacts::expect_vm(builder.build(), "kvm_integration sandbox build")
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
    let (kernel, initramfs) = test_artifacts::artifacts();

    // Build VM configuration. Memory follows the AGENTS.md sizing formula for
    // the test image (see `build_local_kvm_sandbox`).
    let cfg = VoidBoxConfig::new()
        .memory_mb(256)
        .vcpus(1)
        .kernel(&kernel)
        .initramfs(&initramfs)
        .enable_vsock(true);

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
    let sandbox = build_local_kvm_sandbox();

    let output = test_artifacts::expect_vm(
        sandbox.exec("echo", &["hello", "world"]).await,
        "guest exec echo (kvm_integration)",
    );

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
    let sandbox = build_local_kvm_sandbox();

    let msg = b"hello from stdin over KVM";
    let output = test_artifacts::expect_vm(
        sandbox.exec_with_stdin("cat", &[], msg).await,
        "guest exec cat (kvm_integration)",
    );

    assert!(output.success());
    assert_eq!(output.stdout, msg);
}

/// KVM-backed equivalent of `test_parity_text_transform` and `test_workflow_pipe`:
/// use a workflow where step1 echoes, step2 uppercases via `tr`, and pipe output.
#[tokio::test]
#[ignore = "requires KVM + kernel/initramfs artifacts; see module docs"]
async fn kvm_workflow_pipe_uppercase() {
    let sandbox = build_local_kvm_sandbox();

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

    let observed = test_artifacts::expect_vm(
        workflow
            .observe(ObserveConfig::test())
            .run_in(sandbox)
            .await,
        "workflow run (kvm_integration)",
    );

    let steps: String = observed
        .result
        .step_outputs
        .iter()
        .map(|(name, step)| {
            format!(
                "\n  step {name}: exit_code={} stdout='{}' stderr='{}'",
                step.exit_code,
                step.stdout_str(),
                step.stderr_str()
            )
        })
        .collect();
    let diag = format!(
        "kvm_workflow_pipe_uppercase: workflow exit_code={} output='{}'{steps}",
        observed.result.exit_code,
        observed.result.output_str()
    );
    assert!(observed.result.success(), "{diag}");

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
/// The auto-provisioned test image installs `claudio` as
/// `/usr/local/bin/claude-code`, so the guest exec resolves deterministically.
#[tokio::test]
#[ignore = "requires KVM + kernel/initramfs artifacts; see module docs"]
async fn kvm_claude_workflow_plan_apply() {
    let sandbox = build_local_kvm_sandbox();

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

    let observed = test_artifacts::expect_vm(
        workflow
            .observe(ObserveConfig::test())
            .run_in(sandbox)
            .await,
        "claude workflow run (kvm_integration)",
    );

    let steps: String = observed
        .result
        .step_outputs
        .iter()
        .map(|(name, step)| {
            format!(
                "\n  step {name}: exit_code={} stdout='{}' stderr='{}'",
                step.exit_code,
                step.stdout_str(),
                step.stderr_str()
            )
        })
        .collect();
    let diag = format!(
        "kvm_claude_workflow_plan_apply: workflow exit_code={} output='{}'{steps}",
        observed.result.exit_code,
        observed.result.output_str()
    );
    assert!(observed.result.success(), "{diag}");

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
