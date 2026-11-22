import Causa.Operators
import Proxiima.Edges

/-! Causa — Provenance grounding -/

namespace Causa

/-- The origins and references that contribute to `Proxiima.Edges.groundingFields`. -/
def groundingFrom (m tgt : Memory) : Prop :=
  memory_t tgt ∈ memory_origins m ∨ memory_t tgt ∈ memory_refs m

/-- The stored pins subject to the graph's insertion order, including fork lineage. -/
def pinFrom (m tgt : Memory) : Prop :=
  groundingFrom m tgt ∨ memory_forked_from m = some (memory_t tgt)

def pinFromInTable (memories : Set Memory) (m tgt : Memory) : Prop :=
  m ∈ memories ∧ tgt ∈ memories ∧ pinFrom m tgt

/-- Validity of the admitted Memory graph. -/
structure MemoryGraphValid
    (memories : Set Memory) (goals : Set Goal)
    (heads : Set MemoryHead) : Prop where
  memoryIdUnique : MemoryIdUnique memories
  goalIdUnique : GoalIdUnique goals
  headAligned : MemoryHeadAligned memories heads
  forkAnchorAligned : ForkAnchorAligned memories
  /-- Target existence for the graph's typed pins. -/
  pinTargetsExist :
    ∀ m : Memory, m ∈ memories →
      (∀ id : MemoryId, id ∈ memory_origins m → pinExists memories id) ∧
      (∀ id : MemoryId, id ∈ memory_refs m → pinExists memories id) ∧
      (∀ id : MemoryId, memory_forked_from m = some id → pinExists memories id) ∧
      (∀ id : GoalId, id ∈ memory_goal_refs m →
        goalReferenceTargetExists goals id)
  motivationsValid :
    ∀ g : Goal, g ∈ goals → goal_motivations_valid memories g
  guidanceValid :
    ∀ m : Memory, m ∈ memories → perspective_guidance_valid memories goals m
  originKind : ∀ m : Memory, m ∈ memories → OriginKindValid memories m
  /-- Memory and Goal version IDs are disjoint, per `Proxiima.Identity.admissionIdAvailable`. -/
  memoryGoalIdsDisjoint :
    ∀ (m : Memory) (g : Goal), m ∈ memories → g ∈ goals → memory_t m ≠ goal_t g
  /-- The origin requirement for an Abstraction. -/
  abstractionHasOrigins :
    ∀ m : Memory, m ∈ memories → memory_kind m = .Abstraction →
      memory_origins m ≠ []
  /-- The provenance requirement for derived Memory. -/
  derivedProvenance :
    ∀ m : Memory, m ∈ memories → memory_kind m ≠ .Fact →
      memory_origins m ≠ [] ∨ memory_refs m ≠ []
  /-- Stored support under `Proxiima.Edges.groundingFields`. -/
  groundingSupport :
    ∀ m : Memory, m ∈ memories → memory_kind m ≠ .Fact →
      ∃ tgt : Memory, tgt ∈ memories ∧ groundingFrom m tgt
  /-- The insertion order of stored pin targets. -/
  pinTimeStrict :
    ∀ m tgt : Memory, m ∈ memories → tgt ∈ memories →
      pinFrom m tgt → memory_tick tgt < memory_tick m

/-- Validity of the retained Memory graph after hard erasure. -/
structure RetainedMemoryGraphValid
    (memories : Set Memory) (goals : Set Goal)
    (heads : Set MemoryHead)
    (targets : Set ErasedPinTarget) : Prop where
  memoryIdUnique : MemoryIdUnique memories
  goalIdUnique : GoalIdUnique goals
  erasedPinIdUnique : ErasedPinTargetIdUnique targets
  headAligned : MemoryHeadAligned memories heads
  forkAnchorAligned : ForkAnchorAligned memories
  pinTargetsExist :
    ∀ m : Memory, m ∈ memories →
      (∀ id : MemoryId, id ∈ memory_origins m →
        retainedOriginTargetExists memories targets id) ∧
      (∀ id : MemoryId, id ∈ memory_refs m →
        retainedMemoryReferenceTargetExists memories targets id) ∧
      (∀ id : MemoryId, memory_forked_from m = some id →
        retainedMemoryReferenceTargetExists memories targets id) ∧
      (∀ id : GoalId, id ∈ memory_goal_refs m →
        retainedGoalReferenceTargetExists goals targets id)
  motivationsValid :
    ∀ g : Goal, g ∈ goals → goal_motivations_retained_valid memories goals targets g
  guidanceValid :
    ∀ m : Memory, m ∈ memories → perspective_guidance_retained_valid memories goals targets m
  originKind :
    ∀ m : Memory, m ∈ memories →
      RetainedOriginKindValid memories targets m
  /-- Retained Memory and Goal version IDs are disjoint, per `Proxiima.Identity.admissionIdAvailable`. -/
  memoryGoalIdsDisjoint :
    ∀ (m : Memory) (g : Goal), m ∈ memories → g ∈ goals → memory_t m ≠ goal_t g
  erasedMemoryDisjoint : ErasedTargetsDisjointMemories targets memories
  erasedGoalDisjoint : ErasedTargetsDisjointGoals targets goals
  /-- The origin requirement for a retained Abstraction. -/
  abstractionHasOrigins :
    ∀ m : Memory, m ∈ memories → memory_kind m = .Abstraction →
      memory_origins m ≠ []
  /-- The provenance requirement for retained derived Memory. -/
  derivedProvenance :
    ∀ m : Memory, m ∈ memories → memory_kind m ≠ .Fact →
      memory_origins m ≠ [] ∨ memory_refs m ≠ []
  /-- The insertion order of retained stored pin targets. -/
  pinTimeStrict :
    ∀ m tgt : Memory, m ∈ memories → tgt ∈ memories →
      pinFrom m tgt → memory_tick tgt < memory_tick m

