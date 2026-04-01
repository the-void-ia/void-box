//! CLI handlers for `voidbox attach` and `voidbox shell`.

use std::sync::Arc;

use void_box::backend::pty_session::RawModeGuard;
use void_box_protocol::PtyOpenRequest;

/// Attaches to a running VM by run ID (not yet implemented).
pub async fn cmd_attach(
    _run_id: &str,
    _program: Option<&str>,
    _args: &[String],
    _working_dir: Option<&str>,
    _daemon_url: &str,
) -> Result<i32, Box<dyn std::error::Error>> {
    Err("attach to a running VM is not yet implemented".into())
}

/// Boots an ephemeral VM and attaches an interactive PTY session.
pub async fn cmd_shell(
    program: &str,
    args: &[String],
    working_dir: Option<&str>,
    memory_mb: usize,
    image: Option<&str>,
    network: bool,
) -> Result<i32, Box<dyn std::error::Error>> {
    let kernel = std::env::var("VOID_BOX_KERNEL").map_err(|_| "VOID_BOX_KERNEL not set")?;
    let initramfs = std::env::var("VOID_BOX_INITRAMFS").ok();

    let mut builder = void_box::sandbox::Sandbox::local()
        .kernel(&kernel)
        .memory_mb(memory_mb)
        .network(network);

    if let Some(path) = &initramfs {
        builder = builder.initramfs(path);
    }

    let _ = image;

    let sandbox = builder.build()?;

    let (cols, rows) = terminal_size()?;

    let request = PtyOpenRequest {
        cols,
        rows,
        program: program.to_string(),
        args: args.to_vec(),
        env: Vec::new(),
        working_dir: working_dir.map(String::from),
    };

    let session = sandbox.attach_pty(request).await?;

    let _guard = RawModeGuard::engage(0).map_err(|e| format!("failed to enter raw mode: {e}"))?;

    let exit_code = tokio::task::spawn_blocking(move || session.run())
        .await
        .map_err(|e| format!("pty task panicked: {e}"))??;

    drop(_guard);
    stop_sandbox(sandbox).await;

    Ok(exit_code)
}

/// Reads the current terminal size via ioctl.
fn terminal_size() -> Result<(u16, u16), Box<dyn std::error::Error>> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) };
    if ret < 0 {
        return Err(format!("TIOCGWINSZ failed: {}", std::io::Error::last_os_error()).into());
    }
    Ok((ws.ws_col, ws.ws_row))
}

/// Stops the sandbox, ignoring errors.
async fn stop_sandbox(sandbox: Arc<void_box::sandbox::Sandbox>) {
    let _ = sandbox.stop().await;
}
