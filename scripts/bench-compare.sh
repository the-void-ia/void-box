#!/usr/bin/env bash
# bench-compare.sh — compare HEAD bench results against an arbitrary baseline ref.
#
# Harnesses:
#   1. divan microbenches: cargo bench --bench network --features bench-helpers
#   2. VM wall-clock harness: cargo run --release --bin voidbox-network-bench
#
# Output: markdown report to stdout (or --output FILE).
# See AGENTS.md for harness descriptions and JSON field definitions.

set -euo pipefail

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

info() { printf '%s\n' "$*" >&2; }

usage() {
  cat >&2 <<'EOF'
Usage: scripts/bench-compare.sh [OPTIONS]

Compare HEAD bench results against an arbitrary baseline git ref.

Options:
  --baseline <ref>   Git ref (commit SHA, branch, tag) to compare against.
                     Default: merge-base with origin/main.
  --output <file>    Write markdown report to FILE instead of stdout.
  --skip-vm          Skip the voidbox-network-bench VM harness.
  --skip-divan       Skip the cargo bench --bench network divan harness.
  -h, --help         Show this help and exit.
EOF
}

die() { info "ERROR: $*"; exit 1; }

# ---------------------------------------------------------------------------
# Argument parsing
# ---------------------------------------------------------------------------

BASELINE_REF=""
OUTPUT_FILE=""
SKIP_VM=0
SKIP_DIVAN=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --baseline)
      [[ $# -ge 2 ]] || die "--baseline requires an argument"
      BASELINE_REF="$2"; shift 2 ;;
    --output)
      [[ $# -ge 2 ]] || die "--output requires an argument"
      OUTPUT_FILE="$2"; shift 2 ;;
    --skip-vm)
      SKIP_VM=1; shift ;;
    --skip-divan)
      SKIP_DIVAN=1; shift ;;
    -h|--help)
      usage; exit 0 ;;
    *)
      die "Unknown option: $1 (run with --help for usage)" ;;
  esac
done

# ---------------------------------------------------------------------------
# Resolve paths
# ---------------------------------------------------------------------------

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# ---------------------------------------------------------------------------
# Resolve SHAs
# ---------------------------------------------------------------------------

HEAD_SHA="$(git -C "$REPO_ROOT" rev-parse HEAD)"
HEAD_SHORT="${HEAD_SHA:0:9}"
HEAD_BRANCH="$(git -C "$REPO_ROOT" rev-parse --abbrev-ref HEAD 2>/dev/null || echo "detached")"

if [[ -z "$BASELINE_REF" ]]; then
  info "No --baseline given; resolving merge-base with origin/main ..."
  # Fetch is not done automatically — the caller must ensure origin/main is current.
  BASELINE_REF="$(git -C "$REPO_ROOT" merge-base HEAD origin/main)" \
    || die "Could not resolve merge-base with origin/main. Pass --baseline explicitly."
fi

BASELINE_SHA="$(git -C "$REPO_ROOT" rev-parse "${BASELINE_REF}^{commit}")" \
  || die "Cannot resolve baseline ref '${BASELINE_REF}' to a commit SHA"
BASELINE_SHORT="${BASELINE_SHA:0:9}"

info "HEAD:     ${HEAD_SHORT} (${HEAD_BRANCH})"
info "Baseline: ${BASELINE_SHORT} (${BASELINE_REF})"

# ---------------------------------------------------------------------------
# Worktree setup
# ---------------------------------------------------------------------------

WORKTREE_DIR="$(mktemp -d)"
cleanup() {
  git -C "$REPO_ROOT" worktree remove --force "$WORKTREE_DIR" 2>/dev/null || true
  rm -rf "$WORKTREE_DIR"
}
trap cleanup EXIT

info "Setting up worktree at ${WORKTREE_DIR} for ${BASELINE_SHORT} ..."
git -C "$REPO_ROOT" worktree add --detach "$WORKTREE_DIR" "$BASELINE_SHA" \
  || die "Failed to create git worktree at ${WORKTREE_DIR}"

# ---------------------------------------------------------------------------
# Output buffer (built up as a string, flushed at the end)
# ---------------------------------------------------------------------------

REPORT=""

append() { REPORT="${REPORT}${*}"$'\n'; }

append "# Bench comparison"
append ""
append "- HEAD: \`${HEAD_SHORT}\` (\`${HEAD_BRANCH}\`)"
append "- Baseline: \`${BASELINE_SHORT}\` (\`${BASELINE_REF}\`)"
append ""

