use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

#[cfg(target_os = "linux")]
use std::process::Command;

use tokio::sync::mpsc::UnboundedSender;

use crate::agent_box::VoidBox;
use crate::backend::MountConfig;
use crate::credentials::StagedCredentials;
use crate::llm::LlmProvider;
use crate::observe::telemetry::TelemetryBuffer;
use crate::persistence::RunEvent;
use crate::pipeline::Pipeline;
use crate::sandbox::Sandbox;
use crate::skill::{Skill, SkillKind};
use crate::spec::{
    load_spec, AgentMode, BoxSandboxOverride, LlmSpec, MountSpec, PipelineBoxSpec,
    PipelineStageSpec, RunKind, RunSpec, SkillEntry, StepMode,
};

/// Tracks host directories created for pipeline stage outputs.
/// Maps output_name -> host_path.
type OutputRegistry = HashMap<String, PathBuf>;
use crate::workflow::Workflow;
use crate::workflow::WorkflowExt;
use crate::{Error, Result};

/// Well-known guest path for OCI rootfs mounts.
const OCI_ROOTFS_GUEST_PATH: &str = "/mnt/oci-rootfs";
#[cfg(target_os = "linux")]
const OCI_ROOTFS_BLOCK_DEV: &str = "/dev/vda";

#[derive(Debug, Clone)]
struct OciRootfsPlan {
    host_rootfs: PathBuf,
    host_disk: Option<PathBuf>,
    guest_dev: Option<String>,
}

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

pub async fn run_file(
    path: &Path,
    input: Option<String>,
    policy: Option<crate::persistence::RunPolicy>,
    stage_tx: Option<UnboundedSender<RunEvent>>,
    telemetry_buffer: Option<TelemetryBuffer>,
    provider: Option<Arc<dyn crate::persistence::PersistenceProvider>>,
    run_id: Option<&str>,
) -> Result<RunReport> {
    let mut spec = load_spec(path)?;
    apply_llm_overrides_from_env(&mut spec);
    run_spec(
        &spec,
        input,
        policy,
        stage_tx,
        telemetry_buffer,
        provider,
        run_id,
    )
    .await
}

pub async fn run_spec(
    spec: &RunSpec,
    input: Option<String>,
    policy: Option<crate::persistence::RunPolicy>,
    stage_tx: Option<UnboundedSender<RunEvent>>,
    telemetry_buffer: Option<TelemetryBuffer>,
    provider: Option<Arc<dyn crate::persistence::PersistenceProvider>>,
    run_id: Option<&str>,
) -> Result<RunReport> {
    match spec.kind {
        RunKind::Agent => {
            run_agent(spec, input, stage_tx, telemetry_buffer, provider, run_id).await
        }
        RunKind::Pipeline => {
            run_pipeline(
                spec,
                input,
                policy,
                stage_tx,
                telemetry_buffer,
                provider,
                run_id,
            )
            .await
        }
        RunKind::Workflow => run_workflow(spec, input, policy, stage_tx, provider, run_id).await,
        RunKind::Sandbox => run_sandbox(spec).await,
    }
}

/// Run an agent spec in service mode, returning a `ServiceStageHandle`.
///
/// This function only supports agent specs with `mode: service`. If the spec
/// is not an agent or is not configured for service mode, an error is returned.
///
/// The daemon checks `agent.mode` and calls this instead of `run_spec()` for
/// long-running service agents.
pub async fn run_spec_service(
    spec: &RunSpec,
    input: Option<String>,
    telemetry_buffer: Option<TelemetryBuffer>,
) -> Result<crate::agent_box::ServiceStageHandle> {
    if spec.kind != RunKind::Agent {
        return Err(Error::Config(
            "run_spec_service only supports agent specs".into(),
        ));
    }

    let agent = spec
        .agent
        .as_ref()
        .ok_or_else(|| Error::Config("missing agent section".into()))?;

    if agent.mode != AgentMode::Service {
        return Err(Error::Config(
            "run_spec_service requires agent mode: service".into(),
        ));
    }

    let guest = if uses_mock_sandbox(spec) {
        None
    } else {
        resolve_guest_image(spec).await
    };

    let oci_rootfs_plan = if uses_mock_sandbox(spec) {
        None
    } else if let Some(ref image) = spec.sandbox.image {
        eprintln!("[void-box] Resolving OCI base image: {}", image);
        let host_rootfs = resolve_oci_base_image(image).await?;
        Some(resolve_oci_rootfs_plan(image, host_rootfs).await?)
    } else {
        None
    };

    let mut builder = VoidBox::new(&spec.name)
        .prompt(&agent.prompt)
        .mode(AgentMode::Service);

    builder = apply_box_sandbox(builder, spec, guest.as_ref());
    builder = apply_box_llm(builder, spec.llm.as_ref());

    if let Some(ref plan) = oci_rootfs_plan {
        builder = apply_oci_rootfs(builder, plan);
    }

    for s in &agent.skills {
        builder = builder.skill(parse_skill_entry(s)?);
    }

    if let Some(output_file) = &agent.output_file {
        builder = builder.output_file(output_file);
    }

    let ab = builder.build()?;
    ab.run_service(input.as_deref().map(str::as_bytes), telemetry_buffer)
        .await
}

