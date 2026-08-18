//! Honesty meta-test for the VM-test capability gate: a genuine incapability
//! classifies as a skip, every other error as a failure, so a real regression
//! is never laundered into a green skip. Runs without a VM — it drives the
//! `test_artifacts` classifier, `vm_start`, and `vm_start_value` directly.

#[path = "common/test_artifacts.rs"]
mod test_artifacts;

use test_artifacts::{is_capability_absence, vm_start, vm_start_value, VmStart};
use void_box::Error;

/// `Error::HypervisorUnavailable` — the variant the backends raise only where
/// no hypervisor is available to the process (KVM's `/dev/kvm` probe on the
/// absent-device and access-denied errnos, VZ's hardware-not-available at
/// config validation) — is a capability absence.
#[test]
fn recognizes_genuine_hypervisor_absence() {
    assert!(is_capability_absence(&Error::HypervisorUnavailable(
        "cannot open /dev/kvm: Permission denied".into()
    )));
}

/// A real failure must never be read as a capability absence: every
/// non-`HypervisorUnavailable` variant — a config bug, boot timeout, RPC
/// failure, device error, or any other `Error::Kvm` — fails, not skips.
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

/// `vm_start_value` — the value-preserving twin for constructors and lazy-boot
/// first ops — on a real failure panics, exactly like `vm_start`.
#[test]
#[should_panic(expected = "not a skip")]
fn value_gate_real_failure_fails_never_skips() {
    let _: Option<u32> = vm_start_value(
        Err(Error::Boot(
            "control channel: deadline reached (connect or handshake)".into(),
        )),
        "meta",
    );
}

/// `vm_start_value` passes the booted value through untouched on success. The
/// env var is irrelevant here: classification only runs on `Err`.
#[test]
fn value_gate_preserves_the_value() {
    assert_eq!(vm_start_value(Ok(42_u32), "meta"), Some(42));
}

/// `vm_start` and `vm_start_value` on a genuine incapability skip when
/// `VOID_BOX_REQUIRE_VM` is unset. Both gates are exercised under one
/// save-and-restore of the var — two tests each mutating the process env would
/// race under the default parallel test runner, and the gates read the var
/// only on a capability absence, so this test is also the only concurrent
/// *reader* (the real-failure meta-tests never touch the env). The failing
/// direction (`REQUIRE_VM=1` fails even here) is exercised by the CI VM lane,
/// which sets it.
#[test]
fn incapability_skips_when_not_required() {
    let saved = std::env::var("VOID_BOX_REQUIRE_VM").ok();
    std::env::remove_var("VOID_BOX_REQUIRE_VM");
    let outcome = vm_start(
        Err(Error::HypervisorUnavailable("no /dev/kvm".into())),
        "meta",
    );
    let value_outcome: Option<u32> = vm_start_value(
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
    assert!(
        value_outcome.is_none(),
        "a genuine incapability must yield None (skip) when REQUIRE_VM is unset"
    );
}
