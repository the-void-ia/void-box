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
use void_box::Error;

/// `Error::HypervisorUnavailable` — the variant the backends raise only where a
/// hypervisor is genuinely absent (KVM's `/dev/kvm` probe, VZ's hardware-not-
/// available at config validation) — is a capability absence.
#[test]
fn recognizes_genuine_hypervisor_absence() {
    assert!(is_capability_absence(&Error::HypervisorUnavailable(
        "cannot open /dev/kvm: Permission denied".into()
    )));
}

/// A real failure must never be read as a capability absence. Classification is
/// on the error type, so every non-`HypervisorUnavailable` variant is a failure
/// — a config bug, a boot timeout, a failed RPC, a device error — including any
/// other `Error::Kvm` ioctl error, which cannot be constructed portably here but
/// is excluded by construction (only `HypervisorUnavailable` matches).
#[test]
fn real_failures_are_not_capability_absence() {
    let real: [Error; 4] = [
        Error::Backend("VZ config validation: invalid virtio device".into()),
        Error::Boot("control channel: deadline reached (connect or handshake)".into()),
        Error::Guest("exec timed out after 30s".into()),
        Error::Device("vsock requested but virtio-vsock MMIO backend failed to initialize".into()),
    ];
    for err in &real {
        assert!(
            !is_capability_absence(err),
            "must be a failure, not a skip: {err}"
        );
    }
}

/// `vm_start` on a real failure panics — it does not return `SkipIncapable`. A
/// broken artifact boots to a control-channel timeout, which lands here.
#[test]
#[should_panic(expected = "not a skip")]
fn real_failure_fails_never_skips() {
    let _ = vm_start(
        Err(Error::Boot(
            "control channel: deadline reached (connect or handshake)".into(),
        )),
        "meta",
    );
}

/// `vm_start` on a genuine incapability skips when `VOID_BOX_REQUIRE_VM` is
/// unset. Save and restore the var so the test never leaves the process env
/// mutated; the failing direction (`REQUIRE_VM=1` fails even here) is exercised
/// by the CI VM lane, which sets it.
#[test]
fn incapability_skips_when_not_required() {
    let saved = std::env::var("VOID_BOX_REQUIRE_VM").ok();
    std::env::remove_var("VOID_BOX_REQUIRE_VM");
    let outcome = vm_start(
        Err(Error::HypervisorUnavailable("no /dev/kvm".into())),
        "meta",
    );
    match saved {
        Some(value) => std::env::set_var("VOID_BOX_REQUIRE_VM", value),
        None => std::env::remove_var("VOID_BOX_REQUIRE_VM"),
    }
    assert!(
        matches!(outcome, VmStart::SkipIncapable),
        "a genuine incapability must skip when REQUIRE_VM is unset"
    );
}
