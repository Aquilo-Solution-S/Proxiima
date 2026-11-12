/-
Causa — Compliance

Hard delete is abandonment, the forgotten path, or a spiice-scoped erase:

  wipeable := abandoned ∨ (forgotten ∧ unreferenced ∧ policy) ∨ spiiceScoped

Forget sets a flag on the row and keeps it (#488); erase wipes abandoned owners; a spiice-scoped erase wipes whole
series of one spiice's own schemas under one live owner, on Admin or the
host's system authority (renegotiated in writing, docs/13 §Spiice-scoped
erase). An owner-to-owner transfer carries
the entity's erase reach with it: after the move it is the DESTINATION owner
whose abandonment wipes the row. There is no edge cascade — pins live on the
declaring row.
-/

import Causa.Prelude
import Causa.Owner
import Causa.Memory
import Causa.Authorization
import Causa.EdgeAuthorization

namespace Causa

def abandoned (o : Owner) : Prop := ∀ u : User, o u = none

/-- No stored row, live or forgotten, still pins this `t`. -/
def unreferenced (memories : Set Memory) (id : MemoryId) : Prop :=
  ∀ m : Memory, m ∈ memories →
    id ∉ memory_origins m ∧ id ∉ memory_reference_ids m

/-- No remaining admission names this Content. A forgotten row is still an
    admission and keeps its payload; Content GC is admission-set emptiness. -/
def contentUnreferenced (memories : Set Memory) (id : ContentId) : Prop :=
  ∀ m : Memory, m ∈ memories → memory_content_id m ≠ some id

def contentWipeable (o : Owner) (memories : Set Memory) (id : ContentId) : Prop :=
  abandoned o ∨ contentUnreferenced memories id

/-- The row `id` is stored and flagged forgotten. -/
def forgotten (memories : Set Memory) (id : MemoryId) : Prop :=
  ∃ m : Memory, m ∈ memories ∧ memory_t m = id ∧ memory_forgotten m = true

-- ============================================================
-- Spiice-scoped erase (docs/13 §Spiice-scoped erase)
-- ============================================================

/-- Who issues a spiice-scoped erase: a user, or the host's system authority
    (a retention schedule runs with nobody in the loop). -/
inductive EraseAuthority where
  | user (u : User)
  | system

/-- Administration of the owner, the gate a transfer takes, or the host's
    system authority. -/
def EraseAuthority.admits : EraseAuthority → Owner → Prop
  | .user u, o => may_administer u o
  | .system, _ => True

/-- One spiice-scoped erase: whole series (`handles`), one live owner, and
    only the named spiice's own schemas. The spiice's vocabulary is a
    PARAMETER (`ownSchemas`), as in `Causa.Spiice`; a series carries one
    schema (`seriesSchema`: a new shape is a new handle, Identity). Nothing
    here requires the owner to be abandoned. -/
structure SpiiceScopedErase (memories : Set Memory) where
  owner        : Owner
  ownSchemas   : SchemaRef → Prop
  seriesSchema : Handle → SchemaRef
  handles      : Set Handle
  authority    : EraseAuthority
  authorized   : authority.admits owner
  ownSchema    : ∀ h : Handle, h ∈ handles → ownSchemas (seriesSchema h)
  /-- Every version of a selected series, live or forgotten, is the owner's. -/
  inOwner      : ∀ m : Memory, m ∈ memories → memory_handle m ∈ handles →
    memory_owner m = owner

/-- A `t` the erase selects: any version, live or forgotten, of a selected
    series. -/
def SpiiceScopedErase.selects {memories : Set Memory}
    (e : SpiiceScopedErase memories) (id : MemoryId) : Prop :=
  ∃ m : Memory, m ∈ memories ∧ memory_t m = id ∧ memory_handle m ∈ e.handles

/-- CO-7 / ST-13: abandonment OR (forgotten ∧ unreferenced ∧ policy) OR
    selected by a spiice-scoped erase of that owner. -/
def wipeable (o : Owner) (memories : Set Memory)
    (id : MemoryId) (policy : Prop) : Prop :=
  abandoned o ∨ (forgotten memories id ∧ unreferenced memories id ∧ policy) ∨
    ∃ e : SpiiceScopedErase memories, e.owner = o ∧ e.selects id

/-- Target render: live / forgotten / unavailable. A pin to a forgotten `t`
    renders forgotten, not a missing origin. -/
def pin_render_live (memories : Set Memory) (id : MemoryId) : Prop :=
  ∃ m : Memory, m ∈ memories ∧ memory_t m = id ∧ memory_forgotten m = false

def pin_render_forgotten (memories : Set Memory) (id : MemoryId) : Prop :=
  forgotten memories id

def pin_render_unavailable (memories : Set Memory) (id : MemoryId) : Prop :=
  ¬ pinExists memories id

theorem pin_render_live_not_forgotten
    (memories : Set Memory) (hunique : MemoryIdUnique memories) (id : MemoryId)
    (hl : pin_render_live memories id) : ¬ pin_render_forgotten memories id := by
  obtain ⟨m, hm, ht, hf⟩ := hl
  rintro ⟨m', hm', ht', hf'⟩
  have heq := hunique m m' hm hm' (ht.trans ht'.symm)
  subst heq
  simp [hf] at hf'

-- ============================================================
-- Modeled hard-erase transitions
-- ============================================================

/-- Remove one Memory admission by its immutable `t`.  The model does not
    encode SQL lock order or trigger sequencing; it only describes the
    committed set transform. -/
def hardEraseMemoryRows (memories : Set Memory) (target : Memory) : Set Memory :=
  fun m => m ∈ memories ∧ memory_t m ≠ memory_t target

/-- Add the closed kind witness emitted when a Memory row is erased. -/
def hardEraseMemoryWitnesses
    (targets : Set ErasedPinTarget) (target : Memory) :
    Set ErasedPinTarget :=
  fun e => e ∈ targets ∨
    (erased_pin_target_t e = memory_t target ∧
      erased_pin_target_kind e = (memory_kind target).pinTargetKind)

/-- Pure state relation for a Memory hard erase.  The preconditions model the
    database's uniqueness/disjointness fences, including rejection of an
    already-recorded target with a conflicting kind. -/
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

/-- Forget sets the flag on the target row (#488): every row stays, the
    payload and pins with it, and the hard-erase witness set is untouched. -/
structure ForgetMemoryTransition
    (memories : Set Memory)
    (targets : Set ErasedPinTarget) (target : Memory)
    (memories' : Set Memory)
    (targets' : Set ErasedPinTarget) : Prop where
  targetInMemories : target ∈ memories
  witnessIdsUnique : ErasedPinTargetIdUnique targets
  witnessMemoryDisjoint : ErasedTargetsDisjointMemories targets memories
  memoriesEq : memories' = forgetIn memories (memory_t target)
  targetsEq : targets' = targets

/-- A Goal erase records a Goal witness while leaving the Memory set intact. -/
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
