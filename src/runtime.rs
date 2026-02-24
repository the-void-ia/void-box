use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

use crate::agent_box::VoidBox;
use crate::backend::MountConfig;
use crate::llm::LlmProvider;
use crate::pipeline::Pipeline;
use crate::sandbox::Sandbox;
use crate::skill::{Skill, SkillKind};
use crate::spec::{
    load_spec, BoxSandboxOverride, LlmSpec, MountSpec, PipelineBoxSpec, PipelineStageSpec, RunKind,
    RunSpec, SkillEntry,
};

/// Tracks host directories created for pipeline stage outputs.
/// Maps output_name -> host_path.
type OutputRegistry = HashMap<String, PathBuf>;
use crate::workflow::Workflow;
use crate::workflow::WorkflowExt;
use crate::{Error, Result};

/// Well-known guest path for OCI rootfs mounts.
const OCI_ROOTFS_GUEST_PATH: &str = "/mnt/oci-rootfs";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RunReport {
    pub name: String,
    pub kind: String,
    pub success: bool,
    pub output: String,
    pub stages: usize,
    pub total_cost_usd: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

pub async fn run_file(path: &Path, input: Option<String>) -> Result<RunReport> {
    let mut spec = load_spec(path)?;
    apply_llm_overrides_from_env(&mut spec);
    run_spec(&spec, input).await
}

pub async fn run_spec(spec: &RunSpec, input: Option<String>) -> Result<RunReport> {
    match spec.kind {
        RunKind::Agent => run_agent(spec, input).await,
        RunKind::Pipeline => run_pipeline(spec, input).await,
        RunKind::Workflow => run_workflow(spec, input).await,
    }
}

async fn run_agent(spec: &RunSpec, input: Option<String>) -> Result<RunReport> {
    let agent = spec
        .agent
        .as_ref()
        .ok_or_else(|| Error::Config("missing agent section".into()))?;

    // Resolve guest image (kernel + initramfs) via the 5-step chain.
    let guest = resolve_guest_image(spec).await;

    // Resolve OCI base image before building the sandbox (async).
    let oci_rootfs_host = if let Some(ref image) = spec.sandbox.image {
        eprintln!("[void-box] Resolving OCI base image: {}", image);
        Some(resolve_oci_base_image(image).await?)
    } else {
        None
    };

    let mut builder = VoidBox::new(&spec.name).prompt(&agent.prompt);
    builder = apply_box_sandbox(builder, spec, guest.as_ref());
    builder = apply_box_llm(builder, spec.llm.as_ref());

    // Wire OCI rootfs mount if resolved.
    if let Some(ref rootfs_dir) = oci_rootfs_host {
        builder = apply_oci_rootfs(builder, rootfs_dir);
    }

    for s in &agent.skills {
        builder = builder.skill(parse_skill_entry(s)?);
    }

    if let Some(timeout_secs) = agent.timeout_secs {
        builder = builder.timeout_secs(timeout_secs);
    }

    if let Some(output_file) = &agent.output_file {
        builder = builder.output_file(output_file);
    }

    let ab = builder.build()?;
    let stage = ab.run(input.as_deref().map(str::as_bytes)).await?;

    // Prefer the JSONL result_text, but fall back to file_output when
    // claude-code is killed before emitting the result event.
    let output = if !stage.claude_result.result_text.is_empty() {
        stage.claude_result.result_text.clone()
    } else if let Some(ref data) = stage.file_output {
        String::from_utf8_lossy(data).into_owned()
    } else {
        String::new()
    };

    Ok(RunReport {
        name: spec.name.clone(),
        kind: "agent".to_string(),
        success: !stage.claude_result.is_error,
        output,
        stages: 1,
        total_cost_usd: stage.claude_result.total_cost_usd,
        input_tokens: stage.claude_result.input_tokens,
        output_tokens: stage.claude_result.output_tokens,
    })
}

async fn run_pipeline(spec: &RunSpec, input: Option<String>) -> Result<RunReport> {
    let pipeline = spec
        .pipeline
        .as_ref()
        .ok_or_else(|| Error::Config("missing pipeline section".into()))?;

    // Resolve guest image (kernel + initramfs) via the 5-step chain.
    let guest = resolve_guest_image(spec).await;

    // Resolve OCI base image before building the sandbox (async).
    let oci_rootfs_host = if let Some(ref image) = spec.sandbox.image {
        eprintln!("[void-box] Resolving OCI base image: {}", image);
        Some(resolve_oci_base_image(image).await?)
    } else {
        None
    };

    // Build output registry from all declared outputs across all boxes.
    // We create host dirs eagerly so that input wiring can reference them.
    let mut output_registry: OutputRegistry = HashMap::new();
    for b in &pipeline.boxes {
        for output in &b.outputs {
            let host_dir =
                std::env::temp_dir().join(format!("voidbox-pipeline-{}-{}", b.name, output.name,));
            std::fs::create_dir_all(&host_dir).map_err(|e| {
                crate::Error::Config(format!(
                    "failed to create output dir {}: {}",
                    host_dir.display(),
                    e
                ))
            })?;
            output_registry.insert(output.name.clone(), host_dir);
        }
    }

    let mut boxes_by_name: HashMap<String, VoidBox> = HashMap::new();
    for b in &pipeline.boxes {
        let ab = build_pipeline_box_with_io(spec, b, &output_registry, oci_rootfs_host.as_deref(), guest.as_ref())?;
        boxes_by_name.insert(b.name.clone(), ab);
    }

    let stage_plan = if pipeline.stages.is_empty() {
        pipeline
            .boxes
            .iter()
            .map(|b| PipelineStageSpec::Box {
                name: b.name.clone(),
            })
            .collect::<Vec<_>>()
    } else {
        pipeline.stages.clone()
    };

    let first_name = match &stage_plan[0] {
        PipelineStageSpec::Box { name } => name.clone(),
        PipelineStageSpec::FanOut { .. } => {
            return Err(Error::Config(
                "pipeline first stage cannot be fan_out".to_string(),
            ));
        }
    };

    let first = boxes_by_name
        .remove(&first_name)
        .ok_or_else(|| Error::Config(format!("unknown box '{}'", first_name)))?;

    let mut p = Pipeline::named(&spec.name, first);
    for stage in stage_plan.into_iter().skip(1) {
        match stage {
            PipelineStageSpec::Box { name } => {
                let b = boxes_by_name
                    .remove(&name)
                    .ok_or_else(|| Error::Config(format!("unknown box '{}'", name)))?;
                p = p.pipe(b);
            }
            PipelineStageSpec::FanOut { boxes } => {
                let mut fan = Vec::new();
                for name in boxes {
                    let b = boxes_by_name
                        .remove(&name)
                        .ok_or_else(|| Error::Config(format!("unknown box '{}'", name)))?;
                    fan.push(b);
                }
                p = p.fan_out(fan);
            }
        }
    }

    let result = if let Some(i) = input {
        // lightweight input injection: prefix into first box prompt
        let _ = i;
        p.run().await?
    } else {
        p.run().await?
    };

    let output = result.output.clone();
    let stages = result.stages.len();
    let total_cost_usd = result.total_cost_usd();
    let input_tokens = result.total_input_tokens();
    let output_tokens = result.total_output_tokens();

    Ok(RunReport {
        name: spec.name.clone(),
        kind: "pipeline".to_string(),
        success: result.success(),
        output,
        stages,
        total_cost_usd,
        input_tokens,
        output_tokens,
    })
}

async fn run_workflow(spec: &RunSpec, input: Option<String>) -> Result<RunReport> {
    let w = spec
        .workflow
        .as_ref()
        .ok_or_else(|| Error::Config("missing workflow section".into()))?;

    // Resolve guest image (kernel + initramfs) via the 5-step chain.
    let guest = resolve_guest_image(spec).await;

    // Resolve OCI base image before building the sandbox (async).
    let oci_rootfs_host = if let Some(ref image) = spec.sandbox.image {
        eprintln!("[void-box] Resolving OCI base image: {}", image);
        Some(resolve_oci_base_image(image).await?)
    } else {
        None
    };

    let mut builder = Workflow::define(&spec.name);

    for step in &w.steps {
        let step_name = step.name.clone();
        let program = step.run.program.clone();
        let args = step.run.args.clone();
        let stdin_from = step.run.stdin_from.clone();
        builder = builder.step(&step_name, move |ctx| {
            let program = program.clone();
            let args = args.clone();
            let stdin_from = stdin_from.clone();
            async move {
                let args_ref = args.iter().map(|s| s.as_str()).collect::<Vec<_>>();
                if let Some(src) = stdin_from {
                    if let Some(out) = ctx.output(&src) {
                        return ctx.exec_with_stdin(&program, &args_ref, &out.stdout).await;
                    }
                }

                if let Some(prev) = ctx.prev() {
                    return ctx.exec_with_stdin(&program, &args_ref, prev).await;
                }

                ctx.exec(&program, &args_ref).await
            }
        });
    }

    for step in &w.steps {
        for dep in &step.depends_on {
            builder = builder.pipe(dep, &step.name);
        }
    }

    if let Some(output_step) = &w.output_step {
        builder = builder.output(output_step);
    }

    let workflow = builder.build();

    let sandbox = build_shared_sandbox(spec, oci_rootfs_host.as_deref(), guest.as_ref())?;

    let observed = workflow
        .observe(crate::observe::ObserveConfig::from_env())
        .run_in(sandbox)
        .await?;

    let output = if let Some(i) = input {
        format!("{}\n{}", i, observed.result.output_str())
    } else {
        observed.result.output_str()
    };

    Ok(RunReport {
        name: spec.name.clone(),
        kind: "workflow".to_string(),
        success: observed.result.success(),
        output,
        stages: observed.result.step_outputs.len(),
        total_cost_usd: 0.0,
        input_tokens: 0,
        output_tokens: 0,
    })
}

/// Build a pipeline box with mount-based I/O wiring.
///
/// `output_registry` maps output names from previous stages to host directories.
/// This function:
/// 1. For each `input` declared by the box, mounts the corresponding output host
///    directory read-only at the specified guest path.
/// 2. For each `output` declared by the box, creates a temp directory on the host
///    and mounts it read-write at the specified guest path.
/// 3. Returns the built VoidBox and the newly created output mappings.
fn build_pipeline_box_with_io(
    spec: &RunSpec,
    b: &PipelineBoxSpec,
    output_registry: &OutputRegistry,
    oci_rootfs_host: Option<&Path>,
    guest: Option<&GuestFiles>,
) -> Result<VoidBox> {
    let mut builder = VoidBox::new(&b.name).prompt(&b.prompt);
    builder = apply_box_sandbox(builder, spec, guest);
    builder = apply_box_overrides(builder, b.sandbox.as_ref());
    builder = apply_box_llm(builder, b.llm.as_ref().or(spec.llm.as_ref()));
    for s in &b.skills {
        builder = builder.skill(parse_skill_entry(s)?);
    }
    if let Some(t) = b.timeout_secs {
        builder = builder.timeout_secs(t);
    }

    // Wire inputs from previous stages' outputs
    for input in &b.inputs {
        if let Some(host_dir) = output_registry.get(&input.from) {
            builder = builder.mount(MountConfig {
                host_path: host_dir.to_string_lossy().into_owned(),
                guest_path: input.guest.clone(),
                read_only: true,
            });
            eprintln!(
                "[pipeline:{}] Mounting input '{}' from {} at {}",
                b.name,
                input.from,
                host_dir.display(),
                input.guest,
            );
        } else {
            return Err(crate::Error::Config(format!(
                "box '{}' input '{}' references unknown output (available: {:?})",
                b.name,
                input.from,
                output_registry.keys().collect::<Vec<_>>(),
            )));
        }
    }

    // Wire outputs: create temp dirs on host, mount rw
    for output in &b.outputs {
        let host_dir =
            std::env::temp_dir().join(format!("voidbox-pipeline-{}-{}", b.name, output.name,));
        std::fs::create_dir_all(&host_dir).map_err(|e| {
            crate::Error::Config(format!(
                "failed to create output dir {}: {}",
                host_dir.display(),
                e
            ))
        })?;
        builder = builder.mount(MountConfig {
            host_path: host_dir.to_string_lossy().into_owned(),
            guest_path: output.guest.clone(),
            read_only: false,
        });
        eprintln!(
            "[pipeline:{}] Mounting output '{}' at {} -> {}",
            b.name,
            output.name,
            output.guest,
            host_dir.display(),
        );
    }

    // Wire OCI rootfs mount if resolved.
    if let Some(rootfs_dir) = oci_rootfs_host {
        builder = apply_oci_rootfs(builder, rootfs_dir);
    }

    builder.build()
}

fn build_shared_sandbox(
    spec: &RunSpec,
    oci_rootfs_host: Option<&Path>,
    guest: Option<&GuestFiles>,
) -> Result<std::sync::Arc<Sandbox>> {
    let mode = spec.sandbox.mode.to_ascii_lowercase();
    if mode == "mock" {
        return Sandbox::mock().build();
    }

    let mut builder = Sandbox::local()
        .memory_mb(spec.sandbox.memory_mb)
        .vcpus(spec.sandbox.vcpus)
        .network(spec.sandbox.network);

    if let Some(g) = guest {
        builder = builder.kernel(&g.kernel);
        if let Some(ref i) = g.initramfs {
            builder = builder.initramfs(i);
        }
    }

    for (k, v) in &spec.sandbox.env {
        builder = builder.env(k, v);
    }

    for m in &spec.sandbox.mounts {
        builder = builder.mount(mount_spec_to_config(m));
    }

    // OCI rootfs mount + pivot_root flag
    if let Some(rootfs_dir) = oci_rootfs_host {
        builder = builder
            .mount(MountConfig {
                host_path: rootfs_dir.to_string_lossy().into_owned(),
                guest_path: OCI_ROOTFS_GUEST_PATH.to_string(),
                read_only: true,
            })
            .oci_rootfs(OCI_ROOTFS_GUEST_PATH);
    }

    if mode == "local" && guest.is_none() {
        return Err(Error::Config(
            "sandbox.mode=local requires a kernel (sandbox.kernel, VOID_BOX_KERNEL, or guest_image)".into(),
        ));
    }

    builder.build().or_else(|e| {
        if mode == "auto" {
            eprintln!("[void-box] local sandbox unavailable ({e}); falling back to mock");
            Sandbox::mock().build()
        } else {
            Err(e)
        }
    })
}

fn apply_box_sandbox(
    mut builder: VoidBox,
    spec: &RunSpec,
    guest: Option<&GuestFiles>,
) -> VoidBox {
    let mode = spec.sandbox.mode.to_ascii_lowercase();
    if mode == "mock" {
        return builder.mock();
    }

    builder = builder
        .memory_mb(spec.sandbox.memory_mb)
        .vcpus(spec.sandbox.vcpus)
        .network(spec.sandbox.network);

    if let Some(g) = guest {
        builder = builder.kernel(&g.kernel);
        if let Some(ref i) = g.initramfs {
            builder = builder.initramfs(i);
        }
    }

    for (k, v) in &spec.sandbox.env {
        builder = builder.env(k, resolve_env_value(k, v));
    }

    for m in &spec.sandbox.mounts {
        builder = builder.mount(mount_spec_to_config(m));
    }

    if mode == "auto" && guest.is_none() {
        builder = builder.mock();
    }

    builder
}

/// Pre-resolved guest files (kernel + optional initramfs).
struct GuestFiles {
    kernel: PathBuf,
    initramfs: Option<PathBuf>,
}

/// Resolve guest image files following the 5-step resolution chain:
///
/// 1. `spec.sandbox.kernel` / `spec.sandbox.initramfs` (explicit paths)
/// 2. `$VOID_BOX_KERNEL` / `$VOID_BOX_INITRAMFS` env vars
/// 3. `spec.sandbox.guest_image` OCI ref (explicit)
/// 4. Default: `ghcr.io/the-void-ia/voidbox-guest:v{CARGO_PKG_VERSION}`
/// 5. `None` → mock fallback when `mode: auto`
async fn resolve_guest_image(spec: &RunSpec) -> Option<GuestFiles> {
    // Steps 1-2: local kernel/initramfs paths (sync).
    if let Some(kernel) = resolve_kernel_local(spec) {
        return Some(GuestFiles {
            kernel,
            initramfs: resolve_initramfs_local(spec),
        });
    }

    // Step 3: explicit guest_image in spec.
    // An empty string means "disable auto-pull".
    if let Some(ref guest_image) = spec.sandbox.guest_image {
        if guest_image.is_empty() {
            return None;
        }
        match resolve_oci_guest_image(guest_image).await {
            Ok(files) => return Some(files),
            Err(e) => {
                eprintln!("[void-box] Failed to resolve guest image '{}': {}", guest_image, e);
                if spec.sandbox.mode.eq_ignore_ascii_case("auto") {
                    return None;
                }
                return None;
            }
        }
    }

    // Step 4: default OCI image reference.
    let version = env!("CARGO_PKG_VERSION");
    let default_ref = format!("ghcr.io/the-void-ia/voidbox-guest:v{}", version);
    match resolve_oci_guest_image(&default_ref).await {
        Ok(files) => Some(files),
        Err(e) => {
            eprintln!(
                "[void-box] Failed to resolve default guest image '{}': {}",
                default_ref, e
            );
            // Step 5: None → callers will fall back to mock when mode=auto.
            None
        }
    }
}

/// Pull + extract guest files from an OCI image reference.
async fn resolve_oci_guest_image(image_ref: &str) -> Result<GuestFiles> {
    eprintln!("[void-box] Resolving guest image: {}", image_ref);
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let cache_dir = PathBuf::from(home).join(".voidbox/oci");
    let client = voidbox_oci::OciClient::new(cache_dir);
    let guest = client.resolve_guest_files(image_ref).await.map_err(|e| {
        Error::Config(format!(
            "failed to resolve guest image '{}': {}",
            image_ref, e
        ))
    })?;
    Ok(GuestFiles {
        kernel: guest.kernel,
        initramfs: Some(guest.initramfs),
    })
}

fn resolve_kernel_local(spec: &RunSpec) -> Option<PathBuf> {
    spec.sandbox
        .kernel
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("VOID_BOX_KERNEL").map(PathBuf::from))
        .filter(|p| p.exists())
}

