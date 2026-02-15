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
