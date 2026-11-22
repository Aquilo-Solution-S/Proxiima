/-
Causa — Principles

The principles, in order, one theorem each. A principle the kernel can only
assume sits in the assumed-invariants section with the runtime enforcer that
upholds it. Where the kernel states only half of a principle, the doc comment
names the half the runtime alone enforces.
-/

import Causa.Provenance
import Causa.Audience
import Causa.Compliance

namespace Causa

-- ============================================================
-- Assumed invariants
-- ============================================================
-- The kernel assumes these as structure fields; nothing here proves them.
-- The runtime must uphold each one through the enforcer its comment names.

/-- Principle 1: a Fact is an immutable observation with no origins.

    Assumed, not proved: this is the `Memory.fact_origins_empty` field.
    Runtime enforcers: CHECK `memory_fact_origins_chk` (no origins) and
    trigger `memory_append_only` (immutability, which the kernel does not
    state; its function says which columns it freezes and which rewrite a
    schema evolution may make). -/
theorem principle_1_fact_has_no_origins (m : Memory) (hk : memory_kind m = .Fact) :
    memory_origins m = [] :=
  m.fact_origins_empty hk

-- ============================================================
-- Proved principles
-- ============================================================

/-- Principle 2: derivation only goes up, and every memory grounds in Facts.

    In a valid graph every memory is a Fact or reaches one along its origins,
    an Abstraction's origins are Facts or Abstractions, and a Perspective's
    origins are Abstractions or absent. Runtime enforcers: the typed operator
    ports and trigger `memory_pin_checks`. -/
theorem principle_2_derivation_goes_up_and_grounds_in_facts
    (memories : Set Memory) (goals : Set Goal) (heads : Set MemoryHead)
    (hgraph : MemoryGraphValid memories goals heads)
    (m : Memory) (hm : m ∈ memories) :
    GroundsInFact memories m ∧
    (memory_kind m = .Abstraction →
      ∀ id : MemoryId, id ∈ memory_origins m →
        pinKindFactOrAbstraction memories id) ∧
    (memory_kind m = .Perspective →
      memory_origins m = [] ∨
      ∀ id : MemoryId, id ∈ memory_origins m →
        pinKindIs memories id .Abstraction) := by
  have hkind := hgraph.originKind m hm
  exact ⟨memory_grounds_in_facts memories goals heads hgraph m hm,
    hkind.absFactOrAbs, hkind.perspAbsOrEmpty⟩

/-- Principle 3: one owner per series; access is a role resolved for the
    caller, granted on the owner or on a smaller object.

    Access is a role resolved for the caller: what the owner's group gives, or
    what a grant on a smaller object gives through its subject, which may be a
    group. The kernel states the resolution; one owner per series is upheld by
    the runtime alone: a series has one head row (`memory_head_pkey`,
    `goal_head_pkey`) and the head carries its owner. Runtime enforcers of the
    resolution: owner RLS policies (`proxiima_owner_read`,
    `proxiima_owner_write`), `Role`, and the subject resolver
    `resolve_subject_membership`. -/
theorem principle_access_is_one_resolved_role
    (s : OwnerState) (o : Owner) (gs : List Grant) (u : User) (c : Capability) :
    (flattened s o gs).gives u c ↔ o.gives u c ∨ ∃ g, g ∈ gs ∧ g.gives s u c :=
  flattened_gives s o gs u c

/-- Principle 4: a relationship is a named field on the node that declares
    it; the field fixes its kind.

    In a valid graph a Memory reference (`refs`, `forked_from`) resolves on
    the Memory spine and never names a Goal, and a Goal reference
    (`goal_refs`) resolves on the Goal spine. That relationships are fields
    and not rows is the kernel's shape (`derivePins`); the runtime upholds it
    by column shape and `EdgeKind`. -/
