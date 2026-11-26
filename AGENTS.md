# AGENTS.md

Read [README](README.md) first. The [guide](docs/index.md) owns
concepts and integration; avoid a second invariant or configuration ledger.

## Scope and authority

- The ledger in `lean/Proxiima/` owns the rules it declares. The proof library
  in `lean/Causa/` holds the graph proofs and remaining principles. Link the
  full declaration name where code holds a rule. Run `lake build` and
  `lake exe ledger check` in `lean/` to check the ledger's consistency and
  resolve its links. Rust code and its tests define runtime behavior. The
  ledger check does not prove Rust refinement. CI checks the generated artifacts.
- Types/defaults/limits outside the ledger: rustdoc. Tables: migrations; core functions, triggers,
  policies and views: `crates/storage-pg/definitions/`.
  Settings follow [`Proxiima.Configuration`](docs/ledger.md#configuration). Action/resource
  schemas: the live catalog.
- Require explicit task authorization for new crates, binaries, services, migrations, protocol surfaces and registration paths so review can enforce scope.
- Keep transient plans and bulky evidence in ignored `.local/` so tracked documents remain durable.

## Workspace

| Path | Responsibility |
|---|---|
| `apps/proxiima-mcp` | Canonical MCP host; core only, links no spiice |
| `crates/core` | Domain types, engine, typed ports, registry |
| `crates/proxiima` | Host facade; re-exports the SDK as `proxiima::spiice` |
| `crates/proxiima-spiice` | Spiice SDK: what a spiice crate depends on |
| `crates/storage-pg`, `crates/blob-s3` | PostgreSQL and object backends |
| `crates/proxiima-server`, `crates/auth-oidc` | Transports and bearer authentication |
| `crates/llm-openai-compat` | Host-injected embedding adapter |
| `crates/pg-testkit` | Owned-server test databases/templates |
| `spiices/example` | Test and tutorial spiice (`publish = false`) |
| `tools/dev-idp`, `tools/dev-migrate` | Local issuer and database bootstrap |

## Branch and release policy

1. Start from `origin/verified` so work starts after CI. Push directly to `main` with linear history and keep branches and numbered tags free of force rewrites. Outside contributions use pull requests.
2. Before pushing, have `origin/main..HEAD` reviewed in a fresh session given the diff and its contract, not the reasoning that produced it.
3. Leave the workspace version alone for routine changes. A deliberate bump requests a numbered release through Verify. Do not tag by hand.
4. See [`Proxiima.Repository`](docs/ledger.md#repository).
5. Run `scripts/check-migrations.py` when changing migration history. See
   [`Proxiima.Migrations`](docs/ledger.md#migrations) and [contributing](docs/contributing.md#migrations).

## Boundaries checked by code and review

The principles are in
[`Principles.lean`](lean/Causa/Principles.lean); each names its runtime
enforcer there. This table holds the engineering boundaries.

| Boundary | Enforcer |
|---|---|
| Optional kernel witnesses, typed schema contracts | Payload traits, registry freeze, authorized sidecar helpers |
| Build-time spiices; no raw spiice core SQL | The `proxiima-spiice` crate boundary; the sidecar read context and `OwnerTransaction` refuse core SQL; review |
| Atomic writes and scoped host state | `UnitOfWork`, private permits and PG rollback tests; see [`Proxiima.HostState`](docs/ledger.md#host-state) |
| Trusted composed SQL and closed database vocabularies | `SqlText` and `SqlBuilder`; the architecture guardrail keeps sqlx's safety assertion and query builder inside their module; migration catalog tests |
| No mirrors: one source of truth | Rust to Rust: one type, the compiler. Rust to wire: the schema derive. Code to prose: link, do not restate (review judgment; example: a docs page listing the pin kinds beside `EdgeKind` went stale when a kind was added) |

Judgment: if a proposed edge needs its own reason/confidence, find the node
that owns the statement before adding code. Example: an interpretation belongs
in a Perspective payload, not a new edge kind.

## Delegation

Prefer native read-only Codex workers for non-trivial tracing and scoped native Codex workers with disjoint ownership and executable gates for implementation, to preserve the critical path. Non-Codex workers need explicit selection.
The caller owns scope, diff review, verification, commits and publication. Workers never stage or commit because the worktree index may be outside their write scope. Preserve others' edits and withhold sensitive material from delegates.

## Verification and failures

Run focused checks while iterating and the [contributing gates](docs/contributing.md#local-verification) before pushing. Serve the MCP catalog for tool changes because registration alone cannot verify what a client sees.
Compile `*_for_tests` constructors and fixtures only under `cfg(test)` or the `testkit` feature.
Point PG tests only at an owned server because the fixture sweep operates across it.
Fix, record or escalate each observed red or skip with exact commands, output and any deferral reason under `.local/`. Keep every surviving behavioral assertion.

## Artifacts and commits

- Write technical artifacts in English and keep diagrams conceptual, so the generated schema stays authoritative.
- Scope Conventional Commit subjects to an existing component for release-note grouping.
- Put a concrete operator or client action in a breaking commit's `Upgrade:` footer so the rendered note is actionable.
- Record applicable decisions in `Tesla-Valve` trailers.

Resolve conflicts with a principle before changing architecture because those decisions compound.
