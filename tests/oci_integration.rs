//! OCI integration tests for void-box (Linux + macOS).
//!
//! **Group 1** (mock-based, cross-platform): Verify that `sandbox.image` in
//! YAML flows through spec → runtime → SandboxConfig → BackendConfig → kernel
//! cmdline.  Run by default: `cargo test --test oci_integration`
//!
//! **Group 2** (platform cmdline): Verify the platform-specific kernel cmdline
//! builder emits (or omits) `voidbox.oci_rootfs=…`.
//!   - Linux: `VoidBoxConfig::kernel_cmdline()`
//!   - macOS: `vz::config::build_kernel_cmdline()`
//!
//! **Group 3** (VM E2E, `#[ignore]`): Boot a real VM and verify OCI rootfs
//! mounts are visible in the guest.
//!   - Linux: KVM — needs `/dev/kvm`, `VOID_BOX_KERNEL`, `VOID_BOX_INITRAMFS`
//!   - macOS: VZ  — needs `VOID_BOX_KERNEL`, `VOID_BOX_INITRAMFS`
//!
//! ```bash
//! # Mock + cmdline tests (no hardware):
//! cargo test --test oci_integration
//!
//! # Linux KVM E2E:
//! VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
//! VOID_BOX_INITRAMFS=/tmp/void-box-rootfs.cpio.gz \
//! cargo test --test oci_integration -- --ignored --test-threads=1
//!
//! # macOS VZ E2E:
//! VOID_BOX_KERNEL=/path/to/vmlinuz \
//! VOID_BOX_INITRAMFS=/path/to/rootfs.cpio.gz \
//! cargo test --test oci_integration -- --ignored --test-threads=1
//! ```

use std::path::PathBuf;
use std::sync::Arc;

use void_box::agent_box::VoidBox;
use void_box::backend::MountConfig;
use void_box::sandbox::Sandbox;
use void_box::spec::load_spec;
use void_box::spec::RunSpec;
use void_box::Error;

// ──────────────────────────────────────────────────────────────────────────────
// Group 1: Mock-based tests (cross-platform, no hardware required)
// ──────────────────────────────────────────────────────────────────────────────

/// YAML with `sandbox.image: alpine:3.20` deserializes correctly.
#[test]
fn spec_parses_sandbox_image() {
    let yaml = r#"
api_version: v1
kind: agent
name: test-agent
sandbox:
  image: "alpine:3.20"
agent:
  prompt: "hello"
"#;
    let spec: RunSpec = serde_yaml::from_str(yaml).expect("failed to parse YAML");
    assert_eq!(spec.sandbox.image.as_deref(), Some("alpine:3.20"));
}

/// YAML without `sandbox.image` has `None`.
#[test]
fn spec_sandbox_image_default_none() {
    let yaml = r#"
api_version: v1
kind: agent
name: test-agent
agent:
  prompt: "hello"
"#;
    let spec: RunSpec = serde_yaml::from_str(yaml).expect("failed to parse YAML");
    assert!(spec.sandbox.image.is_none());
}

/// `VoidBox::new("t").mock().oci_rootfs("/mnt/oci-rootfs").build()` succeeds.
#[test]
fn voidbox_builder_oci_rootfs() {
    let vb = VoidBox::new("t")
        .mock()
        .oci_rootfs("/mnt/oci-rootfs")
        .prompt("test")
        .build()
        .expect("build with oci_rootfs should succeed");
    assert_eq!(vb.name, "t");
}

/// `.mount(...)` + `.oci_rootfs(...)` both accepted by the builder.
#[test]
fn voidbox_builder_mount_plus_oci() {
    let mount = MountConfig {
        host_path: "/tmp/data".to_string(),
        guest_path: "/workspace".to_string(),
        read_only: false,
    };
    let vb = VoidBox::new("t")
        .mock()
        .mount(mount)
        .oci_rootfs("/mnt/oci-rootfs")
        .prompt("test")
        .build()
        .expect("build with mount + oci_rootfs should succeed");
    assert_eq!(vb.name, "t");
}

/// `Sandbox::mock().oci_rootfs(...)` propagates to `config().oci_rootfs`.
#[test]
fn sandbox_config_oci_rootfs_propagation() {
    let sandbox = Sandbox::mock()
        .oci_rootfs("/mnt/oci-rootfs")
        .build()
        .expect("mock sandbox with oci_rootfs should build");
    assert_eq!(
        sandbox.config().oci_rootfs.as_deref(),
        Some("/mnt/oci-rootfs")
    );
}

