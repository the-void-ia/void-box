//! Honesty meta-test for the VM-test capability gate.
//!
//! The gate must classify a genuine hardware incapability (no hypervisor) as a
//! skip and every other error — a broken artifact, a boot timeout, a failed RPC
//! — as a failure, so a real regression on a capable machine can never be
//! laundered into a green skip. This test runs without a VM: it drives the
//! `test_artifacts` classifier and `vm_start` directly, so it guards the
//! invariant even where no VM can boot.

#[path = "common/test_artifacts.rs"]
mod test_artifacts;

use test_artifacts::{is_capability_absence, vm_start, VmStart};

/// The genuine capability-absence signals are recognized: the typed
/// `HypervisorUnavailable` marker raised at the `/dev/kvm` probe, and Apple's
/// hardware-availability message.
#[test]
fn recognizes_genuine_hypervisor_absence() {
    assert!(is_capability_absence(
        "hypervisor unavailable: cannot open /dev/kvm: Permission denied (os error 13)"
    ));
    assert!(is_capability_absence(
        "Backend error: VZ config validation: Internal error. \
         Virtualization is not available on this hardware."
    ));
}

/// A real failure must never be read as a capability absence. This is the core
/// anti-laundering invariant: the classifier stays narrow, so a config, boot, or
/// RPC regression on a capable host fails rather than skips. In particular the
/// plain `KVM error:` errnos are excluded — on aarch64 `KVM_ARM_VCPU_INIT`
/// raises ENOENT for an unknown feature bit, a real regression, not a missing
/// device.
#[test]
fn real_failures_are_not_capability_absence() {
    for message in [
        "KVM error: Invalid argument (os error 22)",
        "KVM error: No such file or directory (os error 2)",
        "KVM error: No such device (os error 19)",
        "control_channel: deadline reached (connect or handshake)",
        "Guest communication error: exec timed out after 30s",
        "Device error: vsock requested but virtio-vsock MMIO backend failed to initialize",
    ] {
        assert!(
            !is_capability_absence(message),
            "must be a failure, not a skip: {message}"
        );
    }
}

/// `vm_start` on a real failure panics — it does not return `SkipIncapable`. A
/// broken artifact boots to a control-channel timeout, which lands here.
#[test]
#[should_panic(expected = "not a skip")]
fn real_failure_fails_never_skips() {
    let result: Result<(), String> =
        Err("control_channel: deadline reached (connect or handshake)".into());
    let _ = vm_start(result, "meta");
}

/// `vm_start` on a genuine incapability skips when `VOID_BOX_REQUIRE_VM` is
/// unset. Remove the var first so the test holds even if the environment set it;
/// the failing direction (`REQUIRE_VM=1` fails even here) is exercised by the CI
/// VM lane, which sets it.
#[test]
fn incapability_skips_when_not_required() {
    std::env::remove_var("VOID_BOX_REQUIRE_VM");
    let result: Result<(), String> = Err("hypervisor unavailable: no /dev/kvm".into());
    assert!(
        matches!(vm_start(result, "meta"), VmStart::SkipIncapable),
        "a genuine incapability must skip when REQUIRE_VM is unset"
    );
}
