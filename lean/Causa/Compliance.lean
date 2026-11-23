import Causa.Prelude
import Causa.Owner
import Causa.Memory
import Causa.Authorization
import Causa.EdgeAuthorization

/-! Causa — Compliance -/

namespace Causa

def abandoned (o : Owner) : Prop := ∀ u : User, o u = none

/-- Absence of incoming pins from surviving Memories and Goals. -/
def unreferenced (memories : Set Memory) (goals : Set Goal) (id : MemoryId) : Prop :=
  (∀ m : Memory, m ∈ memories → memory_t m ≠ id →
    id ∉ memory_origins m ∧ id ∉ memory_reference_ids m ∧ id ∉ memory_reads m ∧
    id ∉ memory_goal_refs m ∧ id ∉ memory_guided_by m) ∧
  (∀ g : Goal, g ∈ goals →
    goal_assignment g ≠ some id ∧ id ∉ goal_evidence g ∧
    id ∉ goal_motivated_by g ∧ goal_close_fact_t g ≠ some id ∧ goal_write_act_t g ≠ some id)

/-- Whether a stored Memory is flagged as forgotten. -/
def forgotten (memories : Set Memory) (id : MemoryId) : Prop :=
  ∃ m : Memory, m ∈ memories ∧ memory_t m = id ∧ memory_forgotten m = true

/-- The principal issuing a spiice-scoped erase. -/
inductive EraseAuthority where
  | user (u : User)
  | system

/-- Authorization to issue a spiice-scoped erase for an owner. -/
def EraseAuthority.admits : EraseAuthority → Owner → Prop
  | .user u, o => may_administer u o
  | .system, _ => True

/-- An authorized selection of series for spiice-scoped erasure. -/
structure SpiiceScopedErase (memories : Set Memory) where
  owner        : Owner
  ownSchemas   : SchemaRef → Prop
  seriesSchema : Handle → SchemaRef
  handles      : Set Handle
  authority    : EraseAuthority
  authorized   : authority.admits owner
  ownSchema    : ∀ h : Handle, h ∈ handles → ownSchemas (seriesSchema h)
  /-- Ownership of every stored version in the erase selection. -/
  inOwner      : ∀ m : Memory, m ∈ memories → memory_handle m ∈ handles →
    memory_owner m = owner

/-- Whether the erase selection covers a stored version. -/
def SpiiceScopedErase.selects {memories : Set Memory}
    (e : SpiiceScopedErase memories) (id : MemoryId) : Prop :=
  ∃ m : Memory, m ∈ memories ∧ memory_t m = id ∧ memory_handle m ∈ e.handles

/-- Eligibility for hard erasure. -/
def wipeable (o : Owner) (memories : Set Memory)
    (goals : Set Goal) (id : MemoryId) (policy : Prop) : Prop :=
  abandoned o ∨ (forgotten memories id ∧ unreferenced memories goals id ∧ policy) ∨
    ∃ e : SpiiceScopedErase memories, e.owner = o ∧ e.selects id

/-- Whether a pin resolves to a live Memory for rendering. -/
def pin_render_live (memories : Set Memory) (id : MemoryId) : Prop :=
  ∃ m : Memory, m ∈ memories ∧ memory_t m = id ∧ memory_forgotten m = false

def pin_render_forgotten (memories : Set Memory) (id : MemoryId) : Prop :=
  forgotten memories id