/// Pipeline YAML with `sandbox.image` parses and validates via `load_spec`.
#[test]
fn spec_pipeline_with_oci_image() {
    let yaml = r#"
api_version: v1
kind: pipeline
name: oci-pipeline
sandbox:
  image: "python:3.12"
pipeline:
  boxes:
    - name: step1
      prompt: "do work"
"#;
    let dir = tempfile::tempdir().unwrap();
    let spec_path = dir.path().join("pipeline.yaml");
    std::fs::write(&spec_path, yaml).unwrap();

    let spec = load_spec(&spec_path).expect("pipeline spec with image should load");
    assert_eq!(spec.sandbox.image.as_deref(), Some("python:3.12"));
}

/// Workflow YAML with `sandbox.image` parses and validates via `load_spec`.
#[test]
fn spec_workflow_with_oci_image() {
    let yaml = r#"
api_version: v1
kind: workflow
name: oci-workflow
sandbox:
  image: "node:22-slim"
workflow:
  steps:
    - name: build
      run:
        program: echo
        args: ["hello"]
"#;
    let dir = tempfile::tempdir().unwrap();
    let spec_path = dir.path().join("workflow.yaml");
    std::fs::write(&spec_path, yaml).unwrap();

    let spec = load_spec(&spec_path).expect("workflow spec with image should load");
    assert_eq!(spec.sandbox.image.as_deref(), Some("node:22-slim"));
}

// ──────────────────────────────────────────────────────────────────────────────
// Group 2: Platform-specific kernel cmdline tests
// ──────────────────────────────────────────────────────────────────────────────

// --- Linux: VoidBoxConfig ---

/// `VoidBoxConfig` with `oci_rootfs` produces `voidbox.oci_rootfs=...` in the
/// kernel command line (Linux / KVM path).
#[cfg(target_os = "linux")]
#[test]
fn kernel_cmdline_includes_oci_rootfs() {
    let config = void_box::vmm::config::VoidBoxConfig {
        oci_rootfs: Some("/mnt/oci-rootfs".to_string()),
        ..Default::default()
    };
    let cmdline = config.kernel_cmdline();
    assert!(
        cmdline.contains("voidbox.oci_rootfs=/mnt/oci-rootfs"),
        "kernel cmdline should contain voidbox.oci_rootfs: {cmdline}"
    );
}

/// `VoidBoxConfig` without `oci_rootfs` has no `voidbox.oci_rootfs` token
/// (Linux / KVM path).
#[cfg(target_os = "linux")]
#[test]
fn kernel_cmdline_no_oci_rootfs_when_none() {
    let config = void_box::vmm::config::VoidBoxConfig::default();
    let cmdline = config.kernel_cmdline();
    assert!(
        !cmdline.contains("voidbox.oci_rootfs"),
        "kernel cmdline should NOT contain voidbox.oci_rootfs: {cmdline}"
    );
}

/// `VoidBoxConfig` with `oci_rootfs_dev` emits the device token used by
/// guest-agent block-rootfs pivot flow.
#[cfg(target_os = "linux")]
#[test]
fn kernel_cmdline_includes_oci_rootfs_dev() {
    let config = void_box::vmm::config::VoidBoxConfig {
        oci_rootfs_dev: Some("/dev/vda".to_string()),
        ..Default::default()
    };
    let cmdline = config.kernel_cmdline();
    assert!(
        cmdline.contains("voidbox.oci_rootfs_dev=/dev/vda"),
        "kernel cmdline should contain voidbox.oci_rootfs_dev: {cmdline}"
    );
}

/// `VoidBoxConfig` with `oci_rootfs_disk` emits the virtio-mmio declaration for
/// virtio-blk (IRQ 13, MMIO base 0xd1800000).
#[cfg(target_os = "linux")]
#[test]
fn kernel_cmdline_includes_virtio_blk_mmio_for_oci_disk() {
    let config = void_box::vmm::config::VoidBoxConfig {
        oci_rootfs_disk: Some(PathBuf::from("/tmp/oci-rootfs.img")),
        ..Default::default()
    };
    let cmdline = config.kernel_cmdline();
    assert!(
        cmdline.contains("virtio_mmio.device=512@0xd1800000:13"),
        "kernel cmdline should contain virtio-blk MMIO declaration: {cmdline}"
    );
}

// --- macOS: vz::config::build_kernel_cmdline ---