# ---------------------------------------------------------------------------
# Parse divan output into TSV: name<TAB>median_ns
#
# divan table layout (columns separated by the │ U+2502 box-drawing char):
#   top-level leaf:   field1="<tree><name>  <fastest>", field2=slowest,
#                     field3=median, field4=mean, ...
#   parametric parent: field1="<tree><name>", all other fields empty
#   parametric child: field1="", field2="<tree><name>  <fastest>",
#                     field3=slowest, field4=median, ...
#   MB/s secondary:   field1="", field2=MB/s-fastest, ... (no name — skip)
#
# Strategy: split on │.  The first non-empty field contains the name prefix
# plus the fastest time.  The median is two fields after that.
# ---------------------------------------------------------------------------

parse_divan() {
  local file="$1"
  LC_ALL=en_US.UTF-8 awk -F'│' '
    function unit_ns(val, unit) {
      if (unit == "ns")  return val + 0
      if (unit == "µs")  return val * 1000
      if (unit == "us")  return val * 1000
      if (unit == "ms")  return val * 1000000
      if (unit == "s")   return val * 1000000000
      # Unrecognised unit — treat as µs (safe fallback for future divan changes)
      return val * 1000
    }

    function strip(s,    r) {
      r = s
      gsub(/^[[:space:]╰─├│ ]+/, "", r)
      gsub(/[[:space:]]+$/, "", r)
      return r
    }

    # Extract <number> and <unit> from a string like "330.2 ns" or "50.12 ms".
    # Sets out_val and out_unit.  Returns 1 on success, 0 if no match.
    function extract_time(s, out_val, out_unit,    t, n) {
      t = s
      gsub(/^[[:space:]]+/, "", t)
      # Check for a number followed by a unit
      if (t !~ /^[0-9]/) return 0
      n = split(t, parts, /[[:space:]]+/)
      if (n < 2) return 0
      out_val[1]  = parts[1] + 0
      out_unit[1] = parts[2]
      return 1
    }

    BEGIN { parent = "" }

    # Skip the header line and empty lines
    /^network/ || /^$/ || /^Timer precision/ { next }

    # Skip the MB/s secondary throughput line (no bench name in field 1).
    # Detect: field 1 is empty AND any field contains "MB/s".
    /MB\/s/ && $1 !~ /[[:alpha:]]/ { next }

    {
      # Find the first non-empty field (contains name + fastest time).
      name_field_idx = 0
      name_raw = ""
      for (i = 1; i <= NF; i++) {
        f = $i
        gsub(/^[[:space:]╰─├│ ]+/, "", f)
        gsub(/[[:space:]]+$/, "", f)
        if (f != "") {
          name_field_idx = i
          name_raw = f
          break
        }
      }
      if (name_field_idx == 0) next  # completely empty line

      # The median column is two fields after the name+fastest field.
      median_raw = ""
      if (name_field_idx + 2 <= NF) {
        median_raw = $(name_field_idx + 2)
        gsub(/^[[:space:]│]+/, "", median_raw)
        gsub(/[[:space:]]+$/, "", median_raw)
      }

      # Extract the bench name from the name_raw field.
      # name_raw looks like "dns_cache_hit    220.2 ns" (name + fastest time).
      # Strip the trailing fastest-time portion: everything from the last
      # contiguous digit sequence followed by a unit.
      bench_label = name_raw
      sub(/[[:space:]]+[0-9]+(\.[0-9]+)?[[:space:]]*(ns|us|ms|s|µs)[[:space:]]*$/, "", bench_label)
      # Also strip any residual trailing box-drawing or tree chars
      gsub(/[[:space:]]+$/, "", bench_label)

      # Check whether this row has a median measurement.
      val_arr[1] = ""; unit_arr[1] = ""
      has_median = extract_time(median_raw, val_arr, unit_arr)

      if (!has_median) {
        # This is a parametric parent header row — record as parent.
        parent = bench_label
        next
      }

      # This is a leaf measurement row.
      if (parent != "" && name_field_idx > 1) {
        # Child row: qualify with parent name.
        full_name = parent "/" bench_label
      } else {
        full_name = bench_label
        # Top-level leaf — clear parent so the next top-level bench starts fresh.
        parent = ""
      }

      median_ns = unit_ns(val_arr[1], unit_arr[1])
      print full_name "\t" median_ns
    }
  ' "$file"
}

# ---------------------------------------------------------------------------
# Divan harness
# ---------------------------------------------------------------------------