theorem principle_4_the_field_fixes_the_kind
    (memories : Set Memory) (goals : Set Goal) (heads : Set MemoryHead)
    (hgraph : MemoryGraphValid memories goals heads)
    (m : Memory) (hm : m ∈ memories) :
    (∀ id : MemoryId, id ∈ memory_reference_ids m → pinExists memories id) ∧
    (∀ g : Goal, g ∈ goals → goal_t g ∉ memory_reference_ids m) ∧
    (∀ id : GoalId, id ∈ memory_goal_refs m → goalReferenceTargetExists goals id) := by
  have hpins := hgraph.pinTargetsExist m hm
  have resolves : ∀ id : MemoryId, id ∈ memory_reference_ids m → pinExists memories id := by
    intro id hid
    cases List.mem_append.1 hid with
    | inl hrefs => exact hpins.2.1 id hrefs
    | inr hfork =>
        cases hf : memory_forked_from m with
        | none => rw [hf] at hfork; cases hfork
        | some f =>
            rw [hf] at hfork
            cases List.mem_singleton.1 hfork
            exact hpins.2.2.1 id hf
  refine ⟨resolves, ?_, fun id hid => hpins.2.2.2 id hid⟩
  intro g hg hin
  obtain ⟨m', hm', ht⟩ := resolves (goal_t g) hin
  exact hgraph.memoryGoalIdsDisjoint m' g hm' hg ht

/-- Principle 5: versions append; nothing is retargeted.

    Forgetting and a Memory hard erase retarget nothing: after a forget every
    row is a row from before with the same `t`, handle and pins, and every row
    left after a Memory hard erase was there before. The kernel does not
    model the write of a version, so that a write appends a new version instead of
    updating one is upheld by the runtime alone: triggers `memory_append_only`
    and `goal_append_only`, whose functions say which columns they freeze and
    which rewrite a schema evolution may make. -/
