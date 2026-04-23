#!/usr/bin/env bash
# Agent-binary manifest reader + sha256 verifier (R-B5c.1).
#
# Parser choice: shell + awk. The manifest schema is a closed subset of TOML
# (`[a.b.c]` section headers with `key = "value"` lines, no nested tables, no
# arrays), so a focused awk extractor is sufficient and keeps the build with
# zero runtime dependencies beyond what the existing scripts already require
# (bash, awk, curl, sha256sum/shasum). See docs/agents/claude.md and
# docs/agents/codex.md for the rationale.
#
# Exposed functions (sourced, not executed):
#   agent_manifest_path          -> prints scripts/agents/manifest.toml
#   agent_manifest_require
#     args: agent platform arch
#     -> prints three lines to stdout: version, url, sha256
#     -> nonzero exit and stderr error if the entry is missing or the
#        manifest file is missing/unreadable
#   agent_manifest_sha256 <file> -> prints lowercase hex digest on stdout
#   agent_manifest_verify <file> <expected_sha256>
#     -> returns 0 on match, nonzero with a clear stderr on mismatch

set -u

# Locate the manifest relative to this file. ROOT_DIR is set by the caller.
agent_manifest_path() {
  local here
  here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
  printf '%s\n' "$here/agents/manifest.toml"
}

# Extract the [<agent>.<platform>.<arch>] section and print its
# version/url/sha256 fields, one per line, in that fixed order.
# Fails with a clear error if the file or the entry is missing.
agent_manifest_require() {
  local agent="$1"
  local platform="$2"
  local arch="$3"
  local manifest_file
  manifest_file="$(agent_manifest_path)"
  local section="${agent}.${platform}.${arch}"

  if [[ ! -f "$manifest_file" ]]; then
    echo "ERROR: manifest not found at $manifest_file" >&2
    echo "  R-B5c.1 requires scripts/agents/manifest.toml to be present." >&2
    return 1
  fi
  if [[ ! -r "$manifest_file" ]]; then
    echo "ERROR: manifest not readable at $manifest_file" >&2
    return 1
  fi

  # awk extractor. Finds the exact `[section]` header, then reads key = "value"
  # lines until the next header or EOF. Emits `version\nurl\nsha256\n` in a
  # stable order regardless of field order in the file.
  local extracted
  extracted="$(
    awk -v want="$section" '
      function strip(s) {
        sub(/^[[:space:]]+/, "", s); sub(/[[:space:]]+$/, "", s)
        gsub(/^"|"$/, "", s)
        return s
      }
      /^[[:space:]]*#/ { next }
      /^[[:space:]]*\[/ {
        header = $0
        sub(/^[[:space:]]*\[[[:space:]]*/, "", header)
        sub(/[[:space:]]*\].*$/, "", header)
        in_section = (header == want) ? 1 : 0
        next
      }
      in_section && /=/ {
        key = $0
        sub(/=.*/, "", key); key = strip(key)
        val = $0
        sub(/^[^=]*=/, "", val); val = strip(val)
        if (key == "version") version = val
        else if (key == "url") url = val
        else if (key == "sha256") sha = val
      }
      END {
        if (version == "" || url == "" || sha == "") exit 1
        print version
        print url
        print sha
      }
    ' "$manifest_file"
  )" || {
    echo "ERROR: manifest entry [$section] missing or incomplete in $manifest_file" >&2
    echo "  Expected keys: version, url, sha256." >&2
    return 1
  }

  printf '%s\n' "$extracted"
}

# Print the lowercase-hex SHA-256 of a file, picking whichever tool is
# available on the host (sha256sum on Linux, shasum -a 256 on macOS).
agent_manifest_sha256() {
  local file="$1"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$file" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$file" | awk '{print $1}'
  else
    echo "ERROR: neither sha256sum nor shasum is available" >&2
    return 1
  fi
}

# Compare the SHA-256 of <file> to <expected>. Fails loud on mismatch.
agent_manifest_verify() {
  local file="$1"
  local expected="$2"
  local label="${3:-$(basename "$file")}"
  local observed
  observed="$(agent_manifest_sha256 "$file")" || return 1
  expected="$(printf '%s' "$expected" | tr '[:upper:]' '[:lower:]')"
  observed="$(printf '%s' "$observed" | tr '[:upper:]' '[:lower:]')"
  if [[ "$observed" != "$expected" ]]; then
    echo "ERROR: SHA-256 mismatch for $label" >&2
    echo "  expected: $expected" >&2
    echo "  observed: $observed" >&2
    echo "  file:     $file" >&2
    return 1
  fi
  return 0
}
