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

  # awk extractor. The manifest schema this parser accepts is intentionally
  # narrow:
  #   - `[a.b.c]` section headers (one per tuple)
  #   - `key = "value"` lines — values MUST use double quotes
  #   - `#` line comments and trailing `# …` comments after a closed value
  # Anything else (single-quoted strings, multi-line arrays, nested tables,
  # `=` inside the value itself) is rejected with a clear error. This keeps
  # the surface small and makes silent mis-parses impossible in practice.
  # Emits `version\nurl\nsha256\n` in a stable order regardless of field
  # order in the file.
  local extracted
  extracted="$(
    awk -v want="$section" -v section="$section" '
      function strip_space(s) {
        sub(/^[[:space:]]+/, "", s); sub(/[[:space:]]+$/, "", s)
        return s
      }
      # Unquote a double-quoted TOML scalar and strip any `# trailing comment`.
      # Rejects single-quoted scalars (the schema mandates double quotes) and
      # anything with an un-matched quote pair.
      function unquote(raw,    s) {
        s = strip_space(raw)
        if (s ~ /^'"'"'/) {
          print "ERROR: single-quoted values are not accepted (use double quotes): " raw > "/dev/stderr"
          bad = 1; return ""
        }
        if (s !~ /^".*"/) {
          print "ERROR: value is not a quoted TOML string: " raw > "/dev/stderr"
          bad = 1; return ""
        }
        # Peel the leading quote, then everything up to the next unescaped
        # quote. Whatever follows is treated as an optional trailing comment
        # and must start with whitespace + `#` or be empty.
        s = substr(s, 2)
        closing = index(s, "\"")
        if (closing == 0) {
          print "ERROR: unterminated quoted value: " raw > "/dev/stderr"
          bad = 1; return ""
        }
        value = substr(s, 1, closing - 1)
        tail = substr(s, closing + 1)
        tail = strip_space(tail)
        if (tail != "" && substr(tail, 1, 1) != "#") {
          print "ERROR: unexpected content after quoted value: " raw > "/dev/stderr"
          bad = 1; return ""
        }
        return value
      }
      /^[[:space:]]*#/ { next }
      /^[[:space:]]*$/ { next }
      /^[[:space:]]*\[/ {
        header = $0
        sub(/^[[:space:]]*\[[[:space:]]*/, "", header)
        sub(/[[:space:]]*\].*$/, "", header)
        in_section = (header == want) ? 1 : 0
        next
      }
      in_section && /=/ {
        key = $0
        sub(/=.*/, "", key); key = strip_space(key)
        val = $0
        sub(/^[^=]*=/, "", val)
        val = unquote(val)
        if (bad) exit 2
        if (key == "version") version = val
        else if (key == "url") url = val
        else if (key == "sha256") sha = val
      }
      END {
        if (bad) exit 2
        if (version == "" || url == "" || sha == "") {
          print "ERROR: section [" section "] missing one of version/url/sha256" > "/dev/stderr"
          exit 1
        }
        print version
        print url
        print sha
      }
    ' "$manifest_file"
  )" || {
    echo "ERROR: manifest entry [$section] missing or unparseable in $manifest_file" >&2
    echo "  Expected keys: version, url, sha256 (all double-quoted)." >&2
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