/-- Validity of the retained Memory graph and its Goal pins. -/
structure RetainedCommittedStoreValid
    (memories : Set Memory) (goals : Set Goal)
    (heads : Set MemoryHead)
    (targets : Set ErasedPinTarget) : Prop where
  graph : RetainedMemoryGraphValid memories goals heads targets
  assignments : RetainedGoalAssignmentValid goals memories targets
  evidence : RetainedGoalEvidenceValid goals memories targets
  terminalClose : RetainedGoalTerminalCloseFactValid goals memories targets

theorem grounding_wf
    (memories : Set Memory) (goals : Set Goal)
    (heads : Set MemoryHead)
    (hgraph : MemoryGraphValid memories goals heads) :
    WellFounded (fun lo hi => pinFromInTable memories hi lo) :=
  Subrelation.wf
    (fun {a b} h => hgraph.pinTimeStrict b a h.1 h.2.1 h.2.2)
    (invImage memory_tick Nat.lt_wfRel).wf

inductive GroundsInFact (memories : Set Memory) : Memory → Prop where
  | fact {m} : memory_kind m = .Fact → GroundsInFact memories m
  | step {m tgt} : groundingFrom m tgt →
      GroundsInFact memories tgt → GroundsInFact memories m

theorem memory_grounds_in_facts
    (memories : Set Memory) (goals : Set Goal)
    (heads : Set MemoryHead)
    (hgraph : MemoryGraphValid memories goals heads) :
    ∀ m : Memory, m ∈ memories → GroundsInFact memories m := by
  intro m
  refine (grounding_wf memories goals heads hgraph).induction
    (C := fun m => m ∈ memories → GroundsInFact memories m) m ?_
  intro m ih hm
  by_cases hfact : memory_kind m = .Fact
  · exact GroundsInFact.fact hfact
  · obtain ⟨tgt, htgt, hfrom⟩ := hgraph.groundingSupport m hm hfact
    have htable : pinFromInTable memories m tgt := ⟨hm, htgt, Or.inl hfrom⟩
    exact GroundsInFact.step hfrom (ih tgt htable htgt)

theorem perspective_has_provenance :
    ∀ memories goals heads,
      MemoryGraphValid memories goals heads →
      ∀ m : Memory, m ∈ memories → memory_kind m = .Perspective →
        memory_origins m ≠ [] ∨ memory_refs m ≠ [] := by
  intro memories goals heads hgraph m hm hk
  have hne : memory_kind m ≠ .Fact := by rw [hk]; intro h; exact (nomatch h)
  exact hgraph.derivedProvenance m hm hne

theorem guidance_does_not_ground
    (memories : Set Memory) (goals : Set Goal)
    (heads : Set MemoryHead)
    (hgraph : MemoryGraphValid memories goals heads)
    (p : Memory) (hp : p ∈ memories)
    (hk : memory_kind p = .Perspective)
    (goal_id : GoalId) (hguided : goal_id ∈ memory_guided_by p) :
    goal_id ∉ memory_origins p := by
  intro horigin
  obtain ⟨g, hg, hgt, _⟩ :=
    hgraph.guidanceValid p hp hk goal_id hguided
  obtain ⟨source, hsource, hst⟩ :=
    (hgraph.pinTargetsExist p hp).1 goal_id horigin
  exact hgraph.memoryGoalIdsDisjoint source g hsource hg (hst.trans hgt.symm)

end Causa
