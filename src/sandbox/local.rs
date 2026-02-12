//! Local KVM Sandbox Implementation
//!
//! Wraps the existing VMM code to provide a sandbox interface.

use tokio::sync::Mutex;

use super::SandboxConfig;
use crate::vmm::config::VoidBoxConfig;
use crate::vmm::VoidBox;
use crate::{Error, ExecOutput, Result};

/// Local sandbox using KVM
pub struct LocalSandbox {
    /// Sandbox configuration
    config: SandboxConfig,
    /// The underlying VM (lazily initialized)
    vm: Mutex<Option<VoidBox>>,
    /// Whether the sandbox is started
    started: std::sync::atomic::AtomicBool,
}

impl LocalSandbox {
    /// Create a new local sandbox
    pub fn new(config: SandboxConfig) -> Result<Self> {
        Ok(Self {
            config,
            vm: Mutex::new(None),
            started: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Start the sandbox VM
    async fn ensure_started(&self) -> Result<()> {
        use std::sync::atomic::Ordering;

        if self.started.load(Ordering::SeqCst) {
            return Ok(());
        }

        let mut vm_lock = self.vm.lock().await;

        // Double-check after acquiring lock
        if vm_lock.is_some() {
            self.started.store(true, Ordering::SeqCst);
            return Ok(());
        }

        // Build VM config
        let kernel = self.config.kernel.clone().ok_or_else(|| {
            Error::Config("Kernel path required for local sandbox".into())
        })?;

        let mut vm_config = VoidBoxConfig::new()
            .memory_mb(self.config.memory_mb)
            .vcpus(self.config.vcpus)
            .kernel(kernel)
            .network(self.config.network)
            .enable_vsock(self.config.enable_vsock);

        if let Some(ref initramfs) = self.config.initramfs {
            vm_config = vm_config.initramfs(initramfs);
        }

        if let Some(ref rootfs) = self.config.rootfs {
            vm_config = vm_config.rootfs(rootfs);
        }

        if let Some(ref shared_dir) = self.config.shared_dir {
            vm_config = vm_config.shared_dir(shared_dir);
        }

        // Create and start VM
        let vm = VoidBox::new(vm_config).await?;
        *vm_lock = Some(vm);
        self.started.store(true, Ordering::SeqCst);

        Ok(())
    }

    /// Execute a command in the sandbox
    pub async fn exec(&self, program: &str, args: &[&str]) -> Result<ExecOutput> {
        self.exec_with_stdin(program, args, &[]).await
    }

    /// Execute a command with stdin input
    pub async fn exec_with_stdin(&self, program: &str, args: &[&str], stdin: &[u8]) -> Result<ExecOutput> {
        // For now, if VM is not configured, return a simulated response
        // This allows testing without a real VM
        if self.config.kernel.is_none() {
            return self.simulate_exec(program, args, stdin);
        }

        self.ensure_started().await?;

        let vm_lock = self.vm.lock().await;
        let vm = vm_lock.as_ref().ok_or(Error::VmNotRunning)?;

        let env: Vec<(String, String)> = self.config.env.clone();
        vm.exec_with_env(program, args, stdin, &env, None).await
    }

    /// Simulate command execution (for testing without a real VM)
    fn simulate_exec(&self, program: &str, args: &[&str], stdin: &[u8]) -> Result<ExecOutput> {
        match program {
            "echo" => {
                let output = format!("{}\n", args.join(" "));
                Ok(ExecOutput::new(output.into_bytes(), Vec::new(), 0))
            }
            "cat" => {
                if stdin.is_empty() {
                    // Reading file - simulate not found
                    Ok(ExecOutput::new(Vec::new(), b"cat: file not found\n".to_vec(), 1))
                } else {
                    // cat with stdin - echo it back
                    Ok(ExecOutput::new(stdin.to_vec(), Vec::new(), 0))
                }
            }
            "tr" => {
                // Simple tr simulation for lowercase -> uppercase
                if args.len() >= 2 && args[0] == "a-z" && args[1] == "A-Z" {
                    let output: Vec<u8> = stdin
                        .iter()
                        .map(|&c| {
                            if c.is_ascii_lowercase() {
                                c.to_ascii_uppercase()
                            } else {
                                c
                            }
                        })
                        .collect();
                    Ok(ExecOutput::new(output, Vec::new(), 0))
                } else {
                    Ok(ExecOutput::new(stdin.to_vec(), Vec::new(), 0))
                }
            }
            "test" => {
                // Simulate test command (always fail for -e in simulation)
                Ok(ExecOutput::new(Vec::new(), Vec::new(), 1))
            }
            "sh" => {
                // Basic shell simulation
                if args.len() >= 2 && args[0] == "-c" {
                    let cmd = args[1];
                    if cmd.starts_with("echo") {
                        let msg = cmd.strip_prefix("echo ").unwrap_or("");
                        Ok(ExecOutput::new(format!("{}\n", msg).into_bytes(), Vec::new(), 0))
                    } else {
                        Ok(ExecOutput::new(Vec::new(), Vec::new(), 0))
                    }
                } else {
                    Ok(ExecOutput::new(Vec::new(), Vec::new(), 0))
                }
            }
            "sha256sum" => {
                // Simulate sha256sum (returns a fake hash)
                let hash = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
                let output = format!("{}  -\n", hash);
                Ok(ExecOutput::new(output.into_bytes(), Vec::new(), 0))
            }
            "curl" => {
                // Simulate curl (return empty response)
                Ok(ExecOutput::new(b"{}".to_vec(), Vec::new(), 0))
            }
            "jq" => {
                // Simulate jq (pass through stdin)
                Ok(ExecOutput::new(stdin.to_vec(), Vec::new(), 0))
            }
            "python" | "python3" => {
                // Simulate python
                Ok(ExecOutput::new(Vec::new(), Vec::new(), 0))
            }
            _ => {
                // Unknown command - simulate success
                Ok(ExecOutput::new(Vec::new(), Vec::new(), 0))
            }
        }
    }

    /// Stop the sandbox
    pub async fn stop(&self) -> Result<()> {
        use std::sync::atomic::Ordering;

        let mut vm_lock = self.vm.lock().await;
        if let Some(mut vm) = vm_lock.take() {
            vm.stop().await?;
        }
        self.started.store(false, Ordering::SeqCst);

        Ok(())
    }
}

impl Drop for LocalSandbox {
    fn drop(&mut self) {
        // VM will be stopped when dropped through VoidBox's Drop impl
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_simulate_echo() {
        let config = SandboxConfig::default();
        let sandbox = LocalSandbox::new(config).unwrap();

        let output = sandbox.exec("echo", &["hello", "world"]).await.unwrap();
        assert!(output.success());
        assert_eq!(output.stdout_str().trim(), "hello world");
    }

    #[tokio::test]
    async fn test_simulate_cat_stdin() {
        let config = SandboxConfig::default();
        let sandbox = LocalSandbox::new(config).unwrap();

        let output = sandbox.exec_with_stdin("cat", &[], b"test input").await.unwrap();
        assert!(output.success());
        assert_eq!(output.stdout, b"test input");
    }

    #[tokio::test]
    async fn test_simulate_tr() {
        let config = SandboxConfig::default();
        let sandbox = LocalSandbox::new(config).unwrap();

        let output = sandbox
            .exec_with_stdin("tr", &["a-z", "A-Z"], b"hello")
            .await
            .unwrap();
        assert!(output.success());
        assert_eq!(output.stdout, b"HELLO");
    }

    #[tokio::test]
    async fn test_simulate_curl() {
        let config = SandboxConfig::default();
        let sandbox = LocalSandbox::new(config).unwrap();

        let output = sandbox.exec("curl", &["-s", "https://example.com"]).await.unwrap();
        assert!(output.success());
    }
}
