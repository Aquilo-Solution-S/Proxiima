import Causa.Provenance
import Proxiima.SchemaEvolution

/-! Causa — Schema evolution -/

namespace Causa

open Proxiima.Edges (PinField)
open Proxiima.SchemaEvolution (GoalPinRole PinCarrier)

/-- Exact targets of each Memory pin field, including context-only fields. -/
def memoryMigrationPins (m : Memory) : PinField → List Id
  | .origins => memory_origins m
  | .refs => memory_refs m
  | .goalRefs => memory_goal_refs m
  | .guidedBy => memory_guided_by m
  | .reads => memory_reads m
  | .forkedFrom => (memory_forked_from m).toList

/-- Exact targets of each Goal pin field. -/
def goalMigrationPins (g : Goal) : GoalPinRole → List Id
  | .assignment => (goal_assignment g).toList
  | .dependency => goal_dependencies g
  | .evidence => goal_evidence g
  | .motivation => goal_motivated_by g
  | .closeFact => (goal_close_fact_t g).toList
  | .writeAct => (goal_write_act_t g).toList

/-- A surviving pin keeps its exact target or uses a declared replacement in the same role. -/
def migrationPinResolved {Record : Type} (oldRecords newRecords : Set Record)
    (identity : Record → Id) (mapping : Record → Record → Prop)
    (old : Id) (targets : List Id) : Prop :=
  old ∈ targets ∨
    ∃ source target, source ∈ oldRecords ∧ target ∈ newRecords ∧
      identity source = old ∧ identity target ∈ targets ∧ mapping source target

