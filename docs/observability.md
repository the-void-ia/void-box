# Observability for Claude-in-void runs

## User-facing summary

Each workflow run returns an **`ObservedResult<WorkflowResult>`**:

- **`result`**: `WorkflowResult` with `output`, `exit_code`, `step_outputs` (per-step stdout/stderr/exit_code), and `duration_ms`.
- **`traces()`**: Spans for the workflow and each step (name, status, duration, attributes such as `stdout_bytes` / `stderr_bytes`).
- **`metrics()`**: `MetricsSnapshot` with step durations (e.g. for dashboards or alerting).
- **`logs()`**: Structured log entries (workflow/step start and finish, errors).

Use this to present a clear picture of each run: success/failure, which step failed, how long each step took, and optional export to OTLP for traces.

## What is captured

- **Per-step spans**: Created by the scheduler for each step. On success, the span records `stdout_bytes`; on failure, `stderr_bytes` and error status. Duration is always recorded and sent to the metrics collector.
- **Workflow span**: Parent of all step spans; total duration.
- **Logs**: Info at workflow start, debug at step start/finish, error when a step fails.
- **Metrics**: Step duration (and any custom counters if added). Use `ObserveConfig::test()` for in-memory capture in tests; use `ObserveConfig::default()` and `.otlp_endpoint(...)` for production trace export.

## Recording the executed command

`SpanGuard::record_exec(program, args)` exists to record the exact command (e.g. `claude-code plan /workspace`) on a step span. The scheduler does not call it because it does not see the program/args inside the step closure. To have the exec command on spans, either:

- Thread the observer into `StepContext` and have `ctx.exec` / `ctx.exec_piped` record the command on the current step span, or
- Have step code set a custom attribute via a future API.

For now, step spans still give you step name, duration, and output sizes for debugging.
