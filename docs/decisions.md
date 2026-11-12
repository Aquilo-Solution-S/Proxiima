# Decisions

This page records durable architectural choices. Release history belongs in
release notes. Implementation details belong in rustdoc, migrations, or the
live catalog.

## Principles and behavior have separate owners

The Lean kernel states domainless principles. Rust code and tests are the
behavior. A principle change starts in the kernel, then code and tests follow.
This keeps the kernel meaningful without treating it as an executable claim
about every implementation detail.

Source: [Lean principles](https://github.com/Aquilo-Solution-S/proxiima/blob/main/docs/lean/Causa/Principles.lean).

## One owner per fact

| Fact | Owner |
|---|---|
| Settings | Each crate's `settings!` block; [Configuration](configuration.md) is rendered from them |
| Types, defaults, limits | Rustdoc |
| Tool and resource schemas | Live catalog: `tools/list`, `proxiima://tools`, OpenAPI |
| Tables | SQL migrations |
| Core functions, triggers, policies, views | `crates/storage-pg/definitions/` |
| Invariants | Lean kernel |
| Release changes | Release notes and `Upgrade:` footers |

When a description is needed elsewhere, link to its owner. Do not copy the
table, schema, or default.

## Build-time spiice composition

A spiice is a vocabulary crate linked into a host at build time. A host reads
remote stores through `OtherStores`. This makes composition visible to the
compiler and keeps storage ownership at the host boundary. See [Build a
spiice](build-a-spiice.md) for the current integration surface.

## Trust has three separate statements

Linked code, SQL privileges, and tenant authorization are decided separately.
None of them stands in for another.

Linked code is trusted against malice. A spiice is compiled into the host and
runs in its process, on its database connections. The SDK tier, the typed
services, and the rule that a spiice never queries `proxiima_core` directly
keep honest code on the supported path and keep core free to change its
tables; they do not contain a hostile crate. A host reviews a spiice it links
as it reviews its own code.

SQL privileges are coarse. The runtime role holds `SELECT`, `INSERT`,
`UPDATE`, and `DELETE` on the core tables and on every composed schema, so
table privileges separate no caller from another. Row security does. A table
that serving SQL must not write is hardened on its own: an owner-write policy
that admits no row, and a `SECURITY DEFINER` function owned by the platform
role as its only writer.

Tenant authorization is bound by core. An owner-scoped transaction carries
the owners core resolved for the authenticated caller, and every owner policy
reads them; request input never names them. Platform scope is a role: its
policy is granted to the role that owns the relation, which the serving role
is not.

See [the SQL contract](host-integration.md#sql-contract) for what a spiice or
host may name in its own SQL.

## Identity and ownership have separate jobs

Authentication establishes the principal; ownership selects the data scope.
Sharing uses group membership or explicit transfer rather than entity-side
ACLs. This keeps a private note private even when its writer also administers
a project group.
Personal ownership is derived from identity and its membership cannot be
edited. Organization structure and billing belong to the host.

The [identity and ownership map](concepts.md#identity-and-ownership) explains
the model. The principles are in
[Principles.lean](https://github.com/Aquilo-Solution-S/proxiima/blob/main/docs/lean/Causa/Principles.lean),
with supporting definitions in
[Owner](https://github.com/Aquilo-Solution-S/proxiima/blob/main/docs/lean/Causa/Owner.lean)
and [Authorization](https://github.com/Aquilo-Solution-S/proxiima/blob/main/docs/lean/Causa/Authorization.lean);
runtime owner resolution and command ports enforce the implementation.

### Nesting composes access toward the parent

A subgroup contributes its members to its parent under a capability cap.
Caps weaken each path; independent memberships combine their capabilities.
This lets a team read department-wide knowledge without giving every department
member access to the team's work. The runtime rejects multiple parents and cycles;
the database maintains the effective-role projection. See
[nested groups](concepts.md#nested-groups-access-travels-upward) for the
direction, examples, and executable evidence.

### Durable ownership and Goal collaboration have separate scopes

Nesting composes membership into an organizational tree. A body of knowledge
that needs a different durable owner policy belongs to a separate group, with
direct person memberships when people already sit in other group trees.
Reuse an owner for sets with the same policy. A transfer moves the entire
series and its responsibility. The [Goal collaboration design](#goal-collaboration-keeps-one-durable-owner)
adds selected version publications and delegated work without changing the
owner or the one-parent hierarchy. Goal routes are additive: confidential
data that must be hidden from existing owner readers needs its own owner. See
[sharing a set of memories](concepts.md#sharing-a-set-of-memories) for the
consequences and the enforcing transfer contract.

### Capabilities and delegation only narrow authority

A deployment's served surface and a caller's exact operation keys do not
grant data access. Owner roles remain a separate gate. A delegated grant
additionally binds one owner, one command, a non-managing role ceiling, and
finite expiry. Fresh checks at redemption prevent a queued job from gaining
rights through a later promotion or running after a demotion below its
recorded ceiling. The opaque redeemed phase bounds in-process execution;
revoking the grant prevents future redemption rather than cancelling work
already admitted. See [delegated work](host-integration.md#delegated-work)
for its enforcing types and service.

### Access ends at token expiry

There is no revocation before expiry. A token's access ends when the token
expires; a long-lived stream ends at token expiry or after the maximum stream
lifetime (`PROXIIMA_STREAM_MAX_LIFETIME`), whichever comes first. A delegated
grant ends when it is revoked. The host shortens exposure with short-lived
tokens, not with a revocation signal.

A request context holds its roles and its expiry once, in its owner scope, and
checks expiry in two places: at admission, and again wherever a permit is
minted for a write or a read. An expired context reads, writes and runs
nothing, whichever check asks.

### Blob downloads separate authorization from delivery

Authority mode: `existing-kernel-no-change`. The existing kernel owns read
authority through
[`request_may_read`](lean/Causa/Authorization.lean) and exact Goal-publication
routes through [`goal_publication_read_admitted`](lean/Causa/EdgeAuthorization.lean).
HTTP admission, caching, and transfer lifetime remain runtime contracts.

Each external download request enters Proxiima's existing authorization path
for the authenticated caller, requested operation, and exact cited object.
The host's trusted gateway or CDN transfers bytes from private object storage;
Proxiima need only resolve and authorize the request. The delivery provider
does not define another membership or publication policy. Goal access selects
the publication's captured upload, never a later private upload of the same
bytes.

The gateway obtains a fresh Proxiima decision for every new request, including
cache hits, retries, metadata reads, and ranges. A previous download's
authorization decision is not cached or reused. Clients receive a protected
resource URL, not a reusable storage read capability; private-origin access
prevents bypassing admission. An admitted transfer may finish after revocation.
Later requests re-enter authorization under the caller's existing authority
model; the documented lifetime of an already-redeemed delegated phase is unchanged. This does not promise cancellation
of active streams or recall of bytes already delivered.

A host binds `CitedBlobDeliveryService` to one registered read operation;
its constructor requires the matching boot witness and refuses unknown or
write keys, and each admission checks the
existing capability policy before resolving data authority.
`CitedBlobDeliveryService::authorize_delivery` produces a request-scoped admission;
its private construction and consuming `into_parts` method keep it separate
from serialized tool results. The backend reuses the owner-scoped and exact
Goal-publication queries used by verified reads. Its delivery port requires a
privately constructed permit carrying the checked caller and exact request;
the runtime keeps its concrete store and boot witness on built/running host
handles, outside the shared request context.
Host transport tests must
exercise admission on cache hits, retries and ranges; this is a host integration
obligation because core does not run the delivery gateway. See the
[host integration contract](host-integration.md#blob-download-delivery).

Prior art: NGINX [`auth_request`](https://nginx.org/en/docs/http/ngx_http_auth_request_module.html)
gates delivery on an authorization subrequest. CloudFront's
[external authorization pattern](https://aws.amazon.com/blogs/networking-and-content-delivery/external-server-authorization-with-lambdaedge/)
uses a viewer-request function before cached or origin delivery. These are
delivery options, not required Proxiima dependencies.

### Attribution survives authority changes

Stored `by` and `via` identify the attested writer of each version. They do
not grant access and do not change when ownership transfers. This retains
history without making a former writer a permanent reader. A transport
session, a writer's caller/session UUID, and a delegation grant have distinct
lifecycles. See [requests, sessions, and workers](concepts.md#requests-sessions-and-workers).

## The catalog is the contract

Each capability is one tool action or resource. MCP and REST are two views of
one catalog. Clients discover the current catalog at runtime; documentation
does not become a second schema registry.

The live catalog is described by [Protocol](protocol.md).

## Goals express intent

A Goal owns its desired end-state, owner, state, and dependencies. Scheduling,
wake policy, and execution belong to the host. See [Concepts](concepts.md) and
[Host integration](host-integration.md) for the current boundaries.

### Motivation and guidance are versioned references

A Goal version names the Perspective versions that motivated it. A new
Perspective version names the Goal versions that guided it. The declaring
version owns each statement, and both roles are ordinary Reference pins.
This supports feedback across versions without retargeting history or making
motivation evidence. New causal targets share the source's owner; existing
F/A/P grounding and Fact/Abstraction evidence rules remain separate.

The version fields and valid-store predicates are stated in
[Goals](https://github.com/Aquilo-Solution-S/proxiima/blob/main/docs/lean/Causa/Goals.lean)
and [Provenance](https://github.com/Aquilo-Solution-S/proxiima/blob/main/docs/lean/Causa/Provenance.lean).
Runtime constructors, source-owned pin projections, and database admission
enforce the implementation. Former singular assignments stay on their
original versions as historical data; they are not converted into assertions
of actual influence. See [Goal relationships](concepts.md#goals).

## Pins are node-owned

A pin's kind follows the operation, and the node that declares a relation owns
it. A statement that needs its own meaning is a node, not a new kind of pin.
The kinds are the variants of
[`EdgeKind`](https://github.com/Aquilo-Solution-S/proxiima/blob/main/crates/core/src/edge.rs);
see [Concepts](concepts.md#pins).

## Schema migrations are spiice-defined representation changes

A spiice declares the relation between old and new typed records, including
split and merge cases, and supplies the executable conversion. Core executes
that path before serving the target representation. The Lean theorem states
explicit source coverage, owner preservation, mapped pins, provenance, Goal
constraints, and retained erase-witness obligations, then derives retained
store validity; it does not choose conversion semantics or turn a
representation change into a business event.

A migration may replace admission identities when the spiice supplies the
complete mapping. Captured publication bytes stay unchanged. The internal
publication mapping relates each old capture to all replacement memories;
authorized erase of a replacement removes the associated capture, including
captures retained at a previous owner. This relation grants no read or write
authority and is not an ordinary-read alias.

The contract is [SchemaEvolution](lean/Causa/SchemaEvolution.lean). Replay
keys, catalog registration, migration execution, and recovery remain runtime
contracts and executable tests because they are not kernel fields.

Prior art: Confluent's [data contracts](https://docs.confluent.io/platform/current/schema-registry/fundamentals/data-contracts.html)
attach transformation rules and change metadata to schema versions. Here the
spiice supplies that meaning through typed migrations over persisted records.
The execution fence uses PostgreSQL's [transaction and advisory locks](https://www.postgresql.org/docs/current/explicit-locking.html):
source writes and retirement serialize at the database boundary, and completion
commits with the transformed records.

## Goal collaboration keeps one durable owner

A guest grant that works Goals covers the title and progress of that one
Goal. Creating Goals and changing their decomposition still require owner
authority; a guest grant never becomes owner-wide Goal work.

The target collaboration model keeps each Goal and memory series under one
personal or Group owner. Context, exact-admission publication, and series work bindings
have distinct authority. Goal topology and semantic pins do not grant access.
N:M publication and explicitly delegated N:M editing support overlapping work
without changing ownership. Independent adaptations fork an exact source
version into a new series; later refinements revise that series.

A publication identifies a Fact itself or an exact Abstraction/Perspective
version. Private revisions cannot replace a
previously published admission in the audience's view. Goal access is additive
and cannot restrict existing owner membership. Completion is separate from
revocation and erasure. Source-owner/controller changes must invalidate prior
authorizations without A-to-B-to-A resurrection.

The [collaboration model](concepts.md#goal-collaboration) owns the explanation
and examples. Its permission contract is stated in the kernel and enforced
by the runtime's typed command permits and owner-scoped storage paths.

Publication consent covers the Goal’s managed audience, including future
invitees. Existing Roles on the Goal's owning Group apply automatically,
alongside explicit Goal grants; their permissions combine. Source-owner and Goal-controller transfers invalidate existing
consent. A cross-owner fork requires explicit source-owner fork permission
and destination write permission.

## PostgreSQL is the store

The storage ports have one production implementation, the PostgreSQL backend,
and an engine is always built over storage: `Engine::new` takes the registry
and the `StoragePorts` together, so no engine can start without a store. The
ports that refuse every call exist for tests only and are compiled only under
`cfg(test)` or the `testkit` feature, which a production host does not enable.
A second backend would be a new decision, not a flag on this one.