theorem principle_5_no_transition_retargets_a_pin :
    (∀ (memories : Set Memory) (forgottenId : MemoryId) (m' : Memory),
      m' ∈ forgetIn memories forgottenId →
      ∃ m : Memory, m ∈ memories ∧ memory_t m' = memory_t m ∧
        memory_handle m' = memory_handle m ∧ derivePins m' = derivePins m) ∧
    (∀ (memories : Set Memory) (goals : Set Goal) (targets : Set ErasedPinTarget)
      (target : Memory) (memories' : Set Memory) (targets' : Set ErasedPinTarget),
      HardEraseMemoryTransition memories goals targets target memories' targets' →
      ∀ m' : Memory, m' ∈ memories' → m' ∈ memories ∧ memory_t m' ≠ memory_t target) := by
  refine ⟨?_, ?_⟩
  · intro memories forgottenId m' hm'
    obtain ⟨m, hm, h⟩ := hm'
    rcases h with ⟨_, he⟩ | ⟨_, he⟩ <;> subst he
    · exact ⟨m, hm, rfl, rfl, rfl⟩
    · exact ⟨m', hm, rfl, rfl, rfl⟩
  · intro memories goals targets target memories' targets' htransition m' hm'
    rw [htransition.memoriesEq] at hm'
    exact hm'

/-- Principle 6: erasure is an authorized inverse and leaves no dangling pin.

    A pin that resolved before a hard erase still resolves after it: to the
    surviving row, or to the kinded witness the erase records. This holds for
    Memory pins (origins and references) across a Memory erase and for Goal
    references across a Goal erase. Who may erase is upheld by the runtime
    alone: the erase ports (`OwnerInversePort`, `OwnerEraseAuthorityPort`,
    `WriteSession::erase_series`); the witness is recorded by triggers
    `memory_erased_pin_target` and `goal_erased_pin_target`. -/
theorem principle_6_erasure_leaves_no_dangling_pin :
    (∀ (memories : Set Memory) (goals : Set Goal) (targets : Set ErasedPinTarget)
      (target : Memory) (memories' : Set Memory) (targets' : Set ErasedPinTarget),
      HardEraseMemoryTransition memories goals targets target memories' targets' →
      ∀ id : MemoryId,
        retainedMemoryReferenceTargetExists memories targets id →
        retainedMemoryReferenceTargetExists memories' targets' id) ∧
    (∀ (memories : Set Memory) (goals : Set Goal) (targets : Set ErasedPinTarget)
      (target : Goal) (memories' : Set Memory) (goals' : Set Goal)
      (targets' : Set ErasedPinTarget),
      HardEraseGoalTransition memories goals targets target memories' goals' targets' →
      ∀ id : GoalId,
        retainedGoalReferenceTargetExists goals targets id →
        retainedGoalReferenceTargetExists goals' targets' id) := by
  refine ⟨?_, ?_⟩
  · intro memories goals targets target memories' targets' htransition id h
    rcases h with ⟨m, hm, ht⟩ | ⟨kind, e, he, hid, hk⟩
    · by_cases hmt : memory_t m = memory_t target
      · have hrec := hard_erase_memory_records_kind
          memories goals targets target memories' targets' htransition
        rw [← hmt, ht] at hrec
        exact Or.inr ⟨memory_kind target, hrec⟩
      · rw [htransition.memoriesEq]
        exact Or.inl ⟨m, ⟨hm, hmt⟩, ht⟩
    · rw [htransition.targetsEq]
      exact Or.inr ⟨kind, e, Or.inl he, hid, hk⟩
  · intro memories goals targets target memories' goals' targets' htransition id h
    rcases h with ⟨g, hg, ht⟩ | ⟨e, he, hid, hk⟩
    · by_cases hgt : goal_t g = goal_t target
      · have hrec := hard_erase_goal_records_kind
          memories goals targets target memories' goals' targets' htransition
        rw [← hgt, ht] at hrec
        exact Or.inr hrec
      · rw [htransition.goalsEq]
        exact Or.inl ⟨g, ⟨hg, hgt⟩, ht⟩
    · rw [htransition.targetsEq]
      exact Or.inr ⟨e, Or.inl he, hid, hk⟩

/-- Principle 7: similarity never authors an origin or a reference; a `read`
    pin records an observed read and never grounds.

    Whether a memory grounds in Facts does not depend on its read set: any
    read set may replace it. The kernel has no retrieval, so the similarity
    half is upheld by the runtime alone: retrieval is separate from writes,
    and `ReadSet` is host context, never a tool argument. -/
theorem principle_7_a_read_pin_never_grounds
    (memories : Set Memory) (m : Memory) (reads : List MemoryId) :
    GroundsInFact memories m ↔ GroundsInFact memories { m with reads := reads } := by
  constructor
  · intro h
    cases h with
    | fact hk => exact GroundsInFact.fact hk
    | step hpin htgt => exact GroundsInFact.step hpin htgt
  · intro h
    cases h with
    | fact hk => exact GroundsInFact.fact hk
    | step hpin htgt => exact GroundsInFact.step hpin htgt

/-- Principle 8: a Goal is intent, separate from the three memory kinds; the
    host executes.

    In a valid graph no Goal is an origin of any Memory, so a Goal never
    grounds one; in particular a Goal that guided a Perspective is not among
    its origins. That the host, not core, executes a Goal is upheld by the
    runtime alone: core has no scheduler. -/
theorem principle_8_a_goal_never_grounds
    (memories : Set Memory) (goals : Set Goal) (heads : Set MemoryHead)
    (hgraph : MemoryGraphValid memories goals heads)
    (m : Memory) (hm : m ∈ memories) :
    (∀ g : Goal, g ∈ goals → goal_t g ∉ memory_origins m) ∧
    (memory_kind m = .Perspective →
      ∀ id : GoalId, id ∈ memory_guided_by m → id ∉ memory_origins m) := by
  refine ⟨?_, ?_⟩
  · intro g hg hin
    obtain ⟨m', hm', ht⟩ := (hgraph.pinTargetsExist m hm).1 (goal_t g) hin
    exact hgraph.memoryGoalIdsDisjoint m' g hm' hg ht
  · intro hk id hid
    exact guidance_does_not_ground memories goals heads hgraph m hm hk id hid

end Causa
