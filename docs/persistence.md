# Persistence Providers

`void-box` now supports a persistence abstraction for daemon state and TUI conversation history.

## Provider Abstraction

Code: `src/persistence.rs`

Trait:

- `load_runs()`
- `save_run()`
- `append_session_message()`
- `load_session_messages()`

Built-in providers:

- `disk` (default): persists to local files under:
  - `~/.local/state/void-box/runs/*.json`
  - `~/.local/state/void-box/sessions/*.jsonl`
- `sqlite` (example adapter): currently delegates to disk provider and shows where a real SQLite backend plugs in.
- `valkey` (example adapter): currently delegates to disk provider and shows where a real Valkey/Redis backend plugs in.

## Selecting a Provider

```bash
# default
VOIDBOX_PERSISTENCE_PROVIDER=disk

# example adapters
VOIDBOX_PERSISTENCE_PROVIDER=sqlite
VOIDBOX_PERSISTENCE_PROVIDER=valkey
```

Optional state directory override:

```bash
VOIDBOX_STATE_DIR=/tmp/void-box-state
```

## Conversation Persistence

Daemon endpoints:

- `POST /v1/sessions/:id/messages`
- `GET /v1/sessions/:id/messages`

The TUI writes user/assistant interactions to these endpoints and can show them with `/history`.