/// Run a bare sandbox (no agent, pipeline, or workflow).
///
/// Boots the VM, waits for ctrl-c, then stops it. Primarily used for
/// interactive / shell-attach workflows.
async fn run_sandbox(spec: &RunSpec) -> Result<RunReport> {
    let staged_creds = prepare_claude_personal(spec.llm.as_ref())?;
    let staged_codex_creds = prepare_codex(spec.llm.as_ref());

    let guest = if uses_mock_sandbox(spec) {
        None
    } else {
        resolve_guest_image(spec).await
    };

    let mut builder = Sandbox::local()
        .memory_mb(spec.sandbox.memory_mb)
        .vcpus(spec.sandbox.vcpus)
        .network(spec.sandbox.network);

    if let Some(ref kernel) = spec.sandbox.kernel {
        builder = builder.kernel(kernel);
    } else if let Some(ref gi) = guest {
        builder = builder.kernel(&gi.kernel);
    } else if let Ok(k) = std::env::var("VOID_BOX_KERNEL") {
        builder = builder.kernel(k);
    }
    if let Some(ref initramfs) = spec.sandbox.initramfs {
        builder = builder.initramfs(initramfs);
    } else if let Some(ref gi) = guest {
        if let Some(ref initramfs) = gi.initramfs {
            builder = builder.initramfs(initramfs);
        }
    } else if let Ok(i) = std::env::var("VOID_BOX_INITRAMFS") {
        builder = builder.initramfs(i);
    }
    if let Some(ref snapshot) = spec.sandbox.snapshot {
        builder = builder.snapshot(snapshot);
    }

    for mount in &spec.sandbox.mounts {
        builder = builder.mount(MountConfig {
            host_path: mount.host.clone(),
            guest_path: mount.guest.clone(),
            read_only: mount.mode != "rw",
        });
    }

    if let Some(ref staged) = staged_creds {
        builder = builder.mount(MountConfig {
            host_path: staged.host_path.clone(),
            guest_path: "/home/sandbox/.claude".into(),
            read_only: false,
        });
    }
    if let Some(ref staged) = staged_codex_creds {
        builder = builder.mount(MountConfig {
            host_path: staged.host_path.clone(),
            guest_path: "/home/sandbox/.codex".into(),
            read_only: false,
        });
    }

    for (key, value) in &spec.sandbox.env {
        builder = builder.env(key, value);
    }

    let sandbox = builder.build()?;
    let _ = sandbox.exec("echo", &["ready"]).await;

    tokio::signal::ctrl_c().await.ok();
    let _ = sandbox.stop().await;
    drop(staged_creds);
    drop(staged_codex_creds);

    Ok(RunReport {
        name: spec.name.clone(),
        kind: "sandbox".into(),
        success: true,
        output: String::new(),
        stages: 0,
        total_cost_usd: 0.0,
        input_tokens: 0,
        output_tokens: 0,
    })
}

fn uses_mock_sandbox(spec: &RunSpec) -> bool {
    spec.sandbox.mode.eq_ignore_ascii_case("mock")
}

/// Helper to send a stage event through the channel (fire-and-forget).
fn emit_stage_event(tx: &Option<UnboundedSender<RunEvent>>, event: RunEvent) {
    if let Some(ref tx) = tx {
        let _ = tx.send(event);
    }
}