if [[ "$SKIP_DIVAN" -eq 0 ]]; then
  info "--- divan harness ---"

  # Run divan bench in $1 (cwd), writing TSV-parseable stdout to $2.
  # $3 is a human-readable label used in log lines.
  # Tries --features bench-helpers first; falls back to no features if the
  # feature isn't recognized at that ref.
  run_divan_at() {
    local cwd="$1"
    local out="$2"
    local label="$3"
    local err
    err="$(mktemp)"
    if (cd "$cwd" && cargo bench --bench network --features bench-helpers >"$out" 2>"$err"); then
      rm -f "$err"
      return 0
    fi
    if grep -qiE 'does not have feature|does not contain this feature|unknown feature' "$err"; then
      info "  ${label} lacks bench-helpers feature, retrying without"
      rm -f "$err"
      if (cd "$cwd" && cargo bench --bench network >"$out" 2>/dev/null); then
        return 0
      fi
    fi
    rm -f "$err"
    return 1
  }

  DIVAN_TMP_BASELINE="$(mktemp)"
  DIVAN_TMP_HEAD="$(mktemp)"

  info "Running divan benches on baseline (${BASELINE_SHORT}) ..."
  # cargo's build progress goes to stderr; bench table goes to stdout.
  run_divan_at "$WORKTREE_DIR" "$DIVAN_TMP_BASELINE" "baseline" \
    || info "WARN: divan baseline bench failed; divan section will be incomplete"

  info "Running divan benches on HEAD (${HEAD_SHORT}) ..."
  run_divan_at "$REPO_ROOT" "$DIVAN_TMP_HEAD" "HEAD" \
    || info "WARN: divan HEAD bench failed; divan section will be incomplete"

  DIVAN_BASELINE_TSV="$(parse_divan "$DIVAN_TMP_BASELINE")"
  DIVAN_HEAD_TSV="$(parse_divan "$DIVAN_TMP_HEAD")"
  rm -f "$DIVAN_TMP_BASELINE" "$DIVAN_TMP_HEAD"

  # Build the markdown table via awk: join on bench name, emit rows.
  DIVAN_TABLE="$(
    awk -F'\t' '
      # Load baseline
      NR == FNR {
        if ($1 != "") {
          baseline_ns[$1] = $2
          if (!seen[$1]++) order[++n] = $1
        }
        next
      }
      # Load head
      {
        if ($1 != "") {
          head_ns[$1] = $2
          if (!seen[$1]++) order[++n] = $1
        }
      }
      END {
        for (i = 1; i <= n; i++) {
          name = order[i]
          b = baseline_ns[name]
          h = head_ns[name]

          # Format a nanosecond value into a human-readable string
          # using the shortest unit whose display value is >= 1.
          if (b == "") {
            b_str = "—"
          } else {
            bv = b + 0
            if      (bv >= 1000000000) { b_str = sprintf("%.3g s",  bv/1000000000) }
            else if (bv >= 1000000)    { b_str = sprintf("%.3g ms", bv/1000000) }
            else if (bv >= 1000)       { b_str = sprintf("%.3g µs", bv/1000) }
            else                       { b_str = sprintf("%.3g ns", bv) }
          }

          if (h == "") {
            h_str = "—"
          } else {
            hv = h + 0
            if      (hv >= 1000000000) { h_str = sprintf("%.3g s",  hv/1000000000) }
            else if (hv >= 1000000)    { h_str = sprintf("%.3g ms", hv/1000000) }
            else if (hv >= 1000)       { h_str = sprintf("%.3g µs", hv/1000) }
            else                       { h_str = sprintf("%.3g ns", hv) }
          }

          # Delta
          if (b == "" || h == "") {
            delta_str = "—"
            pct_str = "—"
          } else {
            bv = b + 0; hv = h + 0
            diff = hv - bv
            abs_diff = (diff < 0) ? -diff : diff
            if      (abs_diff >= 1000000000) { unit = "s";  factor = 1000000000 }
            else if (abs_diff >= 1000000)    { unit = "ms"; factor = 1000000 }
            else if (abs_diff >= 1000)       { unit = "µs"; factor = 1000 }
            else                             { unit = "ns"; factor = 1 }
            sign = (diff >= 0) ? "+" : ""
            delta_str = sprintf("%s%.3g %s", sign, diff/factor, unit)

            if (bv != 0) {
              pct = (hv - bv) / bv * 100
              psign = (pct >= 0) ? "+" : ""
              pct_str = sprintf("%s%.1f%%", psign, pct)
            } else {
              pct_str = "—"
            }
          }

          print name "\t" b_str "\t" h_str "\t" delta_str "\t" pct_str
        }
      }
    ' \
    <(printf '%s\n' "$DIVAN_BASELINE_TSV") \
    <(printf '%s\n' "$DIVAN_HEAD_TSV")
  )"

  append "## divan microbenches (\`cargo bench --bench network\`)"
  append ""
  append "| Bench | Baseline | HEAD | Δ | Δ% |"
  append "|-------|---------:|-----:|--:|---:|"

  if [[ -n "$DIVAN_TABLE" ]]; then
    while IFS=$'\t' read -r name b_str h_str delta_str pct_str; do
      append "| ${name} | ${b_str} | ${h_str} | ${delta_str} | ${pct_str} |"
    done <<< "$DIVAN_TABLE"
  else
    append "| *(no data)* | | | | |"
  fi
  append ""
