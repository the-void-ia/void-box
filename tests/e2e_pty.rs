//! End-to-end PTY session tests.
//!
//! Requires KVM, `VOID_BOX_KERNEL`, and `VOID_BOX_INITRAMFS` (test image
//! with BusyBox for `/bin/sh`).

#[path = "common/test_artifacts.rs"]
mod test_artifacts;

#[cfg(target_os = "linux")]
mod pty_tests {
    use super::test_artifacts;
    use void_box::sandbox::Sandbox;
    use void_box_protocol::PtyOpenRequest;

    /// Build the sandbox and boot it with a gated first exec. `None` means the
    /// host cannot virtualize and the test must return early (skip). Booting
    /// here — rather than letting the first `attach_pty` do it — keeps every
    /// PTY-op result meaningful: the tests assert on `attach_pty` outcomes
    /// (`pty_command_not_allowed` even asserts on an expected `Err`'s
    /// message), and a hypervisor absence surfacing through those calls would
    /// fail the assertions confusingly instead of skipping.
    async fn test_sandbox() -> Option<std::sync::Arc<Sandbox>> {
        let (kernel, initramfs) = test_artifacts::artifacts();
        let build = Sandbox::local()
            .kernel(&kernel)
            .initramfs(&initramfs)
            .memory_mb(512)
            .network(false)
            .build();
        let sandbox = test_artifacts::expect_vm(build, "sandbox build (e2e_pty)");
        test_artifacts::vm_start_value(
            sandbox.exec("echo", &["boot"]).await,
            "first boot exec (e2e_pty)",
        )?;
        Some(sandbox)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore]
    async fn pty_open_and_immediate_exit() {
        let Some(sandbox) = test_sandbox().await else {
            return;
        };

        let request = PtyOpenRequest {
            cols: 80,
            rows: 24,
            program: "sh".to_string(),
            args: vec!["-c".to_string(), "echo hello-pty && exit 0".to_string()],
            env: vec![],
            working_dir: None,
            interactive: false,
        };

        let session = sandbox.attach_pty(request).await.unwrap();
        let exit_code = tokio::task::spawn_blocking(move || session.run())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(exit_code, 0);
        let _ = sandbox.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore]
    async fn pty_command_not_allowed() {
        let Some(sandbox) = test_sandbox().await else {
            return;
        };

        let request = PtyOpenRequest {
            cols: 80,
            rows: 24,
            program: "forbidden-binary".to_string(),
            args: vec![],
            env: vec![],
            working_dir: None,
            interactive: false,
        };

        let result = sandbox.attach_pty(request).await;
        let Err(err) = result else {
            panic!("expected attach_pty to fail for forbidden-binary");
        };
        let err = err.to_string();
        assert!(err.contains("not allowed"), "unexpected error: {}", err);

        let _ = sandbox.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore]
    async fn pty_nonzero_exit_code() {
        let Some(sandbox) = test_sandbox().await else {
            return;
        };

        let request = PtyOpenRequest {
            cols: 80,
            rows: 24,
            program: "sh".to_string(),
            args: vec!["-c".to_string(), "exit 42".to_string()],
            env: vec![],
            working_dir: None,
            interactive: false,
        };

        let session = sandbox.attach_pty(request).await.unwrap();
        let exit_code = tokio::task::spawn_blocking(move || session.run())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(exit_code, 42);
        let _ = sandbox.stop().await;
    }

    /// A child that exits shortly after the host closes the session must
    /// still report its own exit code, not the teardown signal. In a
    /// non-interactive run the host's stdin reaches EOF immediately, closing
    /// the session while the child is still sleeping; the guest must let the
    /// child finish rather than hanging it up first (which would surface as
    /// `128 + SIGHUP = 129`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore]
    async fn pty_exit_code_survives_session_close() {
        let Some(sandbox) = test_sandbox().await else {
            return;
        };

        let request = PtyOpenRequest {
            cols: 80,
            rows: 24,
            program: "sh".to_string(),
            args: vec!["-c".to_string(), "sleep 1; exit 42".to_string()],
            env: vec![],
            working_dir: None,
            interactive: false,
        };

        let session = sandbox.attach_pty(request).await.unwrap();
        let exit_code = tokio::task::spawn_blocking(move || session.run())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(exit_code, 42);
        let _ = sandbox.stop().await;
    }
}
