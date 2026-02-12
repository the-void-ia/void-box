# Canonical workflows for Claude-in-void

## Workflow shape: plan → apply → validate → summarize

A typical agent workflow that edits code inside a sandbox:

1. **fetch_context** (optional): gather repo state or constraints.
2. **plan**: run `claude-code plan /workspace`; stdout is the plan.
3. **apply**: run `claude-code apply /workspace` with stdin = plan output from step 2.
4. **run_tests** (optional): e.g. `cargo test` or `npm test` in the workspace.
5. **summarize**: collect outputs and report success/failure.

## Mapping to void-box Workflow API

- Each step is a `Workflow::define(...).step(name, |ctx| async { ... }).pipe(...)`.
- **plan** step: `ctx.exec("claude-code", &["plan", "/workspace"]).await` → returns plan bytes.
- **apply** step: `ctx.exec_piped("claude-code", &["apply", "/workspace"]).await` (stdin = plan from previous step).
- **run_tests**: `ctx.exec("cargo", &["test"]).await` (or equivalent).
- **Error handling**: steps return `Result<Vec<u8>>`; the scheduler records failed steps and can continue or short-circuit depending on configuration. Use `exec_raw` if you need exit codes without failing the step.

## Branching and exploration

- Use `workflow::composition`: `.pipe("a", "b")` for linear flow; multiple steps with different `depends_on` for DAGs.
- To explore alternative paths, define parallel steps or multiple workflows and run them in separate sandboxes (each `Sandbox::local().build()` is an isolated void).

## Example (mock sandbox)

See `examples/claude_workflow.rs`: a single workflow that runs plan → apply using the mock sandbox (simulated claude-code), then prints observability data.
