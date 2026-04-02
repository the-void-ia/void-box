//! End-to-end PTY session tests.
//!
//! Requires KVM, `VOID_BOX_KERNEL`, and `VOID_BOX_INITRAMFS` (test image
//! with BusyBox for `/bin/sh`).

#[cfg(target_os = "linux")]
mod pty_tests {
    use void_box::sandbox::Sandbox;
    use void_box_protocol::PtyOpenRequest;

    fn skip_reason() -> Option<String> {
        if std::env::var("VOID_BOX_KERNEL").is_err() {
            return Some("VOID_BOX_KERNEL not set".into());
        }
        if std::env::var("VOID_BOX_INITRAMFS").is_err() {
            return Some("VOID_BOX_INITRAMFS not set".into());
        }
        None
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

    #[tokio::test]
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

    #[tokio::test]
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

    #[tokio::test]
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
}
