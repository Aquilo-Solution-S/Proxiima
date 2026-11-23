# Concepts

## The four kinds

The Memory layers and the separate Goal spine are defined in [`Proxiima.Memory`](ledger.md#memory).
Goals are desired end states. See [`Proxiima.Goals`](ledger.md#goals).

Citation is provenance attached to a Fact or Abstraction. Actions and wake
behavior belong to the host integration.

## Identity and ownership

The [`Proxiima.OwnerAuthority`](ledger.md#owner-authority) ledger defines durable ownership and owner roles.
For exact-version publications and grants, see [Goal collaboration](#goal-collaboration).

Follow the [request flow](host-integration.md#authentication-and-owner-scope)
for integration and [Protocol](protocol.md#owner-scope) for transport behavior.
The [decisions](decisions.md#identity-and-ownership-have-separate-jobs) explain
why these boundaries exist.

A person, agent, or service is a principal (`UserId`). An owner is an `OwnerId`;
a group owner is also a `GroupId`. See [`Proxiima.Identity`](ledger.md#identity).

The host binds external credentials to principals and supplies the trusted
owner resolver. Proxiima carries the resulting authority through a sealed
`OwnerScope`, engine command ports, and PostgreSQL row-level security (RLS).
An owner UUID, a group name, a token claim an application interprets, or a
model label is no authorization proof. The stock OIDC adapter verifies the
credential and maps its issuer and subject before owner access is resolved.

### Owners

Personal owners and shared groups follow [`Proxiima.OwnerAuthority`](ledger.md#owner-authority).
Business labels, account provisioning and billing belong to the host.

Choose the owner to match the intended audience. Use a separate group for a body of
knowledge that needs its own readers and maintainers.

### Roles

The role fields, memory and Goal capability axes, and all five presets are in [`Proxiima.OwnerAuthority`](ledger.md#owner-authority).
See [host authority](data-and-erasure.md#host-authority) for erasing a whole owner.

### Granting a role

**On the owner.** Direct relations and inherited roles follow [`Proxiima.OwnerAuthority`](ledger.md#owner-authority).

**On a Goal.** Audience grants follow [`Proxiima.GoalCollaboration`](ledger.md#goal-collaboration).

### Nested groups: access travels upward

Group containment and inherited authority follow [`Proxiima.OwnerAuthority`](ledger.md#owner-authority).
This lets a team read department-wide knowledge while keeping the team's own work within its membership.

### Sharing a set of memories

Whole-series transfers follow [`Proxiima.OwnerAuthority`](ledger.md#owner-authority).
See [ownership and lifecycle](data-and-erasure.md#ownership-and-lifecycle),
[revision and fork](#revision-shared-editing-and-fork), and [Goal collaboration](#goal-collaboration).

### Requests, sessions, and workers

Request confinement and owner selection follow [`Proxiima.OwnerAuthority`](ledger.md#owner-authority).
See [owner scope](protocol.md#owner-scope), [token expiry](decisions.md#access-ends-at-token-expiry),
and [granting a role](#granting-a-role) for live Goal routes.

A tool call passes three independent gates: the deployment serves the tool
(`ServedSurface`), the caller's scope permits the exact key (`ToolScope`), and
the role permits the effect on this owner and kind. Allowing a tool does not
make a viewer an editor, and an admin cannot call a command its scope excludes.
Listing and invocation read the same catalog declaration. See [capability admission](host-integration.md#capability-admission).

| Mechanism | Purpose | Boundary |
|---|---|---|
| MCP transport session | [Listener admission](ledger.md#transport) | [Credential and session agreement](ledger.md#authentication) |
| Writer `via` | [Writer attribution](ledger.md#identity) | The authenticator supplies it independently of the MCP transport session |
| Delegation grant and redeemed phase | Let queued work run one bounded command | One owner and command, finite expiry, recorded role ceiling, fresh checks at redemption |

Delegated workers and credential-verified forwarding follow
[`Proxiima.Authentication`](ledger.md#authentication). See
[delegated work](host-integration.md#delegated-work) and
[forwarded owner access](host-integration.md#forwarded-owner-access) for host integration.

### Where the guarantees live

| Boundary | Evidence |
|---|---|
| Owner roles, personal membership, and capped nesting | [`Proxiima.OwnerAuthority`](ledger.md#owner-authority) and the [Causa proofs](https://github.com/Aquilo-Solution-S/proxiima/blob/main/lean/Causa/Owner.lean) |
| Verified identity and narrowing scope | [AuthzContext](https://github.com/Aquilo-Solution-S/proxiima/blob/main/crates/core/src/authz.rs) and [OwnerScope](https://github.com/Aquilo-Solution-S/proxiima/blob/main/crates/core/src/owner_scope.rs) |
| Effective memberships and transaction isolation | [Nested-group tests](https://github.com/Aquilo-Solution-S/proxiima/blob/main/crates/storage-pg/tests/nested_groups.rs) and [manager race tests](https://github.com/Aquilo-Solution-S/proxiima/blob/main/crates/storage-pg/src/access/group_manager_race_pg_tests.rs) |
| Per-owner database isolation | [`Proxiima.DatabaseAuthority`](ledger.md#database-authority), [Owner RLS](https://github.com/Aquilo-Solution-S/proxiima/blob/main/crates/storage-pg/definitions/policies.sql), and [multi-owner host tests](https://github.com/Aquilo-Solution-S/proxiima/blob/main/apps/proxiima-mcp/tests/multi_owner_e2e.rs) |

The Lean kernel states principles; it does not prove that Rust or SQL refines
them. Runtime types, database policies, and executable tests enforce the
implementation. An application still supplies its identity provider, business
membership policy, external effects, and lifecycle obligations.

## Memory identity and content

Admission and supersession follow [`Proxiima.Memory`](ledger.md#memory), with identifiers defined in [`Proxiima.Identity`](ledger.md#identity).

Use the [core rustdoc source](https://github.com/Aquilo-Solution-S/proxiima/tree/main/crates/core/src)
for public types and constructors. Use migrations for table shape.

## Layering

Derivation phases and interpretation subjects follow [`Proxiima.Memory`](ledger.md#memory).

See `Causa.principle_2_derivation_goes_up_and_grounds_in_facts` and `Causa.principle_8_a_goal_never_grounds`.

## Pins

Exact pins and their read projections follow [`Proxiima.Edges`](ledger.md#edges).

## Goals

[`Proxiima.Goals`](ledger.md#goals).

## Goal collaboration

Audience grants, exact publications, and work bindings follow [`Proxiima.GoalCollaboration`](ledger.md#goal-collaboration).

### Facts and versioned knowledge

Memory admission and revision follow [`Proxiima.Memory`](ledger.md#memory).

### Revision, shared editing, and fork

A fork gives an adaptation its own revision history. Use a work binding when teams
intend to edit one shared history. The [host integration guide](host-integration.md)
covers the fork and its citation.

### Work and sharing boundaries

Choose restricted owners for lasting confidentiality boundaries. A published
payload can itself disclose source information.

## Citations

`blob_id` is an optional citation on Facts and Abstractions. Perspectives do
not cite. A citation is bibliographic or artefact provenance, not a graph node,
pin, or proof that the cited material is true. Its storage and ownership
contract is part of the data and erasure surface.

A Goal publication of a Fact citing an uploaded object captures its completed
upload. The audience reads that object version through the exact publication;
a later upload does not silently change the shared file or its metadata.
Revoking the publication removes this route as well.

## Self

Self is a cue-dependent retrieval concept, modeled by `Causa.situatedSelf`.
The query result changes with the owner and cue. It is not a row, an
authorization carrier, a parameterless global projection, or a cached causal
chain.

## Kernel boundary

The Lean kernel states these principles. Code and tests establish runtime
behavior; they are not inferred from prose. Start with
[Principles.lean](https://github.com/Aquilo-Solution-S/proxiima/blob/main/lean/Causa/Principles.lean),
[Memory.lean](https://github.com/Aquilo-Solution-S/proxiima/blob/main/lean/Causa/Memory.lean),
[Edges.lean](https://github.com/Aquilo-Solution-S/proxiima/blob/main/lean/Causa/Edges.lean),
and [Goals.lean](https://github.com/Aquilo-Solution-S/proxiima/blob/main/lean/Causa/Goals.lean).
