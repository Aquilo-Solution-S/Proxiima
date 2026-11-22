# Contributing

Use the repository guidance and the Repository ledger alongside this guide. [Repository guidance](https://github.com/Aquilo-Solution-S/proxiima/blob/main/AGENTS.md) · [Repository ledger](ledger.md#repository)

## Before editing

Read [concepts](concepts.md) and the relevant integration page.

## Code and tests

Prefer a type that makes an invalid state unrepresentable. Parse untrusted
configuration, network data, database rows, and serialized payloads at the
boundary, returning a typed error. Recheck facts that can become stale at the
commit boundary. Tests should prove intent and boundary refusal, not duplicate
the implementation.

Typed payloads keep their semantic projections typed through authorization,
storage and read models. Protocol JSON is decoded or emitted at the boundary;
inner code does not serialize a payload and inspect JSON to recover natural-key,
display or schema meaning. `PayloadProjection` and the frozen
`NaturalKeyDeclaration` are the compiler-visible seam. Every payload trait
(including cited-object payloads) implements that supertrait; its `KEY_FIELDS`,
`display`, and `durable` methods
are explicit typed fields, not recovered from protocol JSON. Wire names
therefore remain separate from durable identity codecs, which are explicit
compatibility formats. Use `KeyField::padded_text` only for PostgreSQL `CHAR(n)`
columns; ordinary `text` fields must use `KeyField::text`.

`NaturalKeyDeclaration::of::<P>()` is reserved for a stateful Fact whose
projection declares the key; stateless schemas use
`NaturalKeyDeclaration::EMPTY`. PG sidecar installation is an async fallible
boundary: `with_sidecars(...).await` validates declared key columns against
the real SQL schema and must be awaited and propagated during boot.

Core SQL reads follow [`Proxiima.DatabaseAuthority.MemorySqlRoute`](ledger.md#database-authority).

Do not add a runtime spiice registry, raw spiice SQL against core tables,
caller-supplied owner authorization, an edge table, generic JSON payload
escape hatches, or compatibility aliases for removed ontology. If a feature
needs a connection with its own reason or confidence, add the node that owns
that statement; do not add an edge sidecar.

## Migrations

For migration numbering, baseline adoption, and definitions, see
[`Proxiima.Migrations`](ledger.md#migrations). Run `scripts/check-migrations.py`
before changing SQL. Edit the replaceable SQL in `crates/storage-pg/definitions/`
in place. Compare the schema and seed rows of a new baseline with
the previous release before landing it.

A migration that drops, renames or retypes a column of `proxiima_core.memory`
first drops `proxiima_core.live_memory` and
`proxiima_core.memory_including_forgotten` (and `memory_version` if it lists the
column); the definitions pass recreates them. A fresh test database does not
show the omission, because the views do not exist yet when migrations run.

Before retiring a Fact sidecar, run the erase-capable release with its host
inverses and sidecars still present. Stop producers of the retired schema.
The later schema-only migration uses the existing platform-scoped migration
transaction, at `READ COMMITTED`:

```sql
SELECT proxiima_core.assert_fact_schema_drained(
    'example/retired-v1', ARRAY['proxiima_example.retired_v1']);
DROP TABLE proxiima_example.retired_v1;
DELETE FROM proxiima_core.spiice_surface
 WHERE table_name = 'proxiima_example.retired_v1';
```

Remove the sidecar's generated presence trigger on `proxiima_core.memory` in
that migration as well. The guard requires the non-bypass migration role,
platform scope, and existing declared sidecar names. It holds a memory writer
fence until transaction end and refuses any raw admission naming the schema
or a retiring table, including forgotten versions and rows of other owners.
Keep the guard, DDL, registration removal, and migration ledger commit in the
same transaction; a successful call in an earlier transaction proves nothing
about the later DROP. Admission checks the sidecar's catalog existence as well
as its registration, so a stale writer cannot commit a stamp after DROP. The
guard erases no data and stores no completion flag; its refusal instructs the
operator to run the erase-capable release first. A fresh empty store can
proceed directly.

## Local verification

Run focused checks during iteration and the complete gates before pushing.
Report skipped checks explicitly.
Set `PROXIIMA_TEST_PG_URL` to your disposable PostgreSQL server. [Development bootstrap](get-started.md#1-boot-postgres)

```sh
cargo fmt --all -- --check
cargo check --workspace
cargo clippy --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --locked
cargo nextest run --workspace --all-targets --all-features --locked
cargo nextest run -p proxiima-mcp --all-targets --locked
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features --locked
cargo test --doc --workspace --all-features --locked
(cd lean && lake build && lake exe ledger check)
python3 scripts/check-migrations.py --self-test
python3 scripts/check-migrations.py
python3 scripts/check-doc-links.py
python3 scripts/check-upgrade-notes.py --self-test
python3 scripts/check-release-version.py --self-test
python3 scripts/check-release-version.py
python3 scripts/verify-review.py --self-test
python3 scripts/check-architecture-guardrails.py --self-test
python3 scripts/check-architecture-guardrails.py
python3 scripts/check-schema-ids.py --self-test
python3 scripts/check-schema-ids.py
python3 -m pip install -r requirements-docs.txt
python3 -m mkdocs build --strict
```

### PgBouncer

Run PgBouncer 1.26.1 or later in transaction mode in front of
`PROXIIMA_TEST_PG_URL`, following the `pgbouncer` job configuration. Set
`PROXIIMA_TEST_PGBOUNCER_URL` to that listener and run the job's nextest
filters for the storage suites and the host boot test. Without the pooler URL,
`pgbouncer_pg` returns early and the other suites connect directly.
[CI workflow](https://github.com/Aquilo-Solution-S/proxiima/blob/main/.github/workflows/ci.yml)

### S3 integration tests

Start the local S3 service on Compose port 9100 and initialize the versioned
`proxiima-dev` bucket. Set the five S3 variables before running nextest.

```sh
docker compose -f docker-compose.dev.yml up -d --wait s3
docker compose -f docker-compose.dev.yml --profile init run --rm s3-init
export PROXIIMA_S3_ENDPOINT_URL=http://127.0.0.1:9100
export PROXIIMA_S3_BUCKET=proxiima-dev
export PROXIIMA_S3_REGION=us-east-1
export PROXIIMA_S3_ACCESS_KEY_ID=proxiima
export PROXIIMA_S3_SECRET_ACCESS_KEY=proxiima-dev
cargo nextest run --workspace --all-targets --all-features --locked --run-ignored all
```

If you set `PROXIIMA_DEV_S3_PORT`, use that port in the endpoint URL. Report
a run without `--run-ignored all` as not running the S3 lane.

### Settings

[`Proxiima.Configuration`](ledger.md#configuration). When a setting declaration changes, run `the_operator_page_is_the_rendered_settings_table` in `apps/proxiima-mcp` and commit the regenerated table.
Every crate with declared settings adds a `declared_defaults…` test that checks
its readers through `proxiima_core::test_fixtures::assert_declared_defaults`.

### Development settings

These names are read only by development tools, scripts and tests, never by
a shipped host, so they are not on the operator page.

| Setting | Purpose | Reader |
|---|---|---|
| `PROXIIMA_TEST_PG_URL` | admin URL of an owned test Postgres server | `pg-testkit` and the schema script fallback |
| `PROXIIMA_TEST_PGBOUNCER_URL` | transaction-mode PgBouncer in front of the test server; the storage fixtures route serving pools through it | `pg-testkit` |
| `PROXIIMA_PG_TESTKIT_CHILD` | child-process marker | `pg-testkit` split-template and admin URL tests |
| `PROXIIMA_PG_TESTKIT_TEMPLATE` | template database name | `pg-testkit` split-template test |
| `PROXIIMA_DIFFERENTIAL_DIR` | directory the owner differential tests dump their actuals to | storage-pg differential tests |
| `PROXIIMA_SCALE_EMBEDDINGS`, `PROXIIMA_SCALE_QUERIES`, `PROXIIMA_SCALE_ERASE_MEMORIES` | size of the scale harness run ([operate](operate.md)) | `crates/storage-pg/tests/scale_run.rs` |
| `PROXIIMA_DEV_POSTGRES_PORT` | host port of the Compose Postgres | `docker-compose.dev.yml` |
| `PROXIIMA_DEV_S3_PORT` | host port of the Compose S3 store | `docker-compose.dev.yml` |
| `PROXIIMA_SCHEMA_PG_URL` | admin URL of the schema dump | `scripts/regen-schema-sql.sh` |
| `PROXIIMA_PG_DUMP` | `pg_dump` command of the schema dump | `scripts/regen-schema-sql.sh` |
| `PROXIIMA_RESET_CONFIRM` | reset confirmation token, spelled exactly `reset-my-dev-db` | `tools/dev-migrate --reset` (declared in its own `settings!` block) |

## Review and release

Follow the branch policy for reviewing and releasing changes. [Branch policy](https://github.com/Aquilo-Solution-S/proxiima/blob/main/AGENTS.md#branch-and-release-policy)

`nightly` is a moving source selector. Refresh it in an existing clone with
`git fetch origin +refs/tags/nightly:refs/tags/nightly`. Nightly trees keep the
last declared release version, so identify them by SHA. Pin the same immutable
numbered tag or commit across consumer dependencies. Cargo locks a git
dependency to a commit until `cargo update`. Per-release changes live only in
the release notes.

For a spiice change, also verify the served catalog and the host e2e path. For
an outbox or host-state change, exercise the transaction and retry or rollback
boundary against real Postgres. For an API change, update the facade rustdoc,
the relevant integration page, and the executable example together.

Continue with [build a spiice](build-a-spiice.md) or [host integration](host-integration.md).
