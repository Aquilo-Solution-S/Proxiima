# Causa: A Formal Epistemology for Agent Memory

Proxiima implements the epistemological loop described in
[*Proxima — Facts, Goals, and the Boundary of Life*](https://aquilo-solutions.com/articles/proxima)
and visualized at [aquilo-solutions.com](https://aquilo-solutions.com):

- **Facts** are observer-bound, immutable records of what reached the system.
  They are never rewritten in place; later observations add new Facts.
- **Goals** are desired end states — directed local reversal of entropy — that
  give observations a measure and motivate action.
- **Abstractions** extract grounded patterns from Facts (and prior Abstractions)
  with traceable provenance.
- **Perspectives** frame how observations and goals relate and how future
  abstractions form, without altering the Facts beneath them.
- **Reflective observation vs. terminal closure:** a Fact may observe a Goal
  so admitted derivations chained through that Fact ground in Facts while Goal
  intent stays outside Memory origins
  (`Causa.principle_9_reflective_goal_observation_grounds`), distinct from
  terminal Goal closure (`Causa.RetainedGoalTerminalCloseFactValid`; see
  [Concepts](concepts.md)).

**Causa** is the Lean 4 proof library under
[`lean/Causa/`](https://github.com/Aquilo-Solution-S/proxiima/tree/main/lean/Causa).
Its theorems are machine-checked with zero `sorry` and audited by
[Tesla Valve](https://github.com/Aquilo-Solution-S/tesla-valve) so only Lean's
standard axioms (`propext`, `Quot.sound`, `Classical.choice`) may appear.

## Authoritative formal artifacts

Rather than copying theorem statements into prose, this page links directly to
the checked sources:

- **Nine principles:**
  [`lean/Causa/Principles.lean`](https://github.com/Aquilo-Solution-S/proxiima/blob/main/lean/Causa/Principles.lean)
  (`Causa.principle_1_fact_has_no_origins` through
  `Causa.principle_9_reflective_goal_observation_grounds`).
- **Provenance and retained-store validity:**
  [`lean/Causa/Provenance.lean`](https://github.com/Aquilo-Solution-S/proxiima/blob/main/lean/Causa/Provenance.lean)
  (`Causa.GroundsInFact`, `Causa.GroundsThroughFact`,
  `Causa.RetainedCommittedStoreValid`).
- **Goal-forest invariants and situated retrieval:**
  [`lean/Causa/Goals.lean`](https://github.com/Aquilo-Solution-S/proxiima/blob/main/lean/Causa/Goals.lean)
  (`Causa.GoalForestInv`, `Causa.goal_tree_acyclic`,
  `Causa.closed_ancestor_closes_descendant`, `Causa.situatedSelf`).
- **Rendered ledger and per-theorem axiom inventory:** generated from Lean into
  [the ledger](ledger.md).

## Three-layer verification against live Postgres

`Causa` proves the model's internal consistency; runtime behavior lives in Rust,
SQL, triggers, and PostgreSQL row-level security. Proxiima connects the formal
model to the running database through three complementary layers on standard
stable Rust and Lean 4:

1. **Lean 4 proofs and ledger audit (`lean/Causa/` + `lean/Proxiima/`):**
   `lake build` and `lake exe ledger check` check every proof theorem, axiom
   usage, closed decision table, and code link.
2. **Exhaustive closed-decision vectors:**
   `lake exe ledger emit rust` and `lake exe ledger emit vectors` generate the
   Rust `ledger!` constants and exhaustive JSON decision vectors replayed across
   the Rust workspace.
3. **Stateful live-Postgres `Causa` oracle with planted-bug controls
   ([`causa_stateful_oracle.rs`](https://github.com/Aquilo-Solution-S/proxiima/blob/main/crates/storage-pg/tests/causa_stateful_oracle.rs)):**
   - **Planted-bug controls:** 32 controls — one for each `CausaViolation`
     variant — prove `verify_causa_snapshot` reports that violation on a broken
     snapshot (isolated where structurally independent, and paired where
     invariants are mathematically coupled such as cycles vs. birth order).
   - **Stateful live-Postgres trace:** executes deterministic randomized
     interleaved `Engine` + `PgStorage` operations across two owners
     (Memory writes, reflective Goal observations and derivations, revisions,
     forks, soft-forgets, restores, Memory hard erases, Goal lifecycle and
     decomposition, and guarded write refusals) and verifies the stored Memory
     graph, Goal-forest, and trace-history invariants
     (`Causa.RetainedCommittedStoreValid`, `Causa.GoalForestInv`,
     `Causa.ForkAnchorAligned`, `Causa.MemoryHeadAligned`, and Principles 1, 2,
     4, 5, 6, 7, 8, and 9) on live PostgreSQL tables after every setup and trace
     step. Role-resolution access rules
     (`Causa.principle_access_is_one_resolved_role`) and Goal hard-erasure paths
     are verified separately by the owner-authority, RLS, and erase test suites.

## Running the Causa verification stack locally

Configure `PROXIIMA_TEST_PG_URL` for your disposable PostgreSQL server as
described in [local verification](contributing.md#local-verification), then run:

```sh
# 1. Lean 4 proofs + Tesla Valve ledger audit
(cd lean && lake build && lake exe ledger check)

# 2. Planted-bug controls (fast unit test, no Postgres required)
cargo test --locked -p proxiima-storage-pg --test causa_stateful_oracle causa_oracle_planted_bug_controls

# 3. Live-Postgres stateful Causa trace (raise PROXIIMA_CAUSA_STEPS for deep sweeps)
PROXIIMA_CAUSA_STEPS=1000 \
  cargo test --locked -p proxiima-storage-pg --test causa_stateful_oracle
```
