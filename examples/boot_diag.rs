#![cfg(target_os = "linux")]
//! Boot diagnostic: start a VM and capture serial output to see what the guest kernel is doing.

use std::path::PathBuf;
use void_box::vmm::config::VoidBoxConfig;
use void_box::vmm::MicroVm;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let kernel = std::env::var("VOID_BOX_KERNEL").expect("VOID_BOX_KERNEL required");
    let initramfs = std::env::var("VOID_BOX_INITRAMFS").ok();

    let mut config = VoidBoxConfig::new()
        .memory_mb(256)
        .vcpus(1)
        .kernel(PathBuf::from(&kernel))
        .network(true)
        .enable_vsock(true);

    if let Some(ref p) = initramfs {
        config = config.initramfs(PathBuf::from(p));
    }

    let mut vm = MicroVm::new(config).await.expect("Failed to create VM");

    eprintln!("[diag] VM started, reading serial output for 20 seconds...");

    for _ in 0..200 {
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        let output = vm.read_serial_output();
        if !output.is_empty() {
            let s = String::from_utf8_lossy(&output);
            print!("{}", s);
        }
    }

    eprintln!("\n[diag] Done, stopping VM.");
    let _ = vm.stop().await;
}
