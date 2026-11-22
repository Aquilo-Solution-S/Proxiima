# proxiima-mcp

Canonical Proxiima host: Streamable HTTP MCP server. This is the binary to run locally with a
coding agent and the wiring template for other hosts.

## Run Locally

Follow the [local quick start](../../docs/get-started.md) to start Postgres and
the local issuer, migrate with the platform role, and then start the MCP server.

## Auth

- MCP bearer auth is host/OIDC only. Local means a local issuer, not a bypass.
- MCP initialize: send `X-Proxiima-Owner: personal:<USER_ID>` or another authorized owner key.
- Configure OIDC/host auth per [`docs/configuration.md`](../../docs/configuration.md) and [`docs/operate.md`](../../docs/operate.md).

## Tool Profiles

Default is fail-closed `memory`. Local full surface (including
`core_membership` / `core_transfer`): `PROXIIMA_TOOL_PROFILE=full`. Any other
value, `code` included, is refused at startup.

## Discovery

Clients must read `initialize.instructions`, `tools/list`, resources, `proxiima://how-to`, and `proxiima://tools`.
