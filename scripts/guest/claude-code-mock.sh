#!/bin/sh
# Mock claude-code CLI for void-box guest images.
# Used when ANTHROPIC_API_KEY is not set or for testing.
# Subcommands: plan [dir] | apply [dir]
# Apply reads plan from stdin and echoes a summary.

set -e
CMD="${1:-}"
DIR="${2:-.}"

case "$CMD" in
  plan)
    # Emit a fixed mock plan (JSON-like one-liner).
    echo '{"actions":["mock edit"],"summary":"mock plan for '"$DIR"'"}'
    ;;
  apply)
    # Read stdin (plan from previous step), echo applied summary.
    if [ -t 0 ]; then
      echo "Mock apply: no stdin (use pipe from plan)"
      exit 1
    fi
    _count=0
    while read -r _; do _count=$((_count+1)); done
    echo "Mock applied ${_count} plan line(s) in $DIR"
    exit 0
    ;;
  *)
    echo "Usage: claude-code-mock.sh plan [dir] | apply [dir]" >&2
    exit 1
    ;;
esac
