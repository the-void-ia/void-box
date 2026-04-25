# `RELEASE_DIGESTS.json` — release metadata schema

Each void-box GitHub release uploads a `RELEASE_DIGESTS.json` file alongside
the image and kernel artifacts. It records the verified
version + URL + SHA-256 for every pinned build input shipped with that
release, so a downstream consumer of a given tag has the provenance record
without cloning the repo.

The file is a single source of record. Currently it holds the R-B5c.1
vendored-agent entries; R-B5b.1 will extend it to cover kernels and
initramfs artifacts as that work lands. New top-level sibling keys should
be added without renaming existing ones.

## Schema

```jsonc
{
  // Incremented when a backward-incompatible change is made (e.g. a key is
  // renamed or removed). Consumers SHOULD check `schema` and refuse
  // anything higher than they support.
  "schema": 1,

  // R-B5c.1 — vendored agent binaries. One entry per
  // (agent, platform, arch) tuple; values are copied verbatim from
  // scripts/agents/manifest.toml at release time.
  "agents": {
    "<agent>": {           // e.g. "claude-code", "codex"
      "<platform>": {      // "linux" today; reserve for future "macos" etc.
        "<arch>": {        // "x86_64" or "aarch64"
          "version": "<string>",
          "url": "<string, may contain {version} placeholder>",
          "sha256": "<64-char lowercase hex>"
        }
      }
    }
  }
}
```

### Reserved keys (for R-B5b.1)

R-B5b.1 will extend the schema with sibling top-level keys for the
release-built kernel and initramfs artifacts:

- `"kernels"`    — slim + distro kernel digests
- `"initramfs"`  — per-flavor initramfs digests

The publisher (`.github/workflows/release-images.yml`) will populate
these from the same release-time inputs. Schema-v1 releases omit them;
consumers MUST tolerate their absence.

## Producer

`.github/workflows/release-images.yml` → `publish-release-digests` job.
Runs after all matrix image builds complete, derives the `agents` block
from `scripts/agents/manifest.toml`, and uploads the resulting JSON to the
release with `gh release upload --clobber`.

## Consumer guidance

- Pin by downloading the JSON, not by scraping release-asset sha256 files.
- Treat any `schema` value greater than the version you implemented as a
  hard error.
- Do not infer what a missing top-level key means. `agents` may be empty
  for a hypothetical future release that ships no agent binaries; that is
  not the same as the key being absent.