fn resolve_initramfs_local(spec: &RunSpec) -> Option<PathBuf> {
    spec.sandbox
        .initramfs
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("VOID_BOX_INITRAMFS").map(PathBuf::from))
        .filter(|p| p.exists())
}

/// Apply per-box sandbox overrides on top of the base sandbox config.
fn apply_box_overrides(mut builder: VoidBox, overrides: Option<&BoxSandboxOverride>) -> VoidBox {
    let Some(ov) = overrides else {
        return builder;
    };
    if let Some(mb) = ov.memory_mb {
        builder = builder.memory_mb(mb);
    }
    if let Some(v) = ov.vcpus {
        builder = builder.vcpus(v);
    }
    if let Some(net) = ov.network {
        builder = builder.network(net);
    }
    for (k, v) in &ov.env {
        builder = builder.env(k, resolve_env_value(k, v));
    }
    for m in &ov.mounts {
        builder = builder.mount(mount_spec_to_config(m));
    }
    builder
}

/// Resolve an env-var value: if the spec value is empty (`""`), read the
/// actual value from the host process environment. This lets YAML authors
/// write `GITHUB_TOKEN: ""` to mean "inject from host".
fn resolve_env_value(key: &str, spec_value: &str) -> String {
    if spec_value.is_empty() {
        std::env::var(key).unwrap_or_default()
    } else {
        spec_value.to_string()
    }
}