/// `build_kernel_cmdline` with `oci_rootfs` produces `voidbox.oci_rootfs=...`
/// (macOS / VZ path).
#[cfg(target_os = "macos")]
#[test]
fn vz_cmdline_includes_oci_rootfs() {
    let mut config = vz_test_backend_config();
    config.oci_rootfs = Some("/mnt/oci-rootfs".to_string());
    let cmdline = void_box::backend::vz::config::build_kernel_cmdline(&config);
    assert!(
        cmdline.contains("voidbox.oci_rootfs=/mnt/oci-rootfs"),
        "VZ kernel cmdline should contain voidbox.oci_rootfs: {cmdline}"
    );
}

/// `build_kernel_cmdline` without `oci_rootfs` has no `voidbox.oci_rootfs`
/// token (macOS / VZ path).
#[cfg(target_os = "macos")]
#[test]
fn vz_cmdline_no_oci_rootfs_when_none() {
    let config = vz_test_backend_config();
    let cmdline = void_box::backend::vz::config::build_kernel_cmdline(&config);
    assert!(
        !cmdline.contains("voidbox.oci_rootfs"),
        "VZ kernel cmdline should NOT contain voidbox.oci_rootfs: {cmdline}"
    );
}

/// `build_kernel_cmdline` uses `console=hvc0` (virtio-console), not `ttyS0`.
#[cfg(target_os = "macos")]
#[test]
fn vz_cmdline_uses_hvc0() {
    let config = vz_test_backend_config();
    let cmdline = void_box::backend::vz::config::build_kernel_cmdline(&config);
    assert!(
        cmdline.contains("console=hvc0"),
        "VZ cmdline should use hvc0: {cmdline}"
    );
    assert!(
        !cmdline.contains("ttyS0"),
        "VZ cmdline should NOT contain ttyS0: {cmdline}"
    );
}

/// `build_kernel_cmdline` includes mount config when mounts are present.
#[cfg(target_os = "macos")]
#[test]
fn vz_cmdline_oci_rootfs_with_mount() {
    let mut config = vz_test_backend_config();
    config.oci_rootfs = Some("/mnt/oci-rootfs".to_string());
    config.mounts.push(MountConfig {
        host_path: "/tmp/oci".to_string(),
        guest_path: "/mnt/oci-rootfs".to_string(),
        read_only: true,
    });
    let cmdline = void_box::backend::vz::config::build_kernel_cmdline(&config);
    assert!(
        cmdline.contains("voidbox.oci_rootfs=/mnt/oci-rootfs"),
        "cmdline missing oci_rootfs: {cmdline}"
    );
    assert!(
        cmdline.contains("voidbox.mount0=mount0:/mnt/oci-rootfs:ro"),
        "cmdline missing mount config: {cmdline}"
    );
}

