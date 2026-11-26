# Plugins

Plugins for coding agents that keep their memory in Proxiima: Claude Code,
Codex and Grok read the same folders. The repository root's
`.claude-plugin/marketplace.json` lists them for all three. Each plugin is one
capability, so each CLI's own plugin enable/disable is the opt-in.

| Plugin | What it adds |
|---|---|
| [`proxiima`](proxiima/) | The [`consolidate`](proxiima/skills/consolidate/SKILL.md) skill: decide what the session established, remember the observations no Fact holds yet, then derive an Abstraction per conclusion |
| [`proxiima-compaction`](proxiima-compaction/) | After the agent's context is compacted, its next tool call carries a reminder to run `consolidate`. Needs `proxiima` and `python3` |

The compaction reminder arrives with a tool result because that is the one
place all three CLIs let a hook speak to the model: Grok's hooks cannot add
context at session start, and no CLI's compaction hooks can. How it works is
in [`compaction.py`](proxiima-compaction/hooks/compaction.py).

## Install

Add your Proxiima server to the CLI first. The plugins ship no server
address, because each deployment has its own and Codex does not expand an
environment variable in a plugin's MCP config. Authentication follows
[OAuth discovery](../docs/host-integration.md#oauth-discovery); for a local
server, see [get started](../docs/get-started.md#4-connect-an-agent).

| CLI | Server | Plugins |
|---|---|---|
| Claude Code | `claude mcp add --transport http proxiima <url>` | `claude plugin marketplace add Aquilo-Solution-S/proxiima`, then `claude plugin install <plugin>@proxiima` |
| Codex | `codex mcp add proxiima --url <url>` | `codex plugin marketplace add Aquilo-Solution-S/proxiima`, then `codex plugin add <plugin>@proxiima`; trust a plugin's hooks in `/hooks` |
| Grok | `grok mcp add --transport http proxiima <url>` | `grok plugin marketplace add Aquilo-Solution-S/proxiima`, then `grok plugin install <plugin> --trust` |

Codex asks before each `core_derive`, which the server marks open-world. To
let the skill derive without asking, set `approval_mode = "approve"` under
`[mcp_servers.proxiima.tools.core_derive]` in `~/.codex/config.toml`.

## Tests

[`tests/run.py`](tests/run.py) drives each CLI headless with a cheap model.
Its docstring describes the scenarios:

- `probe` loads [`tests/proxiima-probe`](tests/proxiima-probe/) and reports
  which lifecycle hooks fire and which of their outputs reach the model.
- `smoke` asks for the `consolidate` skill against a local server (see
  [get started](../docs/get-started.md)) and checks the database for an
  Abstraction over the session's Facts.
- `restraint` checks that consolidating two unrelated Facts derives nothing.
- `compaction` adds `proxiima-compaction`, compacts, and checks that the next
  turn consolidates without being asked.

```sh
python3 plugins/tests/run.py probe claude
python3 plugins/tests/run.py compaction codex
```

Every scenario switches off your own MCP servers for the run and refuses to
start while the model could still reach one, so a test never writes to a live
memory.