fn apply_box_llm(builder: VoidBox, llm: Option<&LlmSpec>) -> VoidBox {
    let Some(llm) = llm else {
        return builder;
    };

    let provider = match llm.provider.to_ascii_lowercase().as_str() {
        "claude" => LlmProvider::Claude,
        "ollama" => {
            let model = llm.model.clone().unwrap_or_else(|| "qwen3-coder:7b".into());
            if let Some(host) = &llm.base_url {
                LlmProvider::ollama_with_host(model, host)
            } else {
                LlmProvider::ollama(model)
            }
        }
        "custom" => {
            let base_url = llm
                .base_url
                .clone()
                .unwrap_or_else(|| "http://10.0.2.2:11434".into());
            let mut p = LlmProvider::custom(base_url);
            if let Some(model) = &llm.model {
                p = p.model(model);
            }
            if let Some(api_key_env) = &llm.api_key_env {
                if let Ok(k) = std::env::var(api_key_env) {
                    p = p.api_key(k);
                }
            }
            p
        }
        _ => LlmProvider::Claude,
    };

    builder.llm(provider)
}

fn apply_llm_overrides_from_env(spec: &mut RunSpec) {
    let provider = std::env::var("VOIDBOX_LLM_PROVIDER").ok();
    let model = std::env::var("VOIDBOX_LLM_MODEL").ok();
    let base_url = std::env::var("VOIDBOX_LLM_BASE_URL").ok();
    let api_key_env = std::env::var("VOIDBOX_LLM_API_KEY_ENV").ok();

    if provider.is_none() && model.is_none() && base_url.is_none() && api_key_env.is_none() {
        return;
    }

    let mut llm = spec.llm.clone().unwrap_or(LlmSpec {
        provider: "claude".to_string(),
        model: None,
        base_url: None,
        api_key_env: None,
    });

    if let Some(p) = provider {
        llm.provider = p;
    }
    if let Some(m) = model {
        llm.model = Some(m);
    }
    if let Some(u) = base_url {
        llm.base_url = Some(u);
    }
    if let Some(k) = api_key_env {
        llm.api_key_env = Some(k);
    }

    spec.llm = Some(llm);
}