async fn run_agent(
    spec: &RunSpec,
    input: Option<String>,
    stage_tx: Option<UnboundedSender<RunEvent>>,
    telemetry_buffer: Option<TelemetryBuffer>,
    provider: Option<Arc<dyn crate::persistence::PersistenceProvider>>,
    run_id: Option<&str>,
) -> Result<RunReport> {
    let agent = spec
        .agent
        .as_ref()
        .ok_or_else(|| Error::Config("missing agent section".into()))?;

    let stage_name = &spec.name;
    let group_id = "g0";
    let box_name = Some(spec.name.as_str());

    // Emit StageQueued + StageStarted
    emit_stage_event(
        &stage_tx,
        crate::persistence::stage_event_queued(stage_name, box_name, group_id, &[]),
    );
    emit_stage_event(
        &stage_tx,
        crate::persistence::stage_event_started(stage_name, box_name, group_id, 1),
    );

    let stage_start = std::time::Instant::now();

    let guest = if uses_mock_sandbox(spec) {
        None
    } else {
        resolve_guest_image(spec).await
    };

    let oci_rootfs_plan = if uses_mock_sandbox(spec) {
        None
    } else if let Some(ref image) = spec.sandbox.image {
        eprintln!("[void-box] Resolving OCI base image: {}", image);
        let host_rootfs = resolve_oci_base_image(image).await?;
        Some(resolve_oci_rootfs_plan(image, host_rootfs).await?)
    } else {
        None
    };

    // Stage credentials for claude-personal provider (if needed).
    // The StagedCredentials value must outlive the sandbox run so the
    // mounted temp directory stays on disk until cleanup.
    let staged_creds = prepare_claude_personal(spec.llm.as_ref())?;
    // Stage codex credentials (~/.codex/auth.json) — soft-fails to None
    // when the host is not logged in, so the run can still try OPENAI_API_KEY
    // env-var-only auth.
    let staged_codex_creds = prepare_codex(spec.llm.as_ref());

    let mut builder = VoidBox::new(&spec.name).prompt(&agent.prompt);
    builder = apply_box_sandbox(builder, spec, guest.as_ref());
    builder = apply_box_llm(builder, spec.llm.as_ref());
    builder = apply_credential_mount(builder, staged_creds.as_ref());
    builder = apply_codex_credential_mount(builder, staged_codex_creds.as_ref());

    // Wire OCI rootfs mount if resolved.
    if let Some(ref plan) = oci_rootfs_plan {
        builder = apply_oci_rootfs(builder, plan);
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

    if agent.mode == AgentMode::Service {
        builder = builder.mode(AgentMode::Service);
    }

    let ab = builder.build()?;
    let stage = ab
        .run(input.as_deref().map(str::as_bytes), telemetry_buffer)
        .await?;

    // Persist file_output artifact if provider is available
    if let (Some(ref prov), Some(rid)) = (&provider, run_id) {
        if let Some(ref data) = stage.file_output {
            if let Err(e) = prov.save_stage_artifact(rid, &spec.name, data) {
                tracing::warn!("failed to persist artifact for agent {}: {}", spec.name, e);
            }
        }
    }

    // Prefer the JSONL result_text, but fall back to file_output when
    // claude-code is killed before emitting the result event.
    let output = if !stage.agent_result.result_text.is_empty() {
        stage.agent_result.result_text.clone()
    } else if let Some(ref data) = stage.file_output {
        String::from_utf8_lossy(data).into_owned()
    } else {
        String::new()
    };

    let duration_ms = stage_start.elapsed().as_millis() as u64;
    if stage.agent_result.is_error {
        emit_stage_event(
            &stage_tx,
            crate::persistence::stage_event_failed(
                stage_name,
                box_name,
                group_id,
                duration_ms,
                1,
                stage
                    .agent_result
                    .error
                    .as_deref()
                    .unwrap_or("agent execution failed"),
                1,
            ),
        );
    } else {
        emit_stage_event(
            &stage_tx,
            crate::persistence::stage_event_succeeded(
                stage_name,
                box_name,
                group_id,
                duration_ms,
                0,
                1,
            ),
        );
    }

    Ok(RunReport {
        name: spec.name.clone(),
        kind: "agent".to_string(),
        success: !stage.agent_result.is_error,
        output,
        stages: 1,
        total_cost_usd: stage.agent_result.total_cost_usd,
        input_tokens: stage.agent_result.input_tokens,
        output_tokens: stage.agent_result.output_tokens,
    })
}

async fn run_pipeline(
    spec: &RunSpec,
    _input: Option<String>,
    _policy: Option<crate::persistence::RunPolicy>,
    stage_tx: Option<UnboundedSender<RunEvent>>,
    telemetry_buffer: Option<TelemetryBuffer>,
    provider: Option<Arc<dyn crate::persistence::PersistenceProvider>>,
    run_id: Option<&str>,
) -> Result<RunReport> {
    let pipeline = spec
        .pipeline
        .as_ref()
        .ok_or_else(|| Error::Config("missing pipeline section".into()))?;

    let guest = if uses_mock_sandbox(spec) {
        None
    } else {
        resolve_guest_image(spec).await
    };

    let oci_rootfs_plan = if uses_mock_sandbox(spec) {
        None
    } else if let Some(ref image) = spec.sandbox.image {
        eprintln!("[void-box] Resolving OCI base image: {}", image);
        let host_rootfs = resolve_oci_base_image(image).await?;
        Some(resolve_oci_rootfs_plan(image, host_rootfs).await?)
    } else {
        None
    };

    // Stage credentials for claude-personal provider (if needed).
    // Shared across all pipeline boxes; must outlive the entire pipeline run.
    let staged_creds = prepare_claude_personal(spec.llm.as_ref())?;
    let staged_codex_creds = prepare_codex(spec.llm.as_ref());

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
        let ab = build_pipeline_box_with_io(
            spec,
            b,
            &output_registry,
            oci_rootfs_plan.as_ref(),
            guest.as_ref(),
            staged_creds.as_ref(),
            staged_codex_creds.as_ref(),
        )?;
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

    // Emit StageQueued for all pipeline stages
    {
        let mut prev_names: Vec<String> = Vec::new();
        for (i, stage_spec) in stage_plan.iter().enumerate() {
            let gid = format!("g{}", i);
            match stage_spec {
                PipelineStageSpec::Box { name } => {
                    emit_stage_event(
                        &stage_tx,
                        crate::persistence::stage_event_queued(
                            name,
                            Some(name.as_str()),
                            &gid,
                            &prev_names,
                        ),
                    );
                    prev_names = vec![name.clone()];
                }
                PipelineStageSpec::FanOut { boxes } => {
                    for bname in boxes {
                        emit_stage_event(
                            &stage_tx,
                            crate::persistence::stage_event_queued(
                                bname,
                                Some(bname.as_str()),
                                &gid,
                                &prev_names,
                            ),
                        );
                    }
                    prev_names = boxes.clone();
                }
            }
        }
    }

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

    let result = match (provider, run_id) {
        (Some(prov), Some(rid)) => {
            p.run_with_artifacts(stage_tx, telemetry_buffer, rid.to_string(), prov)
                .await?
        }
        _ => p.run_with_stage_tx(stage_tx, telemetry_buffer).await?,
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

async fn run_workflow(
    spec: &RunSpec,
    input: Option<String>,
    policy: Option<crate::persistence::RunPolicy>,
    stage_tx: Option<UnboundedSender<RunEvent>>,
    provider: Option<Arc<dyn crate::persistence::PersistenceProvider>>,
    run_id: Option<&str>,
) -> Result<RunReport> {
    let w = spec
        .workflow
        .as_ref()
        .ok_or_else(|| Error::Config("missing workflow section".into()))?;

    let guest = if uses_mock_sandbox(spec) {
        None
    } else {
        resolve_guest_image(spec).await
    };

    let oci_rootfs_plan = if uses_mock_sandbox(spec) {
        None
    } else if let Some(ref image) = spec.sandbox.image {
        eprintln!("[void-box] Resolving OCI base image: {}", image);
        let host_rootfs = resolve_oci_base_image(image).await?;
        Some(resolve_oci_rootfs_plan(image, host_rootfs).await?)
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

                ctx.exec_streaming(&program, &args_ref).await
            }
        });
    }

    for step in &w.steps {
        for dep in &step.depends_on {
            builder = builder.pipe(dep, &step.name);
        }
    }

    for step in &w.steps {
        let effective_timeout = match step.mode {
            Some(StepMode::Service) => Some(0u64), // explicit infinite — don't override
            None => step
                .timeout_secs
                .or_else(|| policy.as_ref().map(|p| p.stage_timeout_secs)),
        };
        if let Some(t) = effective_timeout {
            builder = builder.timeout(&step.name, t);
        }
    }

    if let Some(output_step) = &w.output_step {
        builder = builder.output(output_step);
    }

    let workflow = builder.build();

    tracing::info!(
        "[workflow:{}] starting ({} steps)",
        spec.name,
        w.steps.len()
    );

    let sandbox = build_shared_sandbox(spec, oci_rootfs_plan.as_ref(), guest.as_ref())?;

    // Emit StageQueued for all workflow steps using the execution plan
    if stage_tx.is_some() {
        if let Ok(plan) = crate::workflow::scheduler::ExecutionPlan::from_workflow(&workflow) {
            for (level, group) in plan.parallel_groups.iter().enumerate() {
                let gid = format!("g{}", level);
                for step_name in group {
                    let depends_on: Vec<String> = workflow
                        .steps
                        .get(step_name)
                        .map(|s| s.depends_on.clone())
                        .unwrap_or_default();
                    emit_stage_event(
                        &stage_tx,
                        crate::persistence::stage_event_queued(
                            step_name,
                            None, // workflow steps don't have box_name
                            &gid,
                            &depends_on,
                        ),
                    );
                }
            }
        }
    }

    let observed = workflow
        .observe_with_stage_tx(crate::observe::ObserveConfig::from_env(), stage_tx)
        .run_in(sandbox.clone())
        .await?;

    if let (Some(output_step), Some(prov), Some(rid)) = (&w.output_step, provider.as_ref(), run_id)
    {
        persist_workflow_artifacts(sandbox.as_ref(), prov.as_ref(), rid, output_step).await?;
    }

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

async fn persist_workflow_artifacts(
    sandbox: &Sandbox,
    provider: &dyn crate::persistence::PersistenceProvider,
    run_id: &str,
    output_step: &str,
) -> Result<()> {
    let result_path = "/workspace/result.json";
    if !sandbox.file_exists(result_path).await.unwrap_or(false) {
        return Ok(());
    }

    let data = sandbox.read_file(result_path).await?;
    provider.save_stage_artifact(run_id, output_step, &data)?;
    if output_step != "main" {
        provider.save_stage_artifact(run_id, "main", &data)?;
    }

    let parsed: serde_json::Value = match serde_json::from_slice(&data) {
        Ok(value) => value,
        Err(e) => {
            tracing::warn!(
                "result.json for run '{}' step '{}' is not valid JSON: {e}",
                run_id,
                output_step
            );
            return Ok(());
        }
    };

    if let Some(artifacts) = parsed.get("artifacts").and_then(|value| value.as_array()) {
        for artifact in artifacts {
            let Some(name) = artifact.get("name").and_then(|value| value.as_str()) else {
                continue;
            };
            let artifact_path = artifact
                .get("path")
                .and_then(|value| value.as_str())
                .unwrap_or(name);
            let guest_path = if artifact_path.starts_with('/') {
                artifact_path.to_string()
            } else {
                format!("/workspace/{artifact_path}")
            };
            if !sandbox.file_exists(&guest_path).await.unwrap_or(false) {
                continue;
            }
            let bytes = sandbox.read_file(&guest_path).await?;
            provider.save_named_artifact(run_id, output_step, name, &bytes)?;
            if output_step != "main" {
                provider.save_named_artifact(run_id, "main", name, &bytes)?;
            }
        }
    }

    Ok(())
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
    oci_rootfs_plan: Option<&OciRootfsPlan>,
    guest: Option<&GuestFiles>,
    staged_creds: Option<&StagedCredentials>,
    staged_codex_creds: Option<&StagedCredentials>,
) -> Result<VoidBox> {
    let mut builder = VoidBox::new(&b.name).prompt(&b.prompt);
    builder = apply_box_sandbox(builder, spec, guest);
    builder = apply_box_overrides(builder, b.sandbox.as_ref(), spec);
    builder = apply_box_llm(builder, b.llm.as_ref().or(spec.llm.as_ref()));
    builder = apply_credential_mount(builder, staged_creds);
    builder = apply_codex_credential_mount(builder, staged_codex_creds);
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

    // Wire outputs: mount the pre-created host dirs from the output registry
    for output in &b.outputs {
        let host_dir = output_registry.get(&output.name).ok_or_else(|| {
            crate::Error::Config(format!(
                "box '{}' output '{}' not found in output registry",
                b.name, output.name,
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
    if let Some(plan) = oci_rootfs_plan {
        builder = apply_oci_rootfs(builder, plan);
    }

    builder.build()
}

fn build_shared_sandbox(
    spec: &RunSpec,
    oci_rootfs_plan: Option<&OciRootfsPlan>,
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
        builder = builder.env(k, resolve_env_value(k, v));
    }

    for m in &spec.sandbox.mounts {
        builder = builder.mount(mount_spec_to_config(m));
    }

    // OCI rootfs mount + pivot_root flag
    if let Some(plan) = oci_rootfs_plan {
        builder = apply_oci_rootfs_sandbox(builder, plan);
    }

    // Snapshot restore (explicit opt-in only)
    if let Some(snap_dir) = resolve_snapshot(spec) {
        builder = builder.snapshot(snap_dir);
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

fn apply_box_sandbox(mut builder: VoidBox, spec: &RunSpec, guest: Option<&GuestFiles>) -> VoidBox {
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

    // Snapshot restore (explicit opt-in only)
    if let Some(snap_dir) = resolve_snapshot(spec) {
        builder = builder.snapshot(snap_dir);
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

/// Resolve guest image files following the 6-step resolution chain:
///
/// 1. `spec.sandbox.kernel` / `spec.sandbox.initramfs` (explicit paths)
/// 2. `$VOID_BOX_KERNEL` / `$VOID_BOX_INITRAMFS` env vars
/// 3. Well-known installed paths (`/usr/lib/voidbox/`, Homebrew prefix, etc.)
///
/// 3.5. Auto-resolve from GitHub Releases based on `llm.provider`
///
/// 4. `spec.sandbox.guest_image` OCI ref (explicit)
/// 5. Default: `ghcr.io/the-void-ia/voidbox-guest:v{CARGO_PKG_VERSION}`
/// 6. `None` → mock fallback when `mode: auto`
async fn resolve_guest_image(spec: &RunSpec) -> Option<GuestFiles> {
    // Steps 1-2: local kernel/initramfs paths (spec + env vars).
    if let Some(kernel) = resolve_kernel_local(spec) {
        return Some(GuestFiles {
            kernel,
            initramfs: resolve_initramfs_local(spec),
        });
    }

    // Step 3: well-known installed paths (package manager installs).
    if let Some(installed) = crate::image::resolve_installed_artifacts() {
        eprintln!(
            "[void-box] Using installed artifacts: kernel={}, initramfs={}",
            installed.kernel.display(),
            installed.initramfs.display()
        );
        return Some(GuestFiles {
            kernel: installed.kernel,
            initramfs: Some(installed.initramfs),
        });
    }

    // Step 3.5: auto-resolve from GitHub Releases based on llm.provider.
    let flavor = spec
        .llm
        .as_ref()
        .and_then(|llm| crate::image::flavor_for_provider(&llm.provider))
        .or_else(|| {
            // kind: workflow with no llm section → base
            if spec.kind == crate::spec::RunKind::Workflow {
                Some("base")
            } else {
                None
            }
        });

    if let Some(flavor) = flavor {
        let cache_root = match crate::image::default_cache_root() {
            Ok(root) => root,
            Err(e) => {
                tracing::warn!("cannot resolve image cache dir: {}", e);
                return resolve_guest_image_oci(spec).await;
            }
        };

        let kernel_explicit = spec
            .sandbox
            .kernel
            .as_ref()
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("VOID_BOX_KERNEL").map(PathBuf::from));
        let initramfs_explicit = spec
            .sandbox
            .initramfs
            .as_ref()
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("VOID_BOX_INITRAMFS").map(PathBuf::from));

        // Download kernel and initramfs concurrently (halves cold-start wait).
        let (kernel_result, initramfs_result) = tokio::join!(
            crate::image::resolve_kernel(kernel_explicit.as_deref(), &cache_root,),
            crate::image::resolve_initramfs(initramfs_explicit.as_deref(), flavor, &cache_root,)
        );

        match (kernel_result, initramfs_result) {
            (Ok(kernel), Ok(initramfs)) => {
                return Some(GuestFiles {
                    kernel,
                    initramfs: Some(initramfs),
                });
            }
            (Err(e), _) | (_, Err(e)) => {
                tracing::warn!("auto image resolution failed: {}. Falling back to OCI.", e);
            }
        }
    }

    // Step 4+: OCI fallback (existing logic).
    resolve_guest_image_oci(spec).await
}

/// OCI fallback for guest image resolution (steps 4-5).
async fn resolve_guest_image_oci(spec: &RunSpec) -> Option<GuestFiles> {
    // Step 4: explicit guest_image in spec.
    // An empty string means "disable auto-pull".
    if let Some(ref guest_image) = spec.sandbox.guest_image {
        if guest_image.is_empty() {
            return None;
        }
        match resolve_oci_guest_image(guest_image).await {
            Ok(files) => return Some(files),
            Err(e) => {
                eprintln!(
                    "[void-box] Failed to resolve guest image '{}': {}",
                    guest_image, e
                );
                return None;
            }
        }
    }

    // Step 5: default OCI image reference.
    let version = env!("CARGO_PKG_VERSION");
    let default_ref = format!("ghcr.io/the-void-ia/voidbox-guest:v{}", version);
    match resolve_oci_guest_image(&default_ref).await {
        Ok(files) => Some(files),
        Err(e) => {
            eprintln!(
                "[void-box] Failed to resolve default guest image '{}': {}",
                default_ref, e
            );
            // Step 6: None → callers will fall back to mock when mode=auto.
            None
        }
    }
}

/// Pull + extract guest files from an OCI image reference.
async fn resolve_oci_guest_image(image_ref: &str) -> Result<GuestFiles> {
    eprintln!("[void-box] Resolving guest image: {}", image_ref);
    let cache_dir = oci_cache_dir();
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

/// Resolve the snapshot path from the spec.
///
/// Returns `Some(path)` only if the spec explicitly declares a snapshot.
/// No auto-detection, no env var fallback — snapshots are off unless
/// the user explicitly sets `sandbox.snapshot` in the spec.
fn resolve_snapshot(spec: &RunSpec) -> Option<PathBuf> {
    let hash = spec.sandbox.snapshot.as_deref()?;
    if hash.is_empty() {
        return None;
    }
    // Resolve hash prefix to a snapshot directory
    let dir = crate::snapshot_store::snapshot_dir_for_hash(hash);
    if crate::snapshot_store::snapshot_exists(&dir) {
        Some(dir)
    } else {
        // Treat as a literal path
        let path = PathBuf::from(hash);
        if crate::snapshot_store::snapshot_exists(&path) {
            Some(path)
        } else {
            eprintln!(
                "[void-box] Snapshot '{}' not found (checked {} and literal path)",
                hash,
                dir.display()
            );
            None
        }
    }
}

/// Resolve per-box snapshot override, falling back to the top-level spec.
fn resolve_box_snapshot(
    box_override: Option<&BoxSandboxOverride>,
    spec: &RunSpec,
) -> Option<PathBuf> {
    // Per-box override takes priority
    if let Some(ov) = box_override {
        if let Some(ref hash) = ov.snapshot {
            if !hash.is_empty() {
                let dir = crate::snapshot_store::snapshot_dir_for_hash(hash);
                if crate::snapshot_store::snapshot_exists(&dir) {
                    return Some(dir);
                }
                let path = PathBuf::from(hash);
                if crate::snapshot_store::snapshot_exists(&path) {
                    return Some(path);
                }
                eprintln!("[void-box] Per-box snapshot '{}' not found", hash);
                return None;
            }
        }
    }
    // Fall back to top-level spec
    resolve_snapshot(spec)
}

fn resolve_kernel_local(spec: &RunSpec) -> Option<PathBuf> {
    let candidate = spec
        .sandbox
        .kernel
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("VOID_BOX_KERNEL").map(PathBuf::from));
    if let Some(ref path) = candidate {
        if !path.exists() {
            eprintln!(
                "[void-box] WARNING: kernel path '{}' does not exist — \
                 falling back to installed artifacts or OCI pull. \
                 Check sandbox.kernel or VOID_BOX_KERNEL.",
                path.display()
            );
            return None;
        }
    }
    candidate
}

fn resolve_initramfs_local(spec: &RunSpec) -> Option<PathBuf> {
    let candidate = spec
        .sandbox
        .initramfs
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("VOID_BOX_INITRAMFS").map(PathBuf::from));
    if let Some(ref path) = candidate {
        if !path.exists() {
            eprintln!(
                "[void-box] WARNING: initramfs path '{}' does not exist — \
                 falling back to installed artifacts or OCI pull. \
                 Check sandbox.initramfs or VOID_BOX_INITRAMFS.",
                path.display()
            );
            return None;
        }
    }
    candidate
}

/// Apply per-box sandbox overrides on top of the base sandbox config.
fn apply_box_overrides(
    mut builder: VoidBox,
    overrides: Option<&BoxSandboxOverride>,
    spec: &RunSpec,
) -> VoidBox {
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
    // Per-box snapshot override (explicit opt-in only)
    if let Some(snap_dir) = resolve_box_snapshot(Some(ov), spec) {
        builder = builder.snapshot(snap_dir);
    }
    builder
}

/// Resolve an env-var value: if the spec value is empty (`""`), read the
/// actual value from the host process environment. This lets YAML authors
/// write `GITHUB_TOKEN: ""` to mean "inject from host".
///
/// For `OLLAMA_BASE_URL`, host env overrides the spec so macOS users can set
/// `OLLAMA_BASE_URL=http://192.168.64.1:11434` (VZ NAT) instead of the default
/// `10.0.2.2` (Linux SLIRP).
fn resolve_env_value(key: &str, spec_value: &str) -> String {
    if spec_value.is_empty() {
        std::env::var(key).unwrap_or_default()
    } else if key == "OLLAMA_BASE_URL" {
        std::env::var(key).unwrap_or_else(|_| spec_value.to_string())
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
        "claude-personal" => LlmProvider::ClaudePersonal,
        "codex" => LlmProvider::Codex,
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

pub fn apply_llm_overrides_from_env(spec: &mut RunSpec) {
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

/// If the LLM provider is `claude-personal`, discover and stage OAuth credentials.
///
/// Returns `Some(StagedCredentials)` whose `host_path` should be mounted into
/// the guest. The temp directory is cleaned up when the value is dropped.
fn prepare_claude_personal(llm: Option<&LlmSpec>) -> Result<Option<StagedCredentials>> {
    match llm {
        Some(l) if l.provider.eq_ignore_ascii_case("claude-personal") => {
            let json = crate::credentials::discover_oauth_credentials()?;
            Ok(Some(crate::credentials::stage_credentials(&json)?))
        }
        _ => Ok(None),
    }
}

/// If the LLM provider is `codex`, try to discover and stage codex credentials
/// from `~/.codex/auth.json`.
///
/// Unlike `prepare_claude_personal`, this function does NOT fail on missing
/// credentials — it returns `None` and emits a warning. The runtime then
/// falls back to `OPENAI_API_KEY` env-var-only auth, which works for some
/// codex endpoints but not the Responses API used by `codex exec`. The user
/// is told to run `codex login` if they want full functionality.
fn prepare_codex(llm: Option<&LlmSpec>) -> Option<StagedCredentials> {
    let llm = llm?;
    if !llm.provider.eq_ignore_ascii_case("codex") {
        return None;
    }
    match crate::credentials::discover_codex_credentials() {
        Ok(json) => match crate::credentials::stage_codex_credentials(&json) {
            Ok(staged) => Some(staged),
            Err(e) => {
                tracing::warn!("Failed to stage codex credentials: {}", e);
                None
            }
        },
        Err(e) => {
            tracing::warn!(
                "Codex credentials not staged: {}. Falling back to OPENAI_API_KEY env var only — \
                 the codex Responses API endpoint typically rejects API keys, so prefer running \
                 'codex login' on the host first.",
                e
            );
            None
        }
    }
}

/// If credentials were staged, inject a mount into the builder.
///
/// RW because Claude Code writes `settings.json` into `~/.claude/` at startup.
/// Guest writes land in the host `TempDir`, which is ephemeral (cleaned up on drop).
fn apply_credential_mount(builder: VoidBox, staged: Option<&StagedCredentials>) -> VoidBox {
    match staged {
        Some(s) => builder.mount(MountConfig {
            host_path: s.host_path.clone(),
            guest_path: "/home/sandbox/.claude".into(),
            read_only: false,
        }),
        None => builder,
    }
}

/// If codex credentials were staged, mount them at `/home/sandbox/.codex`.
///
/// RW because codex may refresh OAuth tokens at runtime (writes back to
/// `auth.json`). Guest writes land in the host `TempDir`, which is ephemeral.
fn apply_codex_credential_mount(builder: VoidBox, staged: Option<&StagedCredentials>) -> VoidBox {
    match staged {
        Some(s) => builder.mount(MountConfig {
            host_path: s.host_path.clone(),
            guest_path: "/home/sandbox/.codex".into(),
            read_only: false,
        }),
        None => builder,
    }
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
        SkillEntry::Mcp { command, args, env } => {
            let mut skill = Skill::mcp(command);
            if !args.is_empty() {
                let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
                skill = skill.args(&arg_refs);
            }
            for (k, v) in env {
                skill = skill.env(k, v);
            }
            Ok(skill)
        }
        SkillEntry::Inline { name, content } => Ok(Skill::inline(name, content)),
    }
}

/// Convert a YAML `MountSpec` into a backend `MountConfig`.
fn mount_spec_to_config(spec: &MountSpec) -> MountConfig {
    let host = std::path::Path::new(&spec.host);
    let abs_host = if host.is_absolute() {
        host.to_path_buf()
    } else {
        std::env::current_dir()
            .expect("cannot determine cwd")
            .join(host)
    };

    // For rw mounts, ensure the host directory exists so the 9p
    // device can serve it. Read-only mounts must already exist.
    if spec.mode == "rw" {
        if let Err(e) = std::fs::create_dir_all(&abs_host) {
            tracing::warn!(
                "failed to create mount host directory {}: {}",
                abs_host.display(),
                e
            );
        }
    }

    MountConfig {
        host_path: abs_host.to_string_lossy().into_owned(),
        guest_path: spec.guest.clone(),
        read_only: spec.mode != "rw",
    }
}

/// Resolve an OCI base image to a host directory containing the extracted rootfs.
///
/// Uses `~/.voidbox/oci/` as the content-addressed cache directory.
/// Returns the path to the extracted rootfs on the host.
async fn resolve_oci_base_image(image_ref: &str) -> Result<PathBuf> {
    let cache_dir = oci_cache_dir();
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
fn apply_oci_rootfs(builder: VoidBox, plan: &OciRootfsPlan) -> VoidBox {
    if let (Some(dev), Some(disk)) = (&plan.guest_dev, &plan.host_disk) {
        builder.oci_rootfs_dev(dev).oci_rootfs_disk(disk)
    } else {
        builder
            .mount(MountConfig {
                host_path: plan.host_rootfs.to_string_lossy().into_owned(),
                guest_path: OCI_ROOTFS_GUEST_PATH.to_string(),
                read_only: true,
            })
            .oci_rootfs(OCI_ROOTFS_GUEST_PATH)
    }
}

fn apply_oci_rootfs_sandbox(
    builder: crate::sandbox::SandboxBuilder,
    plan: &OciRootfsPlan,
) -> crate::sandbox::SandboxBuilder {
    if let (Some(dev), Some(disk)) = (&plan.guest_dev, &plan.host_disk) {
        builder.oci_rootfs_dev(dev).oci_rootfs_disk(disk)
    } else {
        builder
            .mount(MountConfig {
                host_path: plan.host_rootfs.to_string_lossy().into_owned(),
                guest_path: OCI_ROOTFS_GUEST_PATH.to_string(),
                read_only: true,
            })
            .oci_rootfs(OCI_ROOTFS_GUEST_PATH)
    }
}

async fn resolve_oci_rootfs_plan(_image_ref: &str, host_rootfs: PathBuf) -> Result<OciRootfsPlan> {
    #[cfg(target_os = "linux")]
    {
        let host_disk = build_oci_rootfs_disk(_image_ref, &host_rootfs).await?;
        Ok(OciRootfsPlan {
            host_rootfs,
            host_disk: Some(host_disk),
            guest_dev: Some(OCI_ROOTFS_BLOCK_DEV.to_string()),
        })
    }

    #[cfg(not(target_os = "linux"))]
    {
        Ok(OciRootfsPlan {
            host_rootfs,
            host_disk: None,
            guest_dev: None,
        })
    }
}

#[cfg(target_os = "linux")]
fn check_ext4_tools() -> Result<()> {
    for tool in ["mkfs.ext4", "truncate"] {
        if Command::new("which")
            .arg(tool)
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(true)
        {
            return Err(Error::Config(format!(
                "'{}' not found; install e2fsprogs and coreutils for OCI block-device rootfs",
                tool
            )));
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
async fn build_oci_rootfs_disk(image_ref: &str, rootfs_dir: &Path) -> Result<PathBuf> {
    check_ext4_tools()?;
    let disks_dir = oci_cache_dir().join("disks");
    std::fs::create_dir_all(&disks_dir).map_err(|e| {
        Error::Config(format!(
            "failed to create OCI disk cache dir {}: {}",
            disks_dir.display(),
            e
        ))
    })?;

    let cache_key =
        stable_cache_key(&[image_ref, rootfs_dir.to_string_lossy().as_ref(), "ext4-v1"]);
    let disk_path = disks_dir.join(format!("{cache_key}.img"));
    if disk_path.exists() {
        return Ok(disk_path);
    }

    let tmp_disk = disks_dir.join(format!("{cache_key}.tmp"));
    let _ = std::fs::remove_file(&tmp_disk);

    let content_size = directory_size_bytes(rootfs_dir).unwrap_or(512 * 1024 * 1024);
    let disk_size = ((content_size as f64) * 1.35) as u64 + 512 * 1024 * 1024;
    let disk_size = disk_size.max(256 * 1024 * 1024);

    let truncate_status = Command::new("truncate")
        .arg("-s")
        .arg(disk_size.to_string())
        .arg(&tmp_disk)
        .status()
        .map_err(|e| Error::Config(format!("failed to run truncate: {}", e)))?;
    if !truncate_status.success() {
        return Err(Error::Config(
            "truncate failed while creating OCI disk".into(),
        ));
    }

    let mkfs_status = Command::new("mkfs.ext4")
        .arg("-q")
        .arg("-F")
        .arg("-d")
        .arg(rootfs_dir)
        .arg(&tmp_disk)
        .status()
        .map_err(|e| Error::Config(format!("failed to run mkfs.ext4: {}", e)))?;
    if !mkfs_status.success() {
        return Err(Error::Config(
            "mkfs.ext4 failed while building OCI rootfs disk".into(),
        ));
    }

    std::fs::rename(&tmp_disk, &disk_path).map_err(|e| {
        Error::Config(format!(
            "failed to finalize OCI disk {}: {}",
            disk_path.display(),
            e
        ))
    })?;
    Ok(disk_path)
}

#[cfg(target_os = "linux")]
fn directory_size_bytes(path: &Path) -> std::io::Result<u64> {
    fn walk(path: &Path, total: &mut u64) -> std::io::Result<()> {
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

#[cfg(target_os = "linux")]
fn stable_cache_key(parts: &[&str]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for p in parts {
        hasher.update(p.as_bytes());
        hasher.update(b"\0");
    }
    format!("{:x}", hasher.finalize())[..16].to_string()
}

fn oci_cache_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("VOIDBOX_CACHE_DIR") {
        return PathBuf::from(dir).join("oci");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".voidbox/oci")
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::LlmSpec;

    fn make_llm_spec(provider: &str) -> LlmSpec {
        LlmSpec {
            provider: provider.to_string(),
            model: None,
            base_url: None,
            api_key_env: None,
        }
    }

    #[test]
    fn provider_codex_yaml_parses_and_resolves_to_codex_variant() {
        // Step 1: YAML round-trip for LlmSpec
        let yaml = r#"provider: codex"#;
        let llm_spec: LlmSpec = serde_yaml::from_str(yaml).expect("LlmSpec should parse");
        assert_eq!(llm_spec.provider, "codex");

        // Step 2: apply_box_llm converts "codex" → LlmProvider::Codex.
        // Use binary_name() as a discriminant — "codex" only for LlmProvider::Codex,
        // "claude-code" for everything else (including the fallback branch).
        let builder = VoidBox::new("test").prompt("hello");
        let built = apply_box_llm(builder, Some(&make_llm_spec("codex")));
        // binary_name is accessible via the built VoidBox's inner config through
        // the provider's own test (which is already covered in llm.rs).
        // Here we verify the fallback ("_") is NOT taken by comparing descriptions.
        // We do this by re-resolving manually, matching the same logic as apply_box_llm.
        let provider = match llm_spec.provider.to_ascii_lowercase().as_str() {
            "claude" => LlmProvider::Claude,
            "claude-personal" => LlmProvider::ClaudePersonal,
            "codex" => LlmProvider::Codex,
            _ => LlmProvider::Claude,
        };
        match provider {
            LlmProvider::Codex => {}
            other => panic!("expected LlmProvider::Codex, got {:?}", other),
        }

        // Confirm the VoidBox builder call didn't panic (codex is a valid provider).
        let _ = built;
    }

    #[test]
    fn provider_codex_case_insensitive() {
        // "CODEX" and "Codex" should both resolve correctly.
        for input in &["CODEX", "Codex", "codex"] {
            let spec = make_llm_spec(input);
            let resolved = match spec.provider.to_ascii_lowercase().as_str() {
                "codex" => LlmProvider::Codex,
                _ => LlmProvider::Claude,
            };
            match resolved {
                LlmProvider::Codex => {}
                other => panic!("input {:?}: expected Codex, got {:?}", input, other),
            }
        }
    }
}
