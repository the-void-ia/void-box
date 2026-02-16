# VoidBox Event Model (Skill + Environment)

The event stream is designed so every action can be answered as:

- Which `run`?
- Which `box` (if any)?
- Which `skill`?
- In which `environment`?
- What happened (`event_type`)?

## Core Fields

All events include:

- `ts_ms`
- `level`
- `event_type`
- `message`
- `run_id`

Optional identity/correlation fields:

- `box_name`
- `skill_id`
- `skill_type`
- `environment_id`
- `mode` (`mock|local|auto`)
- `stream` (`stdout|stderr`)
- `seq`
- `payload`

## Event Types

Lifecycle:

- `run.started`
- `run.spec.loaded`
- `run.finished`
- `run.failed`
- `run.cancelled`

Environment:

- `env.provisioned`

Box / workflow planning:

- `box.started`
- `workflow.planned`

Skills:

- `skill.mounted`

Streams/logs:

- `log.chunk`
- `log.closed`

## TUI Mapping

A Claude-like TUI should render this stream in a timeline:

- `skill.mounted` -> capability setup
- `env.provisioned` -> sandbox context
- `log.chunk` -> live execution detail
- `run.finished|run.failed` -> terminal status

This keeps `voidbox = skill + environment` visible in every run.