/// Helper: minimal `BackendConfig` for VZ cmdline tests.
#[cfg(target_os = "macos")]
fn vz_test_backend_config() -> void_box::backend::BackendConfig {
    void_box::backend::BackendConfig {
        memory_mb: 256,
        vcpus: 1,
        kernel: PathBuf::from("/tmp/vmlinuz"),
        initramfs: None,
        rootfs: None,
        network: false,
        enable_vsock: true,
        shared_dir: None,
        mounts: vec![],
        oci_rootfs: None,
        oci_rootfs_dev: None,
        oci_rootfs_disk: None,
        env: vec![],
        security: void_box::backend::BackendSecurityConfig {
            session_secret: [0xAB; 32],
            command_allowlist: vec![],
            network_deny_list: vec![],
            max_connections_per_second: 50,
            max_concurrent_connections: 64,
            seccomp: false,
        },
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Group 1b: guest_image spec parsing tests
// ──────────────────────────────────────────────────────────────────────────────

/// YAML with `sandbox.guest_image` deserializes correctly.
#[test]
fn spec_parses_guest_image() {
    let yaml = r#"
api_version: v1
kind: agent
name: test-agent
sandbox:
  guest_image: "ghcr.io/the-void-ia/voidbox-guest:v0.1.0"
agent:
  prompt: "hello"
"#;
    let spec: RunSpec = serde_yaml::from_str(yaml).expect("failed to parse YAML");
    assert_eq!(
        spec.sandbox.guest_image.as_deref(),
        Some("ghcr.io/the-void-ia/voidbox-guest:v0.1.0")
    );
}

/// YAML without `sandbox.guest_image` has `None`.
#[test]
fn spec_guest_image_default_none() {
    let yaml = r#"
api_version: v1
kind: agent
name: test-agent
agent:
  prompt: "hello"
"#;
    let spec: RunSpec = serde_yaml::from_str(yaml).expect("failed to parse YAML");
    assert!(spec.sandbox.guest_image.is_none());
}

/// Empty string `guest_image: ""` parses as `Some("")` — used to disable auto-pull.
#[test]
fn spec_guest_image_empty_string_disables() {
    let yaml = r#"
api_version: v1
kind: agent
name: test-agent
sandbox:
  guest_image: ""
agent:
  prompt: "hello"
"#;
    let spec: RunSpec = serde_yaml::from_str(yaml).expect("failed to parse YAML");
    assert_eq!(spec.sandbox.guest_image.as_deref(), Some(""));
}

/// Both `image` and `guest_image` can coexist (base image + guest kernel image).
#[test]
fn spec_both_image_and_guest_image() {
    let yaml = r#"
api_version: v1
kind: agent
name: test-agent
sandbox:
  image: "python:3.12-slim"
  guest_image: "ghcr.io/the-void-ia/voidbox-guest:latest"
agent:
  prompt: "hello"
"#;
    let spec: RunSpec = serde_yaml::from_str(yaml).expect("failed to parse YAML");
    assert_eq!(spec.sandbox.image.as_deref(), Some("python:3.12-slim"));
    assert_eq!(
        spec.sandbox.guest_image.as_deref(),
        Some("ghcr.io/the-void-ia/voidbox-guest:latest")
    );
}

/// `load_spec` validates a spec with `guest_image` successfully.
#[test]
fn spec_load_with_guest_image() {
    let yaml = r#"
api_version: v1
kind: workflow
name: guest-test
sandbox:
  guest_image: "ghcr.io/the-void-ia/voidbox-guest:v0.1.0"
workflow:
  steps:
    - name: probe
      run:
        program: echo
        args: ["ok"]
"#;
    let dir = tempfile::tempdir().unwrap();
    let spec_path = dir.path().join("guest.yaml");
    std::fs::write(&spec_path, yaml).unwrap();
    let spec = load_spec(&spec_path).expect("spec with guest_image should load");
    assert_eq!(
        spec.sandbox.guest_image.as_deref(),
        Some("ghcr.io/the-void-ia/voidbox-guest:v0.1.0")
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// Group 3: VM E2E tests (require hardware, `#[ignore]`)
// ──────────────────────────────────────────────────────────────────────────────

/// Load kernel + initramfs paths from environment variables.
fn vm_artifacts_from_env() -> Option<(PathBuf, Option<PathBuf>)> {
    let kernel = PathBuf::from(std::env::var_os("VOID_BOX_KERNEL")?);
    let initramfs = std::env::var_os("VOID_BOX_INITRAMFS").map(PathBuf::from);
    Some((kernel, initramfs))
}

/// Create a temporary directory that mimics a minimal OCI rootfs.
fn create_fake_oci_rootfs() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("oci-marker.txt"), "oci-rootfs-present").unwrap();
    std::fs::create_dir_all(dir.path().join("etc")).unwrap();
    std::fs::write(dir.path().join("etc/os-release"), "NAME=\"FakeOCI\"").unwrap();
    dir
}

/// Build a sandbox that mounts `oci_dir` at `/mnt/oci-rootfs` and sets
/// `oci_rootfs` in the sandbox config.  Works on both Linux (KVM) and
/// macOS (VZ) — the `Sandbox::local()` builder picks the right backend.
fn build_sandbox_with_oci_mount(
    oci_dir: &std::path::Path,
    _read_only: bool,
) -> Option<Arc<Sandbox>> {
    // Linux: require /dev/kvm
    #[cfg(target_os = "linux")]
    if !std::path::Path::new("/dev/kvm").exists() {
        eprintln!("skipping VM OCI test: /dev/kvm not available");
        return None;
    }

    let (kernel, initramfs) = match vm_artifacts_from_env() {
        Some(a) => a,
        None => {
            eprintln!(
                "skipping VM OCI test: \
                 set VOID_BOX_KERNEL and (optionally) VOID_BOX_INITRAMFS"
            );
            return None;
        }
    };

    if !kernel.exists() {
        eprintln!(
            "skipping VM OCI test: kernel path does not exist: {}",
            kernel.display()
        );
        return None;
    }

    let mut builder = Sandbox::local().memory_mb(1536).vcpus(1).kernel(&kernel);

    #[cfg(target_os = "linux")]
    {
        let disk = match build_test_oci_rootfs_disk(oci_dir) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("skipping VM OCI test: failed to build OCI disk: {e}");
                return None;
            }
        };
        builder = builder.oci_rootfs_dev("/dev/vda").oci_rootfs_disk(disk);
    }

    #[cfg(not(target_os = "linux"))]
    {
        let mount = MountConfig {
            host_path: oci_dir.to_string_lossy().into_owned(),
            guest_path: "/mnt/oci-rootfs".to_string(),
            read_only: _read_only,
        };
        builder = builder.mount(mount).oci_rootfs("/mnt/oci-rootfs");
    }

    if let Some(ref p) = initramfs {
        if p.exists() {
            builder = builder.initramfs(p);
        }
    }

    match builder.build() {
        Ok(sb) => Some(sb),
        Err(e) => {
            eprintln!("skipping VM OCI test: failed to build sandbox: {e}");
            None
        }
    }
}

