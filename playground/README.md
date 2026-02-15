# Playground Stack

This folder contains local observability infrastructure for the playground example.

## Start

```bash
playground/up.sh
```

## Stop

```bash
playground/up.sh --down
```

Services:

- Grafana: `http://localhost:3000`
- OTLP gRPC ingest: `localhost:4317`
- OTLP HTTP ingest: `localhost:4318`

After a run, `playground/up.sh` prints direct Grafana links:
- Traces (Tempo Explore)
- Metrics (Prometheus Explore)

Logs are stored locally at `/tmp/void-box-playground-last.log` by default.
