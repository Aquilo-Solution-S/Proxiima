# Configuration

This page lists every setting the canonical host `proxiima-mcp` and the
library crates it links read, with its kind of value and default. Each crate
declares its settings once, in a `settings!` block; the table below is
rendered from those declarations by a test in `apps/proxiima-mcp`, which
rewrites it and fails when it differs. Settings that only development tools,
scripts and tests read are listed in
[contributing](contributing.md#development-settings).

`DATABASE_URL` is the serving connection; `PROXIIMA_PLATFORM_DATABASE_URL` is
the required migration and maintenance connection
(`PROXIIMA_MIGRATION_DATABASE_URL`, when set, takes over the migrations). Both
roles use owner RLS. A serving host holds two pools of
`PROXIIMA_PG_MAX_CONNECTIONS` each (runtime and platform), plus a migration
pool of the same size during boot and the migrate step; size the server's
`max_connections` for every host on it plus operator connections
([behind a connection pooler](operate.md#behind-a-connection-pooler)).

An empty or whitespace-only value is unset. At startup the host logs a warning
naming each `PROXIIMA_*` environment variable that no setting declares; the
variable is ignored.

<!-- settings:begin -->

| Setting | Purpose | When needed | Type | Default |
|---|---|---|---|---|
| DATABASE_URL | serving database connection | required for migrate, boot and maintenance | URL |  |
| PROXIIMA_PLATFORM_DATABASE_URL | platform database connection | required for migrate, boot and maintenance | URL |  |
| PROXIIMA_MIGRATION_DATABASE_URL | direct/session-mode connection of the migrate step and of schema validation at boot and maintenance; same role and database as `PROXIIMA_PLATFORM_DATABASE_URL`, which serves both when this is unset | platform URL behind a transaction-mode pooler ([behind a connection pooler](operate.md#behind-a-connection-pooler)) | URL |  |
| PROXIIMA_STORE_NUMBER | immutable host-assigned store number, 0 to 65535, persisted by the migrate step and checked by every boot | required for migrate, boot and maintenance | integer |  |
| PROXIIMA_MISSING_MIGRATION_POLICY | missing-version policy: `reject` or `allow` | releases intentionally share a database | choice | `reject` |
| PROXIIMA_RUNTIME_GRANTS | runtime grants of the migrate step; serving never grants | owner-RLS deployment | boolean | `false` |
| PROXIIMA_MCP_BIND | MCP listener address; no listener when unset | serving MCP | socket address |  |
| PROXIIMA_EXPOSE_NETWORK | non-loopback exposure gate | public listener | boolean | `false` |
| PROXIIMA_REST_ENABLED | REST projection switch | serving `/v1` | boolean | `false` |
| PROXIIMA_ALLOWED_ORIGINS | CORS origin allowlist (no port = any port; an explicit port also matches a browser Origin that omits it when it is the scheme default, HTTP 80 or HTTPS 443) | browser clients | comma-separated list |  |
| PROXIIMA_ALLOWED_HOSTS | Host allowlist; when unset, the host of `PROXIIMA_PUBLIC_URL` and the allowed origins; loopback is always allowed | non-loopback listener | comma-separated list |  |
| PROXIIMA_STREAM_MAX_LIFETIME | authenticated stream lifetime | long-lived MCP streams | seconds | `3600` |
| PROXIIMA_REQUEST_HEADERS | forwarded request-header names | tools need selected headers | comma-separated list |  |
| PROXIIMA_FORWARDER_SUBJECTS | user UUIDs of subjects requiring a verified owner claim; set with `PROXIIMA_FORWARDER_ROLE` | forwarded owner access | comma-separated list |  |
| PROXIIMA_FORWARDER_ROLE | non-managing forwarding ceiling: `viewer`, `ingest` or `editor` | forwarded owner access | choice |  |
| PROXIIMA_HEALTH_ENDPOINTS | health route switch | probes need HTTP health | boolean | `false` |
| PROXIIMA_MAX_REQUEST_BODY_BYTES | request body ceiling in bytes, at least 1 | listener sizing | integer | `4194304` |
| PROXIIMA_MCP_SSE_KEEP_ALIVE_SECS | SSE ping interval; `0` sends none | proxy idle timeout | seconds | `15` |
| PROXIIMA_MCP_SESSION_IDLE_SECS | session idle timeout; `0` turns it off | session serving | seconds | `300` |
| PROXIIMA_MCP_SSE_RETRY_SECS | SSE retry hint; `0` sends none | client reconnect tuning | seconds | `3` |
| PROXIIMA_MCP_SESSIONS | server-side session switch | stateful MCP clients | boolean | `true` |
| PROXIIMA_MCP_JSON_RESPONSE | JSON response switch | stateless MCP clients | boolean | `false` |
| PROXIIMA_PUBLIC_URL | public OAuth URL: `http` or `https`, with a host and no credentials, query or fragment | OIDC/public serving | URL |  |
| PROXIIMA_OIDC_ISSUER | OIDC issuer; setting it turns on OIDC authentication | OIDC serving | URL |  |
| PROXIIMA_OIDC_AUDIENCE | token audience: this resource server, never an OAuth client id | OIDC serving | text |  |
| PROXIIMA_OIDC_REQUIRE_ACCESS_TOKEN_TYP | require JWT `typ` `at+jwt`; off, an absent `typ` or `JWT` also passes and any other is refused | whenever the issuer sends RFC 9068 access tokens | boolean | `false` |
| PROXIIMA_OIDC_JWKS_URI | explicit JWKS endpoint; discovered from the issuer when unset | pinned discovery override | URL |  |
| PROXIIMA_OIDC_JWKS_JSON | pinned JWKS document; nothing is fetched from the issuer; excludes `PROXIIMA_OIDC_JWKS_URI` | issuer network unavailable | JSON |  |
| PROXIIMA_OIDC_HTTP_TIMEOUT_SECONDS | OIDC discovery and JWKS request timeout, 1 to 300 seconds | tune issuer requests | seconds | `10` |
| PROXIIMA_OIDC_ALLOWED_SUBJECTS | subject allowlist, in addition to the subject map | restrict principals | comma-separated list |  |
| PROXIIMA_OIDC_SUBJECT_MAP_JSON | issuer-aware subject map: a JSON array of `{iss, sub, user_id}` objects | required with `PROXIIMA_OIDC_ISSUER` | JSON |  |
| PROXIIMA_PUBLICATION_SOURCE | CloudEvent source identity: an absolute URI or URN | listenable Fact publication | text |  |
| PROXIIMA_OUTBOX_MAX_PENDING | pending outbox ceiling, at least 1 | outbox backpressure tuning | integer | `100000` |
| PROXIIMA_OUTBOX_MAX_PAYLOAD_BYTES | captured payload ceiling in bytes, at least 1 | outbox export sizing | integer | `524288` |
| PROXIIMA_OUTBOX_PUBLISHED_RETENTION_SECS | published-record retention, at least 60 seconds; `0` keeps published records forever | reclaim delivered records | seconds | `604800` |
| PROXIIMA_OUTBOX_HTTP_URL | HTTP outbox receiver: `https`, or `http` to loopback or a host in `PROXIIMA_PLAINTEXT_HOSTS` | HTTP delivery | URL |  |
| PROXIIMA_OUTBOX_HTTP_TOKEN_FILE | HTTP bearer token file, re-read on every delivery | authenticated HTTP delivery | file path |  |
| PROXIIMA_OUTBOX_HTTP_CA_FILE | HTTP CA bundle (PEM); when set, the only trusted roots | private receiver CA | file path |  |
| PROXIIMA_OBJECT_PURGE_INTERVAL_SECS | object-purge drain cadence; `0` turns it off | in-process expired-upload sweep and purge drain | seconds | `60` |
| PROXIIMA_EMBED_REQUEST_TIMEOUT_SECONDS | provider request timeout, 1 to 3600 seconds | embedding worker tuning | seconds | `120` |
| PROXIIMA_EMBED_BATCH_SIZE | texts per provider request, 1 to 1024 | embedding worker tuning | integer | `32` |
| PROXIIMA_EMBED_DRAIN_CONCURRENCY | batches one drain pass sends at once, 1 to 64 | indexing throughput the provider can take | integer | `1` |
| PROXIIMA_EMBED_WORKER_INTERVAL_SECONDS | first idle wait of the embedding worker, 1 to 3600 seconds; doubles while drains find nothing, up to 60 s or this value if longer; a write committed in the process wakes it at once | embedding worker tuning | seconds | `5` |
| PROXIIMA_EMBED_STALE_CLAIM_TIMEOUT_SECONDS | stale claim timeout, 1 to 86400 seconds and longer than the request timeout | embedding worker recovery | seconds | `900` |
| PROXIIMA_PLAINTEXT_HOSTS | exact host names, besides loopback, that outbound endpoints may reach over plaintext HTTP | named internal HTTP sidecar | comma-separated list |  |
| PROXIIMA_PG_MAX_CONNECTIONS | size of each pool, at least 1: runtime, platform, and the migration pool of boot and the migrate step | pool tuning | integer | `5` |
| PROXIIMA_PG_STATEMENT_TIMEOUT_MS | statement timeout of every pool, sent as a startup option; `0` sends none | database tuning | milliseconds | `300000` |
| PROXIIMA_PG_ACQUIRE_TIMEOUT_SECS | pool acquire timeout | database tuning | seconds | `5` |
| PROXIIMA_PG_IDLE_TIMEOUT_SECS | idle connection timeout | database tuning | seconds | `600` |
| PROXIIMA_PG_MAX_LIFETIME_SECS | connection lifetime | database tuning | seconds | `1800` |
| PROXIIMA_PG_HNSW_EF_SEARCH | HNSW search breadth, 1 to 1000 | semantic search tuning | integer | `100` |
| PROXIIMA_PG_HNSW_ITERATIVE_SCAN | iterative scan mode: `off`, `strict_order` or `relaxed_order` | filtered semantic search | choice | `relaxed_order` |
| PROXIIMA_PG_HNSW_MAX_SCAN_TUPLES | iterative scan bound, 1 to 2147483647 | filtered semantic search | integer | `20000` |
| PROXIIMA_S3_BUCKET | S3 bucket of cited blobs | enable S3 storage | text |  |
| PROXIIMA_S3_REGION | S3 region | S3 storage | text |  |
| PROXIIMA_S3_ACCESS_KEY_ID | S3 access key | S3 storage | text |  |
| PROXIIMA_S3_SECRET_ACCESS_KEY | S3 secret key | S3 storage | secret |  |
| PROXIIMA_S3_ENDPOINT_URL | object I/O endpoint, also signed into upload URLs; AWS endpoint resolution when unset | non-AWS S3 storage | URL |  |
| PROXIIMA_S3_STORE_ID | object store identity, 1 to 64 of `[A-Za-z0-9._-]`; derived from the endpoint and bucket when unset | processes reach one store by different endpoints, or stores share an endpoint and bucket | text |  |
| PROXIIMA_S3_FORCE_PATH_STYLE | S3 path-style switch | path-style endpoint | boolean | `false` |
| PROXIIMA_S3_UPLOAD_TTL_SECONDS | upload URL lifetime, at most 604800 seconds (7 days) | cited blob uploads | seconds | `900` |
| PROXIIMA_S3_MAX_BLOB_BYTES | cited blob size ceiling in bytes | S3 blob storage | integer | `104857600` |
| PROXIIMA_TOOL_PROFILE | served-surface profile: `memory` (fail-closed) or `full` (adds owner transfer and membership changes); every profile serves the declared reads | deployment commands plus mandatory reads | choice | `memory` |
| PROXIIMA_TOOL_ALLOW | canonical tool ids added to the profile | extend deployment commands | comma-separated list |  |
| PROXIIMA_TOOL_DENY | canonical tool ids removed from the profile | narrow commands; mandatory reads cannot be denied | comma-separated list |  |
| PROXIIMA_EMBED_BASE_URL | embedding endpoint; redirects are refused, so configure the provider's final endpoint directly | enable embeddings | URL |  |
| PROXIIMA_EMBED_API_KEY | embedding bearer | hosted embedding endpoint | secret |  |
| PROXIIMA_EMBED_MODEL | embedding model | enable embeddings | text |  |
| PROXIIMA_EMBED_DIM | embedding vector width, one the store indexes | non-default model width | integer | `1024` |
| PROXIIMA_EMBED_MATRYOSHKA | nested-prefix request switch | Matryoshka model | boolean | `false` |
| PROXIIMA_EMBED_MAX_INPUT_CHARS | embedding input ceiling in characters, at least 1 | provider input tuning | integer | `16384` |
| PROXIIMA_EMBED_QUERY_INSTRUCTION | template every search query is embedded in; holds `{query}`, and `\n` is a newline | instruction-tuned model | text |  |

<!-- settings:end -->

Removed names are documented only in release notes; they are not settings and
must not be reintroduced as compatibility aliases.