/// Parse a `SkillEntry` (either a simple string or an OCI object) into a `Skill`.
fn parse_skill_entry(entry: &SkillEntry) -> Result<Skill> {
    match entry {
        SkillEntry::Simple(raw) => parse_skill(raw),
        SkillEntry::Oci {
            image,
            mount,
            readonly,
        } => {
            let mut skill = Skill::oci(image, mount);
            if let SkillKind::Oci {
                readonly: ref mut ro,
                ..
            } = skill.kind
            {
                *ro = *readonly;
            }
            Ok(skill)
        }
    }
}

/// Convert a YAML `MountSpec` into a backend `MountConfig`.
fn mount_spec_to_config(spec: &MountSpec) -> MountConfig {
    MountConfig {
        host_path: spec.host.clone(),
        guest_path: spec.guest.clone(),
        read_only: spec.mode != "rw",
    }
}

/// Resolve an OCI base image to a host directory containing the extracted rootfs.
///
/// Uses `~/.voidbox/oci/` as the content-addressed cache directory.
/// Returns the path to the extracted rootfs on the host.
async fn resolve_oci_base_image(image_ref: &str) -> Result<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let cache_dir = PathBuf::from(home).join(".voidbox/oci");
    let client = voidbox_oci::OciClient::new(cache_dir);
    client.resolve_rootfs(image_ref).await.map_err(|e| {
        Error::Config(format!(
            "failed to resolve OCI image '{}': {}",
            image_ref, e
        ))
    })
}

