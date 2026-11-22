import Causa.Provenance
import Causa.Audience
import Causa.Compliance

/-! Causa — Principles -/

namespace Causa

/-- Principle 1 — Facts are primitive observations. -/
theorem principle_1_fact_has_no_origins (m : Memory) (hk : memory_kind m = .Fact) :
    memory_origins m = [] :=
  m.fact_origins_empty hk

/-- Principle 2 — The layering and grounding of derived Memory. -/
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

/-- Principle 3 — Owner and audience access through one resolved role. -/
theorem principle_access_is_one_resolved_role
    (s : OwnerState) (o : Owner) (gs : List Grant) (u : User) (c : Capability) :
    (flattened s o gs).gives u c ↔ o.gives u c ∨ ∃ g, g ∈ gs ∧ g.gives s u c :=
  flattened_gives s o gs u c

/-- Principle 4 — Relationship typing by the declaring field. -/
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

/-- Principle 5 — Stability of pin targets through forgetting and erasure. -/
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

/-- Principle 6 — Preservation of pin resolution through hard erasure. -/
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

/-- Principle 7 — Read context is independent of grounding. -/
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

/-- Principle 8 — Goal intent is outside Memory grounding. -/
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
