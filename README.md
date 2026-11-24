<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/proxiima-seal-dark.svg">
    <img alt="Proxiima" src="docs/assets/proxiima-seal-light.svg" width="128" height="128">
  </picture>
</p>

# Proxiima

**Facts are the foundation of reality. Goals are the foundation of life.**

Proxiima is a typed, owner-scoped durable memory substrate for agentic systems
built around a causal epistemology — **Facts**, **Abstractions**,
**Perspectives**, and **Goals** — with machine-checked invariants in Lean 4.
Read the philosophy in
[*Proxima — Facts, Goals, and the Boundary of Life*](https://aquilo-solutions.com/articles/proxima)
and see the model as a living graph at [aquilo-solutions.com](https://aquilo-solutions.com).

## Why Proxiima

Stateless model calls forget between runs; flat RAG stores collapse observation,
interpretation, and revision into interchangeable chunks. Proxiima separates
them at the storage and type boundary:

- **Facts** record immutable, observer-bound observations.
- **Abstractions** derive patterns grounded in Facts with traceable provenance.
- **Perspectives** hold framing interpretations and supersede without erasing history.
- **Goals** give direction, decompose into acyclic trees, and record terminal
  closure as an observed Fact. A Fact may also observe a Goal so future
  derivations ground through that observation while Goal intent stays outside
  Memory origins (see [Causa](docs/causa.md) and [Concepts](docs/concepts.md)).

A host supplies identity, event sources, model clients, scheduling, and effects;
Proxiima supplies the durable graph, its write laws, and its MCP/REST surface.

## Verification

Proxiima's principles are formalized in Lean 4 under [`lean/Causa/`](lean/Causa)
with zero `sorry`, audited by
[Tesla Valve](https://github.com/Aquilo-Solution-S/tesla-valve), and paired with
exhaustive emitted decision vectors and a live-Postgres stateful `Causa` oracle
with planted-bug controls for every checked violation variant
([`causa_stateful_oracle.rs`](crates/storage-pg/tests/causa_stateful_oracle.rs)).
See [Causa](docs/causa.md) for the architecture and [Ledger](docs/ledger.md) for
the generated rules, tables, and axiom inventory.

## Start here

| Task | Start here |
|---|---|
| Read the Lean 4 epistemology and verification architecture | [Causa](docs/causa.md) |
| Run Postgres, connect an agent via MCP, remember and search | [Get started](docs/get-started.md) |
| Understand identity, groups, roles, and ownership | [Identity and ownership](docs/concepts.md#identity-and-ownership) |
| Understand memory identity, layering, pins, and Goals | [Concepts](docs/concepts.md) |
| Read the generated rules, tables, and proof axioms | [Ledger](docs/ledger.md) |
| Embed the Rust facade | [Host integration](docs/host-integration.md) |
| Build a linked vocabulary crate | [Build a spiice](docs/build-a-spiice.md) |
| Discover actions, resources, and REST | [Protocol](docs/protocol.md) |
| Deploy, back up, and maintain | [Operate](docs/operate.md) |
| Find settings | [Configuration](docs/configuration.md) |
| Contribute or build this site | [Contributing](docs/contributing.md) |

The [guide](docs/index.md), [Causa](docs/causa.md), [ledger](docs/ledger.md),
and [repository guidance](AGENTS.md) are the starting points for contributors.

The canonical host is [proxiima-mcp](apps/proxiima-mcp). See
[`Proxiima.Composition`](docs/ledger.md#composition) for how hosts compose the
registry. Crates are unpublished. Rust consumers pin the same git selector
across the graph.

It serves the core only and links no spiice. A host that wants a spiice links it.

License: [Apache-2.0](LICENSE), [contribution terms](LICENSING.md).
