# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

For general development guidelines, architecture, testing, platform differences, and known issues, see @AGENTS.md.

## Claude-specific guidance

- Before implementing non-trivial changes, propose a plan and explain the tradeoffs.
- Prefer LSP operations (`goToDefinition`, `findReferences`, `hover`) over Grep/Glob for Rust code navigation; fall back to Grep/Glob only for comments, config files, and non-Rust files.
- Always consider Linux (KVM) and macOS (VZ) platform parity — consult the platform table in @AGENTS.md before touching backend or guest code.
- Be terse: skip end-of-turn summaries.
