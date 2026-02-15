#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMPOSE_FILE="$ROOT_DIR/playground/docker-compose.yml"

if ! command -v docker >/dev/null 2>&1; then
  echo "[playground] ERROR: docker is required" >&2
  exit 1
fi

if docker compose version >/dev/null 2>&1; then
  COMPOSE_CMD=(docker compose)
elif command -v docker-compose >/dev/null 2>&1; then
  COMPOSE_CMD=(docker-compose)
else
  echo "[playground] ERROR: docker compose is required" >&2
  exit 1
fi

if [[ "${1:-}" == "--down" ]]; then
  "${COMPOSE_CMD[@]}" -f "$COMPOSE_FILE" down
  echo "[playground] stopped"
  exit 0
fi

select_provider() {
  local choice provider

  if [[ -n "${PLAYGROUND_PROVIDER:-}" ]]; then
    provider="$PLAYGROUND_PROVIDER"
  elif [[ -t 0 ]]; then
    echo "[playground] Choose provider:"
    echo "  1) Anthropic API key"
    echo "  2) Ollama"
    echo "  3) Mock"
    read -r -p "Select [1-3] (default 3): " choice
    case "${choice:-3}" in
      1) provider="anthropic" ;;
      2) provider="ollama" ;;
      3) provider="mock" ;;
      *)
        echo "[playground] invalid choice, using mock"
        provider="mock"
        ;;
    esac
  else
    provider="mock"
  fi

  case "$provider" in
    anthropic)
      export PLAYGROUND_PROVIDER="anthropic"
      if [[ -z "${ANTHROPIC_API_KEY:-}" && -t 0 ]]; then
        read -r -s -p "[playground] Enter ANTHROPIC_API_KEY: " ANTHROPIC_API_KEY
        echo
        export ANTHROPIC_API_KEY
      fi
      if [[ -z "${ANTHROPIC_API_KEY:-}" ]]; then
        echo "[playground] WARN: ANTHROPIC_API_KEY is empty; run will still proceed"
      fi
      ;;
    ollama)
      export PLAYGROUND_PROVIDER="ollama"
      if [[ -t 0 ]]; then
        read -r -p "[playground] Ollama model [${OLLAMA_MODEL:-phi4-mini}]: " model
        export OLLAMA_MODEL="${model:-${OLLAMA_MODEL:-phi4-mini}}"
      else
        export OLLAMA_MODEL="${OLLAMA_MODEL:-phi4-mini}"
      fi
      ;;
    *)
      export PLAYGROUND_PROVIDER="mock"
      ;;
  esac

  echo "[playground] provider=$PLAYGROUND_PROVIDER"
}

configure_kvm_artifacts() {
  if [[ ! -e /dev/kvm ]]; then
    echo "[playground] /dev/kvm not available; example will run in mock sandbox mode"
    return
  fi

  export VOID_BOX_KERNEL="${VOID_BOX_KERNEL:-/boot/vmlinuz-$(uname -r)}"
  if [[ ! -f "$VOID_BOX_KERNEL" ]]; then
    echo "[playground] WARN: kernel not found at $VOID_BOX_KERNEL; using mock sandbox mode"
    unset VOID_BOX_KERNEL
    return
  fi

  if [[ "$PLAYGROUND_PROVIDER" == "mock" ]]; then
    export VOID_BOX_INITRAMFS="${VOID_BOX_INITRAMFS:-/tmp/void-box-test-rootfs.cpio.gz}"
    if [[ ! -f "$VOID_BOX_INITRAMFS" ]]; then
      echo "[playground] building test initramfs (claudio mock)..."
      (cd "$ROOT_DIR" && scripts/build_test_image.sh)
    fi
  else
    export VOID_BOX_INITRAMFS="${VOID_BOX_INITRAMFS:-/tmp/void-box-rootfs.cpio.gz}"
    if [[ ! -f "$VOID_BOX_INITRAMFS" ]]; then
      echo "[playground] building runtime initramfs..."
      (cd "$ROOT_DIR" && scripts/build_guest_image.sh)
    fi
  fi

  if [[ -f "${VOID_BOX_INITRAMFS:-}" ]]; then
    echo "[playground] KVM artifacts ready:"
    echo "  VOID_BOX_KERNEL=$VOID_BOX_KERNEL"
    echo "  VOID_BOX_INITRAMFS=$VOID_BOX_INITRAMFS"
  fi
}

