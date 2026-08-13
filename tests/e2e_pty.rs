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

    fn test_sandbox() -> std::sync::Arc<Sandbox> {
        let (kernel, initramfs) = test_artifacts::artifacts();
        let build = Sandbox::local()
            .kernel(&kernel)
            .initramfs(&initramfs)
            .memory_mb(512)
            .network(false)
            .build();
        test_artifacts::expect_vm(build, "sandbox build (e2e_pty)")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore]
    async fn pty_open_and_immediate_exit() {
        let sandbox = test_sandbox();

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
        let sandbox = test_sandbox();

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
        let sandbox = test_sandbox();

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
        let sandbox = test_sandbox();

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
