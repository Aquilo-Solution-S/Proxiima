# Decisions

This page records durable architectural choices. Release history belongs in
release notes. Implementation details belong in rustdoc, migrations, or the
live catalog.

## Principles and behavior have separate owners

[`Proxiima.Repository`](ledger.md#repository), [the repository's source authority](https://github.com/Aquilo-Solution-S/proxiima/blob/main/AGENTS.md#scope-and-authority), and [Lean principles](https://github.com/Aquilo-Solution-S/proxiima/blob/main/lean/Causa/Principles.lean).

## One owner per fact

See the [repository's source authority](https://github.com/Aquilo-Solution-S/proxiima/blob/main/AGENTS.md#scope-and-authority).

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

Database roles and tenant isolation follow [`Proxiima.DatabaseAuthority`](ledger.md#database-authority).
Use scoped database functions for serving operations whose table policy denies direct owner writes.

See [the SQL contract](host-integration.md#sql-contract) for what a spiice or
host may name in its own SQL.

## Identity and ownership have separate jobs

[`Proxiima.OwnerAuthority`](ledger.md#owner-authority) defines ownership and membership. A private
note stays private when its writer also administers a project group.

### Nesting composes access toward the parent

[`Proxiima.OwnerAuthority`](ledger.md#owner-authority) defines group nesting. A team can read
department-wide knowledge while keeping its own work within the team.

### Durable ownership and Goal collaboration have separate scopes

See [`Proxiima.OwnerAuthority`](ledger.md#owner-authority) for durable ownership and the
[Goal collaboration design](#goal-collaboration-keeps-one-durable-owner) for exact
publications and work. Use a separate group for data that existing owner readers must not see.

### Capabilities and delegation only narrow authority

The served surface, caller capability and owner role are independent gates.
Grant admission and redemption follow [`Proxiima.Authentication`](ledger.md#authentication).

### Access ends at token expiry

Credential and stream deadlines follow [`Proxiima.Authentication`](ledger.md#authentication),
and permit expiry follows [`Proxiima.OwnerAuthority`](ledger.md#owner-authority).
Hosts can shorten the authorization window by issuing short-lived tokens.

### Blob downloads separate authorization from delivery

Authority mode: `existing-kernel-no-change`. The existing kernel owns read
authority through
[`request_may_read`](https://github.com/Aquilo-Solution-S/proxiima/blob/main/lean/Causa/Authorization.lean) and exact Goal-publication
routes through [`goal_publication_read_admitted`](https://github.com/Aquilo-Solution-S/proxiima/blob/main/lean/Causa/EdgeAuthorization.lean).
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

[`Proxiima.Identity`](ledger.md#identity) keeps authorship history independent
of current access. See [requests, sessions, and workers](concepts.md#requests-sessions-and-workers).

## The catalog is the contract

Each capability is one tool action or resource. MCP and REST are two views of
one catalog. Clients discover the current catalog at runtime; documentation
does not become a second schema registry.

The live catalog is described by [Protocol](protocol.md).

## Goals express intent

[`Proxiima.Goals`](ledger.md#goals).

## Pins are node-owned

A pin's kind follows the operation, and the node that declares a relation owns
it. A statement that needs its own meaning is a node, not a new kind of pin.
The kinds are the variants of
[`EdgeKind`](https://github.com/Aquilo-Solution-S/proxiima/blob/main/crates/core/src/edge.rs);
see [Concepts](concepts.md#pins).

## Schema migrations are spiice-defined representation changes

Spiices supply conversion semantics under [`Proxiima.SchemaEvolution`](ledger.md#schema-evolution), and
[`Causa.schema_migration_pins_resolve`](https://github.com/Aquilo-Solution-S/proxiima/blob/main/lean/Causa/SchemaEvolution.lean) proves pin resolution in the retained migration graph.
Replay keys, catalog registration, execution, and recovery remain runtime contracts.

Prior art: Confluent's [data contracts](https://docs.confluent.io/platform/current/schema-registry/fundamentals/data-contracts.html)
attach transformation rules and change metadata to schema versions.
The execution fence uses PostgreSQL's [transaction and advisory locks](https://www.postgresql.org/docs/current/explicit-locking.html)
to serialize source writes and retirement at the database boundary, with completion committed alongside the transformed records.

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

The [Goal collaboration ledger](ledger.md#goal-collaboration) states the
permission contract enforced by the runtime's typed command permits and
owner-scoped storage paths.

Publication consent covers the Goal’s managed audience, including future
invitees. Existing Roles on the Goal's owning Group apply automatically,
alongside explicit Goal grants; their permissions combine. Source-owner and Goal-controller transfers invalidate existing
consent. A cross-owner fork requires explicit source-owner fork permission
and destination write permission.

## PostgreSQL is the store

Production engine construction over PostgreSQL follows [`Proxiima.DatabaseAuthority`](ledger.md#database-authority).
A second backend would be a new design decision.
