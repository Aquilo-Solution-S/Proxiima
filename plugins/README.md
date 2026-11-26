# Plugins

Plugins for coding agents that keep their memory in Proxiima: Claude Code,
Codex and Grok read the same folders. The repository root's
`.claude-plugin/marketplace.json` lists them for all three.

| Plugin | What it adds |
|---|---|
| [`proxiima`](proxiima/) | The [`consolidate`](proxiima/skills/consolidate/SKILL.md) skill: remember what the session established, then derive one Abstraction over it |

## Install

Add your Proxiima server to the CLI first. The plugin ships no server
address, because each deployment has its own and Codex does not expand an
environment variable in a plugin's MCP config. Authentication follows
[OAuth discovery](../docs/host-integration.md#oauth-discovery); for a local
server, see [get started](../docs/get-started.md#4-connect-an-agent).

| CLI | Server | Plugin |
|---|---|---|
| Claude Code | `claude mcp add --transport http proxiima <url>` | `claude plugin marketplace add Aquilo-Solution-S/proxiima`, then `claude plugin install proxiima@proxiima` |
| Codex | `codex mcp add proxiima --url <url>` | `codex plugin marketplace add Aquilo-Solution-S/proxiima`, then `codex plugin add proxiima@proxiima` |
| Grok | `grok mcp add --transport http proxiima <url>` | `grok plugin marketplace add Aquilo-Solution-S/proxiima`, then `grok plugin install proxiima --trust` |

Codex asks before each `core_derive`, which the server marks open-world. To
let the skill derive without asking, set `approval_mode = "approve"` under
`[mcp_servers.proxiima.tools.core_derive]` in `~/.codex/config.toml`.

## Tests

[`tests/run.py`](tests/run.py) drives each CLI headless with a cheap model.
Its docstring describes both scenarios:

- `probe` loads [`tests/proxiima-probe`](tests/proxiima-probe/) and reports
  which lifecycle hooks fire and which of their outputs reach the model.
- `smoke` runs the `consolidate` skill against a local server (see
  [get started](../docs/get-started.md)) and checks the database for the
  Abstraction.

```sh
python3 plugins/tests/run.py probe claude
python3 plugins/tests/run.py smoke codex
```

Both switch off your own MCP servers for the run and refuse to start while the
model could still reach one, so a test never writes to a live memory.
