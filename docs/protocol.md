# Protocol

The contract is a registered action or resource. MCP and REST project the same
build-time manifest, authorization, typed inputs, and engine operations.

| Discover | Authority |
|---|---|
| Tools and per-action input schemas | MCP `tools/list`, `proxiima://tools` |
| Resources and parameterized reads | MCP `resources/list`, `resources/templates/list` |
| Payload schemas | Live schema resources listed by that server |
| Resource answer schemas | `proxiima://schemas` (`resources`, keyed by URI template), `/v1/openapi.json` |
| HTTP operations and schemas | `/v1/openapi.json` on a REST-enabled host |
| Agent usage instructions | MCP `proxiima://how-to` |

Catalogs are caller-scoped. Discover them on the running deployment; linked
spiices, deployment profiles, and effective tool scope determine the result.
Types, defaults, and limits belong to rustdoc and the generated schemas.

Catalog action rows include caller-visible commands from tagged and argv
dispatchers. An argv command uses its tool's shared argument schema and
description; its per-action field lists are empty because the tool owns its
CLI grammar.

## Owner scope

The host authenticates the bearer and resolves a user through `OwnerAccessPort`
into `OwnerRoles`. `X-Proxiima-Owner` selects one raw owner UUID; it grants no
authority. Reads use the resolved readable owner set. A write selects one owner
and checks its required role at the command boundary. Kind restrictions remain
part of the effective scope.

MCP sessions bind the default owner and authenticated principal at initialization.
A subsequent request with the session ID can omit `X-Proxiima-Owner` and use
the bound owner. A live binding selects the owner, and a supplied header must
name the same owner. Each request reauthenticates the principal, rechecks access,
and retains the credential's effective owner roles. Selecting a default does
not confine reads or per-call space selection; credentials that
are explicitly confined remain confined. Server-issued memory-space keys are
selectors resolved through the same context. Missing and unreadable entity
references have the same public result. A readable source keeps its pins even
when their target is redacted.

`core_forget` and `core_restore` accept the same optional `space` key as
`core_remember`. Pass a key from `core_memory_spaces` and the memory's UUID
to hide or restore it in that space; omit the key to use the session's selected
owner. The key grants no write authority.

Goal creation and decomposition use the optional `space` selector, defaulting
to the session owner. Existing Goal edits resolve the named Goal's owner when
`space` is omitted; an explicit space narrows that selection.
Evidence and the caller's self-Perspective may belong to other spaces the
writer can read; reading the goal grants no access to those memories.
Motivation is explicit: `motivated_by` names exact Perspective versions;
omission leaves a new Goal unmotivated, including in `core_episode_commit`.
`guided_by` records exact Goals that guided a Perspective version. Episodes
remain owner-authorized; Goal guests contribute through derive/interpret.

Session retention is scoped to the authenticated principal, independently of
the selected owner. Opening more sessions can evict only that principal's own
oldest binding; a full global binding store refuses a new principal instead
of evicting another principal's sessions. A valid bearer presenting another
principal's session id receives the same opaque 404 as an unknown or expired
session, and does not refresh or reach that session's protocol state.

The earlier of the bearer expiry and the server's stream lifetime limit starts
the final drain of an authenticated MCP response. The server delivers up to
16 frames that are immediately ready, including any frame returned by a poll
that reaches the deadline. It ends the response at the limit, when a poll
would need to wait for a frame, or when the source ends.

A configured forwarder carries its selected owner in the verified token's
`owner` UUID claim and writer claims `by` (principal UUID) and `via`
(session UUID or explicit `null`). Missing or invalid claims refuse
authentication. The
owner header may agree or be absent; it cannot replace the claim. Every
session request must also agree with the current token's claim. The policy
role reaches that one registered personal or group owner, while the
authenticated subject and session principal remain the forwarder. Ordinary
subjects gain no selection or authority from this claim.

