# claudio

Configurable mock of the `claude-code` CLI for void-box E2E tests and the playground.
Emits valid `--output-format stream-json` JSONL to stdout so tests don't need an Anthropic API key.

## How it works

1. Parses `-p <prompt>` from the command line (mirrors real `claude-code` invocation)
2. Reads `MOCK_CLAUDE_*` env vars to select a scenario and tune output
3. Discovers provisioned skills (`/home/sandbox/.claude/skills/*.md`) and MCP servers (`/home/sandbox/.claude/mcp.json`)
4. Emits a `system` init event (with skills, MCP servers, tools, traceparent)
5. Emits `assistant` tool-use / `user` tool-result turn pairs
6. Emits a `result` event with cost, token counts, and a summary that includes the prompt, discovered skills, and MCP servers

When MCP servers are discovered, claudio adds a simulated `mcp__<server>` tool call for each one.

## Scenarios

| Scenario     | Tools | Turns | Notes                                    |
| ------------ | ----: | ----: | ---------------------------------------- |
| `simple`     |     1 |     1 | Single `Write` call                      |
| `multi_tool` |     5 |     3 | Read, Write, Bash cycle                  |
| `error`      |     2 |     2 | Bash + Write; result is an error         |
| `heavy`      |    20 |    10 | Read, Write, Bash, Glob, Grep rotation   |
| `custom`     |   n/a |   n/a | Replays a JSONL file verbatim            |

Tool and turn counts above are defaults; `MOCK_CLAUDE_TOOLS` / `MOCK_CLAUDE_TURNS` override them.
Discovered MCP servers add extra tool calls on top of the base count.

## Environment variables

| Variable                    | Default                    | Description                                |
| --------------------------- | -------------------------- | ------------------------------------------ |
| `MOCK_CLAUDE_SCENARIO`      | `simple`                   | Scenario name (see table above)            |
| `MOCK_CLAUDE_TOOLS`         | per scenario               | Override number of tool calls              |
| `MOCK_CLAUDE_TURNS`         | per scenario               | Override number of conversation turns      |
| `MOCK_CLAUDE_INPUT_TOKENS`  | `500`                      | Simulated input token count                |
| `MOCK_CLAUDE_OUTPUT_TOKENS` | `200`                      | Simulated output token count               |
| `MOCK_CLAUDE_COST`          | `0.003`                    | Simulated cost in USD                      |
| `MOCK_CLAUDE_DELAY_MS`      | `0`                        | Delay (ms) between emitted events          |
| `MOCK_CLAUDE_MODEL`         | `claude-sonnet-4-20250514` | Model name in output                       |
| `MOCK_CLAUDE_ERROR`         | (none)                     | If set, emit an error result               |
| `MOCK_CLAUDE_CUSTOM_JSONL`  | (none)                     | Path to a JSONL file to replay verbatim    |
| `TRACEPARENT`               | (none)                     | W3C traceparent; forwarded in system event |

## Build

```bash
cargo build --release -p claudio --target x86_64-unknown-linux-musl
```

`scripts/build_test_image.sh` compiles claudio and installs the binary as
`/usr/local/bin/claude-code` inside the test initramfs so the guest agent
invokes it exactly like the real CLI.

## What it enables testing

- **Skill provisioning** -- skills written to the guest are discovered and reported back
- **MCP integration** -- MCP server configs are parsed and simulated tool calls are generated
- **stream-json parsing** -- consumers get realistic multi-event JSONL streams
- **OTel / trace propagation** -- `TRACEPARENT` flows through the system event
- **Pipeline data flow** -- token counts, cost, turns, and tool use travel end-to-end