else
  info "Skipping divan harness (--skip-divan)."
fi

# ---------------------------------------------------------------------------
# VM harness
# ---------------------------------------------------------------------------

if [[ "$SKIP_VM" -eq 1 ]]; then
  info "Skipping VM harness (--skip-vm)."
elif [[ -z "${VOID_BOX_KERNEL:-}" ]]; then
  info "Skipping VM harness because VOID_BOX_KERNEL is not set."
elif [[ -z "${VOID_BOX_INITRAMFS:-}" ]]; then
  info "Skipping VM harness because VOID_BOX_INITRAMFS is not set."
else
  info "--- VM harness ---"

  VM_TMP_BASELINE="$(mktemp --suffix=.json)"
  VM_TMP_HEAD="$(mktemp --suffix=.json)"

  info "Running voidbox-network-bench on baseline (${BASELINE_SHORT}) ..."
  (cd "$WORKTREE_DIR" && \
    cargo run --release --bin voidbox-network-bench -- --output "$VM_TMP_BASELINE") \
    || info "WARN: VM baseline bench failed; VM section will be incomplete"

  info "Running voidbox-network-bench on HEAD (${HEAD_SHORT}) ..."
  (cd "$REPO_ROOT" && \
    cargo run --release --bin voidbox-network-bench -- --output "$VM_TMP_HEAD") \
    || info "WARN: VM HEAD bench failed; VM section will be incomplete"

  # JSON field names in display order.
  # These match the Report struct fields in src/bin/voidbox-network-bench/main.rs.
  VM_FIELDS=(
    tcp_bulk_throughput_g2h_mbps
    tcp_throughput_g2h_mbps
    tcp_throughput_h2g_mbps
    tcp_rr_latency_us_p50
    tcp_rr_latency_us_p99
    tcp_crr_latency_us_p50
    udp_dns_qps
    icmp_rr_latency_us_p50
  )

  append "## VM harness (\`voidbox-network-bench\`)"
  append ""
  append "| Metric | Baseline | HEAD | Δ | Δ% |"
  append "|--------|---------:|-----:|--:|---:|"

  for field in "${VM_FIELDS[@]}"; do
    b_val="$(jq -r --arg f "$field" 'if has($f) then .[$f] else null end | if . == null then "null" else tostring end' \
      "$VM_TMP_BASELINE" 2>/dev/null || echo "null")"
    h_val="$(jq -r --arg f "$field" 'if has($f) then .[$f] else null end | if . == null then "null" else tostring end' \
      "$VM_TMP_HEAD"     2>/dev/null || echo "null")"

    if [[ "$b_val" == "null" ]]; then b_str="n/a"; else b_str="$b_val"; fi
    if [[ "$h_val" == "null" ]]; then h_str="n/a"; else h_str="$h_val"; fi

    if [[ "$b_val" == "null" || "$h_val" == "null" ]]; then
      delta_str="—"
      pct_str="—"
    else
      delta_str="$(awk -v b="$b_val" -v h="$h_val" 'BEGIN {
        diff = h - b
        sign = (diff >= 0) ? "+" : ""
        printf "%s%.4g\n", sign, diff
      }')"
      pct_str="$(awk -v b="$b_val" -v h="$h_val" 'BEGIN {
        if (b == 0) { print "—"; exit }
        pct = (h - b) / b * 100
        psign = (pct >= 0) ? "+" : ""
        printf "%s%.1f%%\n", psign, pct
      }')"
    fi

    append "| ${field} | ${b_str} | ${h_str} | ${delta_str} | ${pct_str} |"
  done
  append ""

  rm -f "$VM_TMP_BASELINE" "$VM_TMP_HEAD"
fi

# ---------------------------------------------------------------------------
# Emit report
# ---------------------------------------------------------------------------

if [[ -n "$OUTPUT_FILE" ]]; then
  printf '%s\n' "$REPORT" > "$OUTPUT_FILE"
  info "Report written to ${OUTPUT_FILE}"
else
  printf '%s\n' "$REPORT"
fi