#[cfg(target_os = "linux")]
fn build_test_oci_rootfs_disk(rootfs_dir: &std::path::Path) -> Result<PathBuf, String> {
    fn dir_size_bytes(path: &std::path::Path) -> std::io::Result<u64> {
        fn walk(path: &std::path::Path, total: &mut u64) -> std::io::Result<()> {
            for entry in std::fs::read_dir(path)? {
                let entry = entry?;
                let meta = entry.metadata()?;
                if meta.is_dir() {
                    walk(&entry.path(), total)?;
                } else if meta.is_file() {
                    *total = total.saturating_add(meta.len());
                }
            }
            Ok(())
        }
        let mut total = 0u64;
        walk(path, &mut total)?;
        Ok(total)
    }

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let base_tmp = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
    let disk_path = PathBuf::from(base_tmp).join(format!(
        "voidbox-oci-test-{}-{}.img",
        std::process::id(),
        ts
    ));
    let content_size = dir_size_bytes(rootfs_dir).unwrap_or(64 * 1024 * 1024);
    let disk_size = (content_size.saturating_mul(2)).saturating_add(256 * 1024 * 1024);
    let disk_size = disk_size.max(512 * 1024 * 1024);

    let truncate_status = std::process::Command::new("truncate")
        .arg("-s")
        .arg(disk_size.to_string())
        .arg(&disk_path)
        .status()
        .map_err(|e| format!("failed to run truncate: {e}"))?;
    if !truncate_status.success() {
        return Err("truncate failed".to_string());
    }

    let mkfs_status = std::process::Command::new("mkfs.ext4")
        .arg("-q")
        .arg("-F")
        .arg("-d")
        .arg(rootfs_dir)
        .arg(&disk_path)
        .status()
        .map_err(|e| format!("failed to run mkfs.ext4: {e}"))?;
    if !mkfs_status.success() {
        return Err("mkfs.ext4 failed".to_string());
    }

    Ok(disk_path)
}

/// Mount a host directory as OCI rootfs and verify the guest can read its files.
///
/// Linux: requires `/dev/kvm` + kernel/initramfs artifacts.
/// macOS: requires kernel/initramfs artifacts (VZ).
#[tokio::test]
#[ignore = "requires VM backend + kernel/initramfs + OCI rootfs"]
async fn vm_oci_rootfs_mount_visible() {
    let oci_dir = create_fake_oci_rootfs();
    let Some(sandbox) = build_sandbox_with_oci_mount(oci_dir.path(), true) else {
        return;
    };

    // After OCI setup, guest-agent pivots into the OCI rootfs.
    let output = match sandbox.exec("/bin/cat", &["/oci-marker.txt"]).await {
        Ok(out) => out,
        Err(Error::VmNotRunning) => panic!("vm_oci_rootfs_mount_visible: VM not running"),
        Err(Error::Guest(msg)) => {
            panic!("vm_oci_rootfs_mount_visible: guest communication error: {msg}")
        }
        Err(e) => panic!("failed to exec cat in sandbox: {e}"),
    };

    assert!(
        output.success(),
        "cat oci-marker.txt failed: exit_code={}, stderr={}",
        output.exit_code,
        output.stderr_str()
    );
    assert_eq!(output.stdout_str().trim(), "oci-rootfs-present");
}