/// Wire an OCI base image into a VoidBox builder: add a read-only mount at
/// `/mnt/oci-rootfs` and set the `oci_rootfs` flag so the guest-agent does
/// `pivot_root` after boot.
fn apply_oci_rootfs(builder: VoidBox, rootfs_host_path: &Path) -> VoidBox {
    builder
        .mount(MountConfig {
            host_path: rootfs_host_path.to_string_lossy().into_owned(),
            guest_path: OCI_ROOTFS_GUEST_PATH.to_string(),
            read_only: true,
        })
        .oci_rootfs(OCI_ROOTFS_GUEST_PATH)
}

fn parse_skill(raw: &str) -> Result<Skill> {
    let parts = raw.splitn(2, ':').collect::<Vec<_>>();
    if parts.len() != 2 {
        return Err(Error::Config(format!(
            "invalid skill '{}', expected '<type>:<value>'",
            raw
        )));
    }

    let skill = match parts[0] {
        "agent" => Skill::agent(parts[1]),
        "file" => Skill::file(parts[1]),
        "remote" => Skill::remote(parts[1]),
        "cli" => Skill::cli(parts[1]),
        "mcp" => Skill::mcp(parts[1]),
        other => {
            return Err(Error::Config(format!(
                "unsupported skill type '{}'; use agent|file|remote|cli|mcp",
                other
            )))
        }
    };

    Ok(skill)
}
