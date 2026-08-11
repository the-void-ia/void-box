//! End-to-end PTY session tests.
//!
//! Requires KVM, `VOID_BOX_KERNEL`, and `VOID_BOX_INITRAMFS` (test image
//! with BusyBox for `/bin/sh`).

#[path = "common/vm_preflight.rs"]
mod vm_preflight;

#[cfg(target_os = "linux")]
mod pty_tests {
    use super::vm_preflight;
    use void_box::sandbox::Sandbox;
    use void_box_protocol::PtyOpenRequest;

    fn skip_reason() -> Option<String> {
        let kernel = std::env::var_os("VOID_BOX_KERNEL").map(std::path::PathBuf::from);
        let initramfs = std::env::var_os("VOID_BOX_INITRAMFS").map(std::path::PathBuf::from);
        let reason = match (kernel.as_deref(), initramfs.as_deref()) {
            (Some(k), Some(i)) => vm_preflight::detect_capability(k, Some(i)).err(),
            _ => Some("VOID_BOX_KERNEL / VOID_BOX_INITRAMFS not set".to_string()),
        };
        if let Some(ref r) = reason {
            if vm_preflight::require_vm() {
                panic!("VOID_BOX_REQUIRE_VM=1 but VM is not available: {r}");
            }
        }
        reason
    }

    fn test_sandbox() -> Result<std::sync::Arc<Sandbox>, Box<dyn std::error::Error>> {
        let kernel = std::env::var("VOID_BOX_KERNEL")?;
        let initramfs = std::env::var("VOID_BOX_INITRAMFS")?;
        let sandbox = Sandbox::local()
            .kernel(&kernel)
            .initramfs(&initramfs)
            .memory_mb(512)
            .network(false)
            .build()?;
        Ok(sandbox)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore]
    async fn pty_open_and_immediate_exit() {
        if let Some(reason) = skip_reason() {
            eprintln!("SKIP: {}", reason);
            return;
        }

        let sandbox = test_sandbox().unwrap();

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
        if let Some(reason) = skip_reason() {
            eprintln!("SKIP: {}", reason);
            return;
        }

        let sandbox = test_sandbox().unwrap();

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
        if let Some(reason) = skip_reason() {
            eprintln!("SKIP: {}", reason);
            return;
        }

        let sandbox = test_sandbox().unwrap();

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
        if let Some(reason) = skip_reason() {
            eprintln!("SKIP: {}", reason);
            return;
        }

        let sandbox = test_sandbox().unwrap();

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