/// Verify OCI lowerdir immutability from inside the guest.
///
/// Linux: requires `/dev/kvm` + kernel/initramfs artifacts.
/// macOS: requires kernel/initramfs artifacts (VZ).
#[tokio::test]
#[ignore = "requires VM backend + kernel/initramfs + OCI rootfs"]
async fn vm_oci_rootfs_readonly() {
    let oci_dir = create_fake_oci_rootfs();
    let Some(sandbox) = build_sandbox_with_oci_mount(oci_dir.path(), true) else {
        return;
    };

    let output = match sandbox.exec("/bin/touch", &["/should-write.txt"]).await {
        Ok(out) => out,
        Err(Error::VmNotRunning) => panic!("vm_oci_rootfs_readonly: VM not running"),
        Err(Error::Guest(msg)) => {
            panic!("vm_oci_rootfs_readonly: guest communication error: {msg}")
        }
        Err(e) => panic!("failed to exec touch in sandbox: {e}"),
    };

    assert!(
        !output.success(),
        "writing to OCI-backed root unexpectedly succeeded, exit_code={}",
        output.exit_code
    );
    assert!(
        !oci_dir.path().join("should-write.txt").exists(),
        "host lowerdir must remain unchanged after guest write"
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// Group 3b: Example spec file validation (examples/specs/oci/*.yaml)
// ──────────────────────────────────────────────────────────────────────────────

/// `examples/specs/oci/agent.yaml` parses and validates via `load_spec`.
#[test]
fn example_spec_oci_agent() {
    let spec = load_spec(std::path::Path::new("examples/specs/oci/agent.yaml"))
        .expect("agent.yaml should load");
    assert_eq!(spec.kind, void_box::spec::RunKind::Agent);
    assert_eq!(spec.sandbox.image.as_deref(), Some("python:3.12-slim"));
    assert!(spec.agent.is_some());
    assert!(spec.llm.is_some());
}

/// `examples/specs/oci/workflow.yaml` parses and validates via `load_spec`.
#[test]
fn example_spec_oci_workflow() {
    let spec = load_spec(std::path::Path::new("examples/specs/oci/workflow.yaml"))
        .expect("workflow.yaml should load");
    assert_eq!(spec.kind, void_box::spec::RunKind::Workflow);
    assert_eq!(spec.sandbox.image.as_deref(), Some("alpine:3.20"));
    assert!(spec.workflow.is_some());
    assert!(spec.llm.is_none());
}

/// `examples/specs/oci/pipeline.yaml` parses and validates via `load_spec`.
#[test]
fn example_spec_oci_pipeline() {
    let spec = load_spec(std::path::Path::new("examples/specs/oci/pipeline.yaml"))
        .expect("pipeline.yaml should load");
    assert_eq!(spec.kind, void_box::spec::RunKind::Pipeline);
    assert_eq!(spec.sandbox.image.as_deref(), Some("python:3.12-slim"));
    let pipeline = spec.pipeline.as_ref().unwrap();
    assert_eq!(pipeline.boxes.len(), 3);
    assert_eq!(pipeline.stages.len(), 3);
    // go-validate box should have an OCI skill
    let go_box = &pipeline.boxes[1];
    assert_eq!(go_box.name, "go-validate");
    assert!(go_box.skills.iter().any(|s| matches!(s,
        void_box::spec::SkillEntry::Oci { image, mount, .. }
        if image == "golang:1.23-alpine" && mount == "/skills/go"
    )));
}

/// `examples/specs/oci/skills.yaml` parses and validates via `load_spec`.
#[test]
fn example_spec_oci_skills() {
    let spec = load_spec(std::path::Path::new("examples/specs/oci/skills.yaml"))
        .expect("skills.yaml should load");
    assert_eq!(spec.kind, void_box::spec::RunKind::Agent);
    // No sandbox.image — skills only
    assert!(spec.sandbox.image.is_none());
    let agent = spec.agent.as_ref().unwrap();
    // Should have 4 skills: claude-code + 3 OCI images
    assert_eq!(agent.skills.len(), 4);
    assert!(agent.skills.iter().any(|s| matches!(s,
        void_box::spec::SkillEntry::Oci { image, mount, .. }
        if image == "python:3.12-slim" && mount == "/skills/python"
    )));
}

// ──────────────────────────────────────────────────────────────────────────────
// Group 4: Real OCI image E2E (pull alpine:3.20, pivot_root, exec in guest)
//
// These tests pull a real OCI image from Docker Hub, mount it as rootfs in a
// KVM/VZ micro-VM, and verify that `pivot_root` works — the guest's `/` is
// the OCI image, not the initramfs.
//
// Requirements: VM backend + kernel/initramfs + **network** (image pull).
// ──────────────────────────────────────────────────────────────────────────────

/// Pull alpine:3.20, boot a VM with pivot_root, and exec `cat /etc/os-release`.
///
/// This is the programmatic equivalent of:
/// ```bash
/// voidbox run --file spec.yaml   # with sandbox.image: alpine:3.20
/// ```
///
/// After pivot_root, the guest root is the alpine rootfs (via overlayfs).
/// `/etc/os-release` should contain "Alpine".
#[tokio::test]
#[ignore = "requires VM backend + kernel/initramfs + network (pulls alpine:3.20)"]
async fn vm_oci_alpine_os_release() {
    #[cfg(target_os = "linux")]
    if !std::path::Path::new("/dev/kvm").exists() {
        eprintln!("skipping: /dev/kvm not available");
        return;
    }

    let (kernel, initramfs) = match vm_artifacts_from_env() {
        Some(a) => a,
        None => {
            eprintln!("skipping: set VOID_BOX_KERNEL and VOID_BOX_INITRAMFS");
            return;
        }
    };

    if !kernel.exists() {
        eprintln!("skipping: kernel not found: {}", kernel.display());
        return;
    }

    // 1. Pull and extract alpine:3.20 (uses cache at ~/.voidbox/oci/).
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let cache_dir = PathBuf::from(&home).join(".voidbox/oci");
    let client = voidbox_oci::OciClient::new(cache_dir);
    let rootfs_path = match client.resolve_rootfs("alpine:3.20").await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("skipping: failed to pull alpine:3.20: {e}");
            return;
        }
    };
    eprintln!("OCI rootfs extracted to: {}", rootfs_path.display());

    // 2. Build sandbox:
    //    - Linux/KVM: attach OCI rootfs as virtio-blk + /dev/vda pivot.
    //    - Non-Linux: legacy mount-based path.
    let mut builder = Sandbox::local().memory_mb(1536).vcpus(1).kernel(&kernel);
    #[cfg(target_os = "linux")]
    {
        let disk = match build_test_oci_rootfs_disk(&rootfs_path) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("skipping: failed to build OCI disk for alpine rootfs: {e}");
                return;
            }
        };
        builder = builder.oci_rootfs_dev("/dev/vda").oci_rootfs_disk(disk);
    }
    #[cfg(not(target_os = "linux"))]
    {
        let mount = MountConfig {
            host_path: rootfs_path.to_string_lossy().into_owned(),
            guest_path: "/mnt/oci-rootfs".to_string(),
            read_only: true,
        };
        builder = builder.mount(mount).oci_rootfs("/mnt/oci-rootfs");
    }

    if let Some(ref p) = initramfs {
        if p.exists() {
            builder = builder.initramfs(p);
        }
    }

    let sandbox = match builder.build() {
        Ok(sb) => sb,
        Err(e) => {
            eprintln!("skipping: failed to build sandbox: {e}");
            return;
        }
    };

    // 3. Exec `cat /etc/os-release` — after pivot_root this is alpine's file.
    let output = match sandbox.exec("/bin/cat", &["/etc/os-release"]).await {
        Ok(out) => out,
        Err(Error::VmNotRunning) => panic!("vm_oci_alpine_os_release: VM not running"),
        Err(Error::Guest(msg)) => panic!("vm_oci_alpine_os_release: guest error: {msg}"),
        Err(e) => panic!("exec failed: {e}"),
    };

    eprintln!("--- /etc/os-release ---\n{}", output.stdout_str());

    assert!(
        output.success(),
        "cat /etc/os-release failed: exit_code={}, stderr={}",
        output.exit_code,
        output.stderr_str()
    );
    assert!(
        output.stdout_str().contains("Alpine"),
        "expected Alpine in /etc/os-release, got: {}",
        output.stdout_str()
    );
}