Enforcers: [`AuthzContext`](https://github.com/Aquilo-Solution-S/proxiima/blob/main/crates/core/src/authz.rs),
the engine command ports, and PostgreSQL owner RLS. See the
[identity and ownership map](concepts.md#identity-and-ownership) for roles,
nested groups, and use cases, and the
[request flow](host-integration.md#authentication-and-owner-scope) for integration.

## Reads and consistency

Reads return committed rows admitted by owner or Goal authority. A `t` identifies one version;
`handle` identifies its series. Head queries and exact-version reads are
different operations. A Goal handle read returns its current head; explicit
version selectors can retrieve historical rows. An explicit `space` on a
`proxiima://memory/{id}` read must be readable, including when the memory comes from
another store. When `space` is omitted, only the local route requires read
access to the session's own space.

Search hits and single and batch memory reads report the stored `owner` UUID
independently of the writer. Local `space` labels that home owner: `current` for the session's
owner, otherwise its UUID. A Goal-authorized read can report an owner outside
`core_memory_spaces`; the label grants no owner role or write authority. Goal
collaborators select authorized work with `working_goal`. A remote read,
another store's search hit and another store's lineage node or `core_think`
visit report the owner's UUID that store reported as `space`; a remote read
retains the stored payload and a Fact's occurrence time, just as local reads
do. Its `expand_neighbors` edges and an ancestors walk from it come from that
store, which admits them to the caller as it admits the read.

A lineage node, a `core_think` visit and a `core_recall` sketch carry the same
`space` label, always present, and every memory answer type parses back from
its JSON. It is `null` only when the owner is unknown: a node or visit with a
`status`, or a memory no longer readable.

Pagination uses an opaque cursor bound to the read's filters. Memory recency
uses `(created_at, t)`; Goal pages use `(handle, t)`, so changing an Active Goal
to Paused does not move its series across the page boundary. A page is not a
long-lived database snapshot: concurrent writes can change later results.

Goal state selection is a union. Fact reads can select references to any
readable version of specified Goal series, intersected with other filters.
Unreadable and absent series both match nothing. Pin traversal follows stored
declarations; similarity influences retrieval scores only.

The publication outbox is the host change channel. It captures listenable
Facts in their write transaction and delivers them at least once to host
intakes. Delivery is separate from the client response; consumers deduplicate
by event identity. Host routing and execution stay in the host. See
[host integration](host-integration.md#fact-publication-outbox).

## Search across stores

`core_search_memories` also lists memories newest first when `query` is omitted
and `order=recency` is explicit, without embedding or text matching.
`exclude_schema_ids` removes schemas before the page limit for listings and
query searches, including searches of other stores. See the
[live catalog](#protocol) for arguments and defaults.

An other-store search carries the caller's filters and requested mode through
`RemoteSearch`. Each store resolves its own embedding availability and returns
`RemoteSearchResult`, including whether it fell back to lexical even when it
found no hits. The merged answer reports a fallback if any answer did.

A store that cannot honor the request returns `UnsupportedSearch`; it is
reported in `unanswered` as `{store, why}`, where `why` is that error,
without a retry that drops filters. A store that does not answer in time
carries the deadline error the same way. Recency orders the
combined candidates newest first before the result limit. Other stores answer
on the first page; the local page retains its own cursor.

When other stores are wired but their list does not answer (it failed or timed
out), a search that names no store returns this store's hits with
`inventory_unanswered: true` and `unanswered: []`: other stores' hits are
missing, so retry for a complete page. A search that names another store fails
retryable with `store_unavailable`; one that names only this store runs as
asked.

## Writes

Typed payload registration fixes each schema's kind, references, validation,
sidecars, and listen declaration. A Fact receipt proves admission, not external
truth. Source ingestion deduplicates using its owner, source, and ingestion
key; keyless admissions mint a new version. An `idempotency_key` names one
request: the same key with the same content replays and returns the same
handle; with changed content it fails with `idempotency_conflict` naming
that handle; write a new version with a new key and `supersedes`. See
the [admission rules](ledger.md#admission) in `Proxiima.Admission`.

An episode or explicit `UnitOfWork` commits its declared operations atomically.
Authoring derived content carries declared-input provenance. An interpretation
Perspective declares references without claiming a derivation. No operation
writes an independent edge.

Goal resources expose `motivated_by`; Perspective reads expose `guided_by`.
Both name exact versions, with unreadable targets omitted from these role
fields. Generic memory-source edge traversal includes Perspective guidance;
Goal-owned motivation is read through the Goal resource. Owner export keeps
the stored declarations, including historical assignments.

`proxiima://goal/{id}/surroundings` reads the declarations around a Goal in both
directions, including dependencies, evidence, motivation, guidance and memory
pins. It reports distances from the Goal and pages the declarations without a
client-side walk. Other-store memories are leaves read there as the caller.
Unreadable leaves contribute only an anonymous count; an unanswered store is
reported separately. Its continuation identifies the read and position,
never an unreadable endpoint. Like other pages, later calls see current data
rather than a retained snapshot.

Goal transitions append a later version. `transition` `abandon` takes an
Active or Paused goal in one write, so a paused goal never becomes Active on
its way out; `mark_achieved` takes only an Active goal. Both closing actions refuse a Goal while a
decompose child is open, with `InvalidArgument` on `goal` and the message
prefix `goal has open children: ` followed by the open children's current
version IDs in child series handle order. A Goal can close once none of its
children are open, including when they are all Abandoned.
Pause's `pending: keep|discard` is an instruction for the host's queue. The
host decides how to execute that instruction; Proxiima persists intent and
state. Hard deletion uses the explicit host or spiice-scoped inverse paths in
[data and erasure](data-and-erasure.md).

Caller scope names exact command/action and resource keys. A deployment's
served surface always includes declared reads and core resources; that does
not add keys to the caller. Resource templates use their template key rather
than a concrete URI, and the guide requires `resource:how-to`. MCP and REST
listings and invocation apply the same combined policy and owner-role checks.

## REST projection

Both the host's `rest` Cargo feature and its runtime REST switch must be on.
The shared authentication layer protects MCP and `/v1`. The stock Docker build
serves MCP; build a REST-capable host explicitly when needed.

| Route shape | Behavior |
|---|---|
| `GET /v1/tools` | Caller-visible tool catalog |
| `GET /v1/tools/{tool}` | Tool metadata and schemas |
| `POST /v1/tools/{tool}` | Flat call, including dispatcher `action` |
| `POST /v1/tools/{tool}/{action}` | Narrowed action call |
| `QUERY` on a read-only tool/action | Read with JSON body; `POST` also works |
| `GET /v1/resources/…` | Generated resource read |
| `GET /v1/openapi.json` | Generated OpenAPI contract |
| `GET /v1/how-to` | Caller-scoped guide |

Resource URIs map from `proxiima://x` to `/v1/resources/x`; template arguments
become path or query parameters. The instructional `proxiima://how-to` resource
also answers at `/v1/how-to` when its caller key is admitted. Tool and action
`ToolEffect` declarations choose allowed methods. A whole-dispatcher `QUERY`
that selects a write returns `405` with the selected action's `Allow` header.
The `proxiima://tools` catalog exposes each registered tool's reach as
`open_world`. OpenAPI tool operations expose it as `x-proxiima-open-world`.

A narrowed action route injects `action`; including it in that route's body
is invalid even if the values agree. Unknown actions return `404`. The
generated action schema supplies the request shape; REST does not maintain a
second schema registry. Responses carry `Cache-Control: private, no-store`.

REST call context is header-borne: bearer, selected owner, optional
`X-Proxiima-Model-Id` and `X-Proxiima-Self-Perspective`, and `User-Agent`. A trusted
model identity bound by the host outranks a caller label; a conflicting label
is refused. Forwarded authority is available only through the host's explicit
forwarder policy.

## Errors

MCP tools return the shared structured tool error; resource parameter errors
name the parameter. An unknown resource and a missing or invisible entity
return a resource-not-found error. The negotiated MCP revision controls wire
error-code compatibility. Internal errors redact source chains and credentials.
No PostgreSQL or driver sentence reaches a caller: a constraint refusal names
the constraint and its SQLSTATE, a core SQL refusal or backstop keeps core's own
sentence, any other database fault only its SQLSTATE, and the server logs the
full error.

PostgreSQL stores U+0000 in neither `text` nor `jsonb`. A tool argument
holding it in any string or key is refused as invalid input naming the string's
JSON Pointer, before the tool runs; strip or skip it. A host writing through
the engine directly gets the same class, without the pointer, from the
database's refusal, and nothing is written.

A tool that ran and failed answers MCP's tool-execution error, not a JSON-RPC
error: a result with `isError: true` whose content is the tool's own text. A
typed tool returns `ToolError::Failure(text)`; a host tool answers
`ToolReply::Failure` or `ToolReply::StructuredFailure`. Proxiima neither
redacts nor rewords that text. Every other `ToolError` stays a JSON-RPC error.
REST answers a tool failure as `422` with type `tool-failure` and the text as
`detail`. A successful tool that returns a compact summary alongside follow-up
MCP `resource_link` blocks answers `ToolReply::StructuredWithLinks`; JSON-only
surfaces unwrap its structured object.

REST returns `application/problem+json`: machine-readable `type`, `title`,
`status`, safe `detail`, and request `instance`. Its exhaustive variant map is
[`rest/error.rs`](https://github.com/Aquilo-Solution-S/proxiima/blob/main/crates/proxiima-server/src/rest/error.rs).
The listener's own refusals in front of every route (Host, Origin, CORS
preflight, body size, bearer, owner selection, MCP session) are the same
problem documents, identical on `/mcp`, `/v1` and host-mounted routes; their
`type`s are in
[`problem.rs`](https://github.com/Aquilo-Solution-S/proxiima/blob/main/crates/proxiima-server/src/problem.rs).
The listener refuses an over-limit declared `Content-Length` before
authentication and bounds streamed bodies as the route reads them.
A `401` also carries its `WWW-Authenticate` challenge. Under `/v1/`, a path
no route matches is a `404` problem and a method a route does not serve a
`405` problem with `Allow`; paths outside `/v1/` and `/mcp` keep the host's
own fallback.

| Failure class | HTTP status |
|---|---|
| Missing or invalid bearer | 401 |
| Insufficient authority; refused Host, Origin, or owner selection | 403 |
| Missing or invisible entity / route; unknown or expired MCP session | 404 |
| Invalid input or unknown schema; malformed Host or CORS preflight | 400 |
| Tool input requires unchanged admission (`input-not-admitted`) | 400 |
| Idempotency, storage, or suppression conflict | 409 |
| Request body over the listener's limit | 413 |
| Layering violation | 422 |
| Tool failure (`tool-failure`, the tool's text) | 422 |
| Missing capability or publication capacity exhausted | 503 |
| Another store did not answer (`store-unavailable`, retryable) | 503 |
| MCP session limit reached (`session-limit-reached`, retryable) | 503 |
| Internal/deployment failure | 500 |

A memory on another store that the store did not answer for (it failed, timed
out, or answered for a different memory) is never reported as missing: nothing
is known about whether it exists. A single read (`proxiima://memory/{id}`,
derive sources, interpret subjects) fails with JSON-RPC `-32000` and `error.data`
`{"code": "store_unavailable", "store": N}` (REST `503`, type
`store-unavailable`); retry it. The batch read `proxiima://memories` lists such
ids in `unanswered`, beside `missing` (an answered not-found, erased, or
unknown store). Lineage keeps such an origin as a node with status
`unavailable` and an `unavailable_reason`: `not readable`, `store
unavailable`, `deadline exceeded`, or `origin limit exceeded`. The edge to another
store's memory carries its recorded kind: `origin` for a derivation's source,
`reference` for an interpretation's subject, `read` for a memory the host says
the session read (descendant walks from that memory cannot see it). Lineage ancestors and
`core_think` ancestors follow one rule: declared origins (`origin`), declared
payload subjects such as an interpretation's (`reference`), and other stores'
memories as leaves. A lineage page holds at most `limit` edges, other-store
ones included; in `core_think` such a memory is a leaf visit with `store`, its
`edge` kind, and the same `status`/`unavailable_reason` when it could not be
read.

Goal evidence (`core_goal` set, modify, decompose children, `mark_achieved`)
may name a memory on another store. The write reads it there as the caller
first, so an unreadable memory is `not_found` and an unanswering store is
`store_unavailable`, both before any write; nothing of the memory is copied.
A modify that omits evidence keeps a remote id as written. `proxiima://goals`
lists each version's `evidence`. `proxiima://memory/{id}` reads the version
named (`version=exact`, the default); `?version=head` returns the newest
version of its series the caller may read, from this store or from the store
that issued the UUID, and the output's `handle` and `series` say which version
and series that is.

Operation-specific schema and error variants remain in code. Retry only when
the operation and its idempotency contract allow it; a transport disconnect
does not prove that a write rolled back.
