# Observability

`void-box` captures traces, metrics, and structured logs for workflow runs.

## What You Get Per Run

- `ObservedResult.result`: workflow output, step outputs, exit code, duration
- `ObservedResult.traces()`: workflow + step spans
- `ObservedResult.metrics()`: in-memory metrics snapshot
- `ObservedResult.logs()`: structured run logs

## OTLP Export

When configured, traces and metrics are exported via OTLP.
Structured logs remain local in the current playground flow.

Required:

- build/run with feature flag: `--features opentelemetry`
- set endpoint env var:
  - `VOIDBOX_OTLP_ENDPOINT=http://localhost:4317`
- optional service name:
  - `VOIDBOX_SERVICE_NAME=void-box-playground`

Example:

```bash
VOIDBOX_OTLP_ENDPOINT=http://localhost:4317 \
VOIDBOX_SERVICE_NAME=void-box-playground \
cargo run --example playground_pipeline --features opentelemetry
```

## Fastest Grafana Path

Use the one-command playground script:

```bash
playground/up.sh
```

This will:

1. Start Grafana LGTM via Docker Compose
2. Run `playground_pipeline` with OTLP enabled
3. Print direct Grafana Explore links for traces and metrics
4. Write full run logs to `/tmp/void-box-playground-last.log` (default)

Stop stack:

```bash
playground/up.sh --down
```