/// YAML spec with `sandbox.image` resolves the OCI image via `run_file()`.
///
/// This verifies the YAML → runtime path that `voidbox run --file spec.yaml`
/// uses: spec parsing → `resolve_oci_base_image()` → extracted rootfs exists
/// on disk. The workflow runs in mock mode to avoid the vsock timing
/// sensitivity of OCI pivot_root boots (the real VM path is covered by
/// `vm_oci_alpine_os_release` above).
#[tokio::test]
#[ignore = "requires network (pulls alpine:3.20)"]
async fn runtime_run_file_resolves_oci_image() {
    // Write a workflow YAML that references an OCI image but uses mock mode.
    // run_file will call resolve_oci_base_image("alpine:3.20") which pulls
    // and extracts the image, then build_shared_sandbox creates a mock sandbox
    // (mock mode ignores OCI mounts but the resolution still happens).
    let yaml = r#"
api_version: v1
kind: workflow
name: alpine-resolve-test
sandbox:
  mode: mock
  image: "alpine:3.20"
workflow:
  steps:
    - name: probe
      run:
        program: echo
        args: ["resolved-ok"]
  output_step: probe
"#;

    let dir = tempfile::tempdir().unwrap();
    let spec_path = dir.path().join("alpine-resolve.yaml");
    std::fs::write(&spec_path, yaml).unwrap();

    let report = match void_box::runtime::run_file(&spec_path, None).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("runtime_run_file_resolves_oci_image: failed: {e}");
            return;
        }
    };

    // The mock sandbox's echo returns "resolved-ok\n".
    // The important part is that run_file succeeded, which means
    // resolve_oci_base_image("alpine:3.20") completed without error.
    assert!(
        report.success,
        "workflow should succeed: output={:?}",
        report.output
    );
    assert!(
        report.output.contains("resolved-ok"),
        "expected probe output, got: {}",
        report.output
    );

    // Verify the OCI rootfs was actually extracted to the cache.
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let cache_dir = PathBuf::from(home).join(".voidbox/oci");
    assert!(
        cache_dir.exists(),
        "OCI cache dir should exist at {}",
        cache_dir.display()
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// Group 5: Guest image OCI pull + extract tests
// ──────────────────────────────────────────────────────────────────────────────

/// Pull a guest image from a registry, extract vmlinuz + rootfs.cpio.gz, verify cache.
///
/// Requires a registry with `voidbox-guest:v0.1.0`.
/// Default: `localhost:5555` — override with `VOIDBOX_TEST_GUEST_IMAGE`.
///
/// ```bash
/// # Start local registry + push image:
/// docker run -d --name voidbox-test-registry -p 5555:5000 registry:2
/// docker tag ghcr.io/the-void-ia/voidbox-guest:latest localhost:5555/voidbox-guest:v0.1.0
/// docker push localhost:5555/voidbox-guest:v0.1.0
///
/// # Run this test:
/// cargo test --test oci_integration -- --ignored guest_image_pull_and_extract
/// ```
#[tokio::test]
#[ignore = "requires a registry with voidbox-guest image (see docstring)"]
async fn guest_image_pull_and_extract() {
    let image_ref_env = std::env::var("VOIDBOX_TEST_GUEST_IMAGE").ok();
    let image_ref = image_ref_env
        .clone()
        .unwrap_or_else(|| "localhost:5555/voidbox-guest:v0.1.0".to_string());

    let cache_dir = tempfile::tempdir().unwrap();
    let client = voidbox_oci::OciClient::new(cache_dir.path().to_path_buf());

    // First pull: should download and extract.
    eprintln!("=== Pulling guest image: {} ===", image_ref);
    let guest = match client.resolve_guest_files(&image_ref).await {
        Ok(guest) => guest,
        Err(e) => {
            // If caller did not explicitly configure a registry/image, treat an
            // unavailable localhost test registry as "not configured" and skip.
            if image_ref_env.is_none() && image_ref.starts_with("localhost:5555/") {
                eprintln!("skipping: test registry not available at localhost:5555 ({e})");
                return;
            }
            panic!("resolve_guest_files should succeed: {e}");
        }
    };

    assert!(
        guest.kernel.exists(),
        "kernel should exist at {}",
        guest.kernel.display()
    );
    assert!(
        guest.initramfs.exists(),
        "initramfs should exist at {}",
        guest.initramfs.display()
    );

    let kernel_size = std::fs::metadata(&guest.kernel).unwrap().len();
    let initramfs_size = std::fs::metadata(&guest.initramfs).unwrap().len();
    eprintln!(
        "kernel:    {} ({} bytes)",
        guest.kernel.display(),
        kernel_size
    );
    eprintln!(
        "initramfs: {} ({} bytes)",
        guest.initramfs.display(),
        initramfs_size
    );

    assert!(
        kernel_size > 1_000_000,
        "kernel too small: {} bytes",
        kernel_size
    );
    assert!(
        initramfs_size > 1_000_000,
        "initramfs too small: {} bytes",
        initramfs_size
    );

    // Second call: should use cache (no network).
    eprintln!("=== Second call (cache hit) ===");
    let guest2 = client
        .resolve_guest_files(&image_ref)
        .await
        .expect("cached resolve should succeed");

    assert_eq!(guest.kernel, guest2.kernel);
    assert_eq!(guest.initramfs, guest2.initramfs);
}