theorem pin_render_live_not_forgotten
    (memories : Set Memory) (hunique : MemoryIdUnique memories) (id : MemoryId)
    (hl : pin_render_live memories id) : ¬ pin_render_forgotten memories id := by
  obtain ⟨m, hm, ht, hf⟩ := hl
  rintro ⟨m', hm', ht', hf'⟩
  have heq := hunique m m' hm hm' (ht.trans ht'.symm)
  subst heq
  simp [hf] at hf'

/-- The committed Memory set after a hard erase. -/
def hardEraseMemoryRows (memories : Set Memory) (target : Memory) : Set Memory :=
  fun m => m ∈ memories ∧ memory_t m ≠ memory_t target

/-- The pin-witness set after a Memory hard erase. -/
def hardEraseMemoryWitnesses
    (targets : Set ErasedPinTarget) (target : Memory) :
    Set ErasedPinTarget :=
  fun e => e ∈ targets ∨
    (erased_pin_target_t e = memory_t target ∧
      erased_pin_target_kind e = (memory_kind target).pinTargetKind)

/-- The committed-state relation for erasing one Memory admission. -/
structure HardEraseMemoryTransition
    (memories : Set Memory)
    (goals : Set Goal) (targets : Set ErasedPinTarget)
    (target : Memory) (memories' : Set Memory)
    (targets' : Set ErasedPinTarget) : Prop where
  targetInMemories : target ∈ memories
  memoryIdsUnique : MemoryIdUnique memories
  witnessIdsUnique : ErasedPinTargetIdUnique targets
  witnessMemoryDisjoint :
    ErasedTargetsDisjointMemories targets (hardEraseMemoryRows memories target)
  witnessGoalDisjoint : ErasedTargetsDisjointGoals targets goals
  targetWitnessCompatible :
    ErasedPinTargetKindCompatible targets (memory_t target)
      (memory_kind target).pinTargetKind
  targetNotGoal : ∀ g : Goal, g ∈ goals → memory_t target ≠ goal_t g
  memoriesEq : memories' = hardEraseMemoryRows memories target
  targetsEq : targets' = hardEraseMemoryWitnesses targets target

theorem hard_erase_memory_records_kind
    (memories : Set Memory) (goals : Set Goal)
    (targets : Set ErasedPinTarget) (target : Memory)
    (memories' : Set Memory) (targets' : Set ErasedPinTarget)
    (htransition : HardEraseMemoryTransition
      memories goals targets target memories' targets') :
    recordedMemoryTargetKind
      targets' (memory_t target) (memory_kind target) := by
  rw [htransition.targetsEq]
  refine ⟨{ t := memory_t target, kind := (memory_kind target).pinTargetKind }, ?_, rfl, rfl⟩
  exact Or.inr ⟨rfl, rfl⟩

theorem hard_erase_memory_result_well_formed
    (memories : Set Memory) (goals : Set Goal)
    (targets : Set ErasedPinTarget) (target : Memory)
    (memories' : Set Memory) (targets' : Set ErasedPinTarget)
    (htransition : HardEraseMemoryTransition
      memories goals targets target memories' targets') :
    ErasedPinTargetIdUnique targets' ∧
    ErasedTargetsDisjointMemories targets' memories' ∧
    ErasedTargetsDisjointGoals targets' goals := by
  rw [htransition.targetsEq, htransition.memoriesEq]
  constructor
  · intro e1 e2 h1 h2 hid
    change e1 ∈ targets ∨
      (erased_pin_target_t e1 = memory_t target ∧
        erased_pin_target_kind e1 = (memory_kind target).pinTargetKind) at h1
    change e2 ∈ targets ∨
      (erased_pin_target_t e2 = memory_t target ∧
        erased_pin_target_kind e2 = (memory_kind target).pinTargetKind) at h2
    cases h1 with
    | inl h1old =>
        cases h2 with
        | inl h2old => exact htransition.witnessIdsUnique e1 e2 h1old h2old hid
        | inr h2new =>
            exact erased_pin_target_ext e1 e2 hid
              ((htransition.targetWitnessCompatible e1 h1old (hid.trans h2new.1)).trans
                h2new.2.symm)
    | inr h1new =>
        cases h2 with
        | inl h2old =>
            exact erased_pin_target_ext e1 e2 hid
              (h1new.2.trans
                (htransition.targetWitnessCompatible e2 h2old
                  (hid.symm.trans h1new.1)).symm)
        | inr h2new =>
            exact erased_pin_target_ext e1 e2 (h1new.1.trans h2new.1.symm)
              (h1new.2.trans h2new.2.symm)
  constructor
  · intro e he m hm
    change e ∈ targets ∨
      (erased_pin_target_t e = memory_t target ∧
        erased_pin_target_kind e = (memory_kind target).pinTargetKind) at he
    change m ∈ memories ∧ memory_t m ≠ memory_t target at hm
    cases he with
    | inl hold =>
        exact htransition.witnessMemoryDisjoint e hold m ⟨hm.1, hm.2⟩
    | inr hnew =>
        intro same
        exact hm.2 ((hnew.1.symm.trans same).symm)
  · intro e he g hg
    change e ∈ targets ∨
      (erased_pin_target_t e = memory_t target ∧
        erased_pin_target_kind e = (memory_kind target).pinTargetKind) at he
    cases he with
    | inl hold => exact htransition.witnessGoalDisjoint e hold g hg
    | inr hnew =>
        exact fun same => htransition.targetNotGoal g hg (hnew.1.symm.trans same)

/-- The committed Goal set after a hard erase. -/
def hardEraseGoalRows (goals : Set Goal) (target : Goal) : Set Goal :=
  fun g => g ∈ goals ∧ goal_t g ≠ goal_t target

def hardEraseGoalWitnesses
    (targets : Set ErasedPinTarget) (target : Goal) : Set ErasedPinTarget :=
  fun e => e ∈ targets ∨
    (erased_pin_target_t e = goal_t target ∧ erased_pin_target_kind e = .Goal)

structure HardEraseGoalTransition
    (memories : Set Memory) (goals : Set Goal)
    (targets : Set ErasedPinTarget) (target : Goal)
    (memories' : Set Memory) (goals' : Set Goal)
    (targets' : Set ErasedPinTarget) : Prop where
  targetInGoals : target ∈ goals
  witnessIdsUnique : ErasedPinTargetIdUnique targets
  witnessMemoryDisjoint : ErasedTargetsDisjointMemories targets memories
  witnessGoalDisjoint :
    ErasedTargetsDisjointGoals targets (hardEraseGoalRows goals target)
  targetWitnessCompatible :
    ErasedPinTargetKindCompatible targets (goal_t target) .Goal
  targetNotMemory : ∀ m : Memory, m ∈ memories → goal_t target ≠ memory_t m
  memoriesEq : memories' = memories
  goalsEq : goals' = hardEraseGoalRows goals target
  targetsEq : targets' = hardEraseGoalWitnesses targets target

theorem hard_erase_goal_records_kind
    (memories : Set Memory) (goals : Set Goal)
    (targets : Set ErasedPinTarget) (target : Goal)
    (memories' : Set Memory) (goals' : Set Goal)
    (targets' : Set ErasedPinTarget)
    (htransition : HardEraseGoalTransition
      memories goals targets target memories' goals' targets') :
    recordedGoalTargetExists targets' (goal_t target) := by
  rw [htransition.targetsEq]
  refine ⟨{ t := goal_t target, kind := .Goal }, ?_, rfl, rfl⟩
  exact Or.inr ⟨rfl, rfl⟩

theorem hard_erase_goal_result_well_formed
    (memories : Set Memory) (goals : Set Goal)
    (targets : Set ErasedPinTarget) (target : Goal)
    (memories' : Set Memory) (goals' : Set Goal)
    (targets' : Set ErasedPinTarget)
    (htransition : HardEraseGoalTransition
      memories goals targets target memories' goals' targets') :
    ErasedPinTargetIdUnique targets' ∧
    ErasedTargetsDisjointMemories targets' memories' ∧
    ErasedTargetsDisjointGoals targets' goals' := by
  rw [htransition.targetsEq, htransition.memoriesEq, htransition.goalsEq]
  constructor
  · intro e1 e2 h1 h2 hid
    change e1 ∈ targets ∨
      (erased_pin_target_t e1 = goal_t target ∧ erased_pin_target_kind e1 = .Goal) at h1
    change e2 ∈ targets ∨
      (erased_pin_target_t e2 = goal_t target ∧ erased_pin_target_kind e2 = .Goal) at h2
    cases h1 with
    | inl h1old =>
        cases h2 with
        | inl h2old => exact htransition.witnessIdsUnique e1 e2 h1old h2old hid
        | inr h2new =>
            exact erased_pin_target_ext e1 e2 hid
              ((htransition.targetWitnessCompatible e1 h1old (hid.trans h2new.1)).trans
                h2new.2.symm)
    | inr h1new =>
        cases h2 with
        | inl h2old =>
            exact erased_pin_target_ext e1 e2 hid
              (h1new.2.trans
                (htransition.targetWitnessCompatible e2 h2old
                  (hid.symm.trans h1new.1)).symm)
        | inr h2new =>
            exact erased_pin_target_ext e1 e2 (h1new.1.trans h2new.1.symm)
              (h1new.2.trans h2new.2.symm)
  constructor
  · intro e he m hm
    change e ∈ targets ∨
      (erased_pin_target_t e = goal_t target ∧ erased_pin_target_kind e = .Goal) at he
    cases he with
    | inl hold => exact htransition.witnessMemoryDisjoint e hold m hm
    | inr hnew =>
        exact fun same => htransition.targetNotMemory m hm (hnew.1.symm.trans same)
  · intro e he g hg
    change e ∈ targets ∨
      (erased_pin_target_t e = goal_t target ∧ erased_pin_target_kind e = .Goal) at he
    change g ∈ goals ∧ goal_t g ≠ goal_t target at hg
    cases he with
    | inl hold =>
        exact htransition.witnessGoalDisjoint e hold g ⟨hg.1, hg.2⟩
    | inr hnew => exact fun same => hg.2 (same.symm.trans hnew.1)

end Causa