print_run_summary() {
  local mode
  if [[ -n "${VOID_BOX_KERNEL:-}" && -n "${VOID_BOX_INITRAMFS:-}" ]]; then
    mode="KVM"
  else
    mode="Mock"
  fi

  echo
  echo "[playground] run summary"
  echo "  provider:        ${PLAYGROUND_PROVIDER}"
  echo "  sandbox mode:    ${mode}"
  echo "  otlp endpoint:   ${VOIDBOX_OTLP_ENDPOINT}"
  echo "  service name:    ${VOIDBOX_SERVICE_NAME}"
  echo "  kernel:          ${VOID_BOX_KERNEL:-<not set>}"
  echo "  initramfs:       ${VOID_BOX_INITRAMFS:-<not set>}"
  if [[ "${PLAYGROUND_PROVIDER}" == "ollama" ]]; then
    echo "  ollama model:    ${OLLAMA_MODEL:-<not set>}"
  fi
  echo
}

echo "[playground] starting LGTM stack..."
"${COMPOSE_CMD[@]}" -f "$COMPOSE_FILE" up -d

echo "[playground] waiting for Grafana health..."
for i in $(seq 1 60); do
  if command -v curl >/dev/null 2>&1; then
    HEALTH_OK=$(curl -fsS "http://localhost:3000/api/health" >/dev/null 2>&1 && echo yes || echo no)
  else
    HEALTH_OK=$(wget -q -O - "http://localhost:3000/api/health" >/dev/null 2>&1 && echo yes || echo no)
  fi
  if [[ "$HEALTH_OK" == "yes" ]]; then
    break
  fi
  sleep 1
  if [[ "$i" == "60" ]]; then
    echo "[playground] ERROR: Grafana did not become healthy in time" >&2
    exit 1
  fi
done

select_provider
configure_kvm_artifacts

export VOIDBOX_OTLP_ENDPOINT="${VOIDBOX_OTLP_ENDPOINT:-http://localhost:4317}"
export VOIDBOX_SERVICE_NAME="${VOIDBOX_SERVICE_NAME:-void-box-playground}"
export PLAYGROUND_GRAFANA_URL="${PLAYGROUND_GRAFANA_URL:-http://localhost:3000}"
export PLAYGROUND_LOG_PATH="${PLAYGROUND_LOG_PATH:-/tmp/void-box-playground-last.log}"
print_run_summary

echo "[playground] running pipeline example..."
(
  cd "$ROOT_DIR"
  cargo run --example playground_pipeline --features opentelemetry 2>&1 | tee "$PLAYGROUND_LOG_PATH"
)

TRACES_URL="$(grep -E '^Traces URL:' "$PLAYGROUND_LOG_PATH" | tail -n1 | sed -E 's/^Traces URL:[[:space:]]*//')"
METRICS_URL="$(grep -E '^Metrics URL:' "$PLAYGROUND_LOG_PATH" | tail -n1 | sed -E 's/^Metrics URL:[[:space:]]*//')"

echo
echo "[playground] wow, it's live"
echo "  Grafana: $PLAYGROUND_GRAFANA_URL"
echo "  Login: admin/admin"
echo "  Service filter: service.name=$VOIDBOX_SERVICE_NAME"
echo "  Provider: $PLAYGROUND_PROVIDER"
echo "  Logs (local): $PLAYGROUND_LOG_PATH"
echo
echo "[playground] direct links"
echo "  Traces: ${TRACES_URL:-<not emitted>}"
echo "  Metrics: ${METRICS_URL:-<not emitted>}"
echo "  Logs: $PLAYGROUND_LOG_PATH"
echo
echo "Tip: run '$0 --down' to stop the stack."