/-- A declared conversion and the premises for retained store validity. -/
structure SchemaMigrationWitness where
  oldMemories : Set Memory
  newMemories : Set Memory
  oldGoals : Set Goal
  newGoals : Set Goal
  oldHeads : Set MemoryHead
  newHeads : Set MemoryHead
  /-- Erase witnesses carried intact across the migration. -/
  retainedTargets : Set ErasedPinTarget
  memoryMap : Memory → Memory → Prop
  goalMap : Goal → Goal → Prop
  /-- Every source Memory has a mapped target admission, independently of the erase witnesses. -/
  sourceMemoryCoverage : ∀ old, old ∈ oldMemories →
    ∃ new, new ∈ newMemories ∧ memoryMap old new
  sourceGoalCoverage : ∀ old, old ∈ oldGoals →
    ∃ new, new ∈ newGoals ∧ goalMap old new
  targetMemoryDeclaration : ∀ new, new ∈ newMemories →
    ∃ old, old ∈ oldMemories ∧ memoryMap old new
  targetGoalDeclaration : ∀ new, new ∈ newGoals →
    ∃ old, old ∈ oldGoals ∧ goalMap old new
  mappedMemoryOwner : ∀ old new, memoryMap old new →
    memory_owner old = memory_owner new
  mappedGoalOwner : ∀ old new, goalMap old new →
    goal_owner old = goal_owner new
  memoryCarrier : Memory → PinCarrier
  goalCarrier : Goal → PinCarrier
  /-- Removed carriers have no remaining admission at the old identity. -/
  removedMemoryCarrier : ∀ old, old ∈ oldMemories →
    memoryCarrier old = .removed → ∀ new, new ∈ newMemories → memory_t old ≠ memory_t new
  removedGoalCarrier : ∀ old, old ∈ oldGoals →
    goalCarrier old = .removed → ∀ new, new ∈ newGoals → goal_t old ≠ goal_t new
  /-- A retained carrier accounts for each old pin in its original field on a declared successor. -/
  mappedMemoryPins : ∀ old, old ∈ oldMemories →
    Proxiima.SchemaEvolution.resolveIncomingPins (memoryCarrier old) = true →
    ∀ role id, id ∈ memoryMigrationPins old role →
    ∃ new, new ∈ newMemories ∧ memoryMap old new ∧
      (match role with
       | .goalRefs | .guidedBy =>
         migrationPinResolved oldGoals newGoals goal_t goalMap id (memoryMigrationPins new role)
       | .origins | .refs | .reads | .forkedFrom =>
         migrationPinResolved oldMemories newMemories memory_t memoryMap id (memoryMigrationPins new role))
  mappedGoalPins : ∀ old, old ∈ oldGoals →
    Proxiima.SchemaEvolution.resolveIncomingPins (goalCarrier old) = true →
    ∀ role id, id ∈ goalMigrationPins old role →
    ∃ new, new ∈ newGoals ∧ goalMap old new ∧
      (match role with
       | .dependency =>
         migrationPinResolved oldGoals newGoals goal_t goalMap id (goalMigrationPins new role)
       | .assignment | .evidence | .motivation | .closeFact | .writeAct =>
         migrationPinResolved oldMemories newMemories memory_t memoryMap id (goalMigrationPins new role))
  /-- Read context resolves to a retained Memory target without adding grounding. -/
  retainedReadTargetsExist : ∀ m, m ∈ newMemories → ∀ id, id ∈ memory_reads m →
    retainedMemoryReferenceTargetExists newMemories retainedTargets id
  newMemoryIdUnique : MemoryIdUnique newMemories
  newGoalIdUnique : GoalIdUnique newGoals
  retainedTargetIdUnique : ErasedPinTargetIdUnique retainedTargets
  newHeadAligned : MemoryHeadAligned newMemories newHeads
  newForkAnchorAligned : ForkAnchorAligned newMemories
  retainedPinTargetsExist : ∀ m, m ∈ newMemories →
    (∀ id, id ∈ memory_origins m →
      retainedOriginTargetExists newMemories retainedTargets id) ∧
    (∀ id, id ∈ memory_refs m →
      retainedMemoryReferenceTargetExists newMemories retainedTargets id) ∧
    (∀ id, memory_forked_from m = some id →
      retainedMemoryReferenceTargetExists newMemories retainedTargets id) ∧
    (∀ id, id ∈ memory_goal_refs m →
      retainedGoalReferenceTargetExists newGoals retainedTargets id)
  retainedMotivationsValid : ∀ g, g ∈ newGoals →
    goal_motivations_retained_valid newMemories newGoals retainedTargets g
  retainedGuidanceValid : ∀ m, m ∈ newMemories →
    perspective_guidance_retained_valid newMemories newGoals retainedTargets m
  retainedOriginKind : ∀ m, m ∈ newMemories →
    RetainedOriginKindValid newMemories retainedTargets m
  memoryGoalIdsDisjoint : ∀ m g, m ∈ newMemories → g ∈ newGoals →
    memory_t m ≠ goal_t g
  erasedMemoryDisjoint : ErasedTargetsDisjointMemories retainedTargets newMemories
  erasedGoalDisjoint : ErasedTargetsDisjointGoals retainedTargets newGoals
  abstractionHasOrigins : ∀ m, m ∈ newMemories →
    memory_kind m = .Abstraction → memory_origins m ≠ []
  derivedProvenance : ∀ m, m ∈ newMemories → memory_kind m ≠ .Fact →
    memory_origins m ≠ [] ∨ memory_refs m ≠ []
  pinTimeStrict : ∀ m target, m ∈ newMemories → target ∈ newMemories →
    pinFrom m target → memory_tick target < memory_tick m
  assignmentsValid : RetainedGoalAssignmentValid newGoals newMemories retainedTargets
  evidenceValid : RetainedGoalEvidenceValid newGoals newMemories retainedTargets
  terminalCloseValid :
    RetainedGoalTerminalCloseFactValid newGoals newMemories retainedTargets

theorem schema_migration_validates_retained_store
    (migration : SchemaMigrationWitness) :
    RetainedCommittedStoreValid migration.newMemories migration.newGoals
      migration.newHeads migration.retainedTargets := by
  exact {
    graph := {
      memoryIdUnique := migration.newMemoryIdUnique
      goalIdUnique := migration.newGoalIdUnique
      erasedPinIdUnique := migration.retainedTargetIdUnique
      headAligned := migration.newHeadAligned
      forkAnchorAligned := migration.newForkAnchorAligned
      pinTargetsExist := migration.retainedPinTargetsExist
      motivationsValid := migration.retainedMotivationsValid
      guidanceValid := migration.retainedGuidanceValid
      originKind := migration.retainedOriginKind
      memoryGoalIdsDisjoint := migration.memoryGoalIdsDisjoint
      erasedMemoryDisjoint := migration.erasedMemoryDisjoint
      erasedGoalDisjoint := migration.erasedGoalDisjoint
      abstractionHasOrigins := migration.abstractionHasOrigins
      derivedProvenance := migration.derivedProvenance
      pinTimeStrict := migration.pinTimeStrict
    }
    assignments := migration.assignmentsValid
    evidence := migration.evidenceValid
    terminalClose := migration.terminalCloseValid
  }

end Causa
