import Causa.Provenance
import Proxiima.SchemaEvolution

/-! Causa — Schema evolution -/

namespace Causa

open Proxiima.Edges (PinField)
open Proxiima.SchemaEvolution (GoalPinRole)

/-- The identity space a migration pin names. -/
inductive MigrationPinTarget where
  | memory
  | goal

structure MigrationPins where
  target : MigrationPinTarget
  ids : List Id

/-- Exact targets of each Memory pin field, including context-only fields. -/
def memoryMigrationPins (m : Memory) : PinField → MigrationPins
  | .origins => ⟨.memory, memory_origins m⟩
  | .refs => ⟨.memory, memory_refs m⟩
  | .goalRefs => ⟨.goal, memory_goal_refs m⟩
  | .guidedBy => ⟨.goal, memory_guided_by m⟩
  | .reads => ⟨.memory, memory_reads m⟩
  | .forkedFrom => ⟨.memory, (memory_forked_from m).toList⟩

/-- Exact targets of each Goal pin field. -/
def goalMigrationPins (g : Goal) : GoalPinRole → MigrationPins
  | .assignment => ⟨.memory, (goal_assignment g).toList⟩
  | .dependency => ⟨.goal, goal_dependencies g⟩
  | .evidence => ⟨.memory, goal_evidence g⟩
  | .motivation => ⟨.memory, goal_motivated_by g⟩
  | .closeFact => ⟨.memory, (goal_close_fact_t g).toList⟩
  | .writeAct => ⟨.memory, (goal_write_act_t g).toList⟩

/-- A declared conversion and the premises for validity of its retained graph. -/
structure SchemaMigrationWitness where
  oldMemories : Set Memory
  newMemories : Set Memory
  oldGoals : Set Goal
  newGoals : Set Goal
  oldHeads : Set MemoryHead
  newHeads : Set MemoryHead
  /-- Erase witnesses the new store may pin. -/
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
  /-- Admission identities removed at cutover, indexed by their identity space. -/
  removedIdentities : MigrationPinTarget → Set Id
  /-- Surviving Memory pins avoid the identities removed at cutover. -/
  memoryPinsAvoidRemoved : ∀ m, m ∈ newMemories → ∀ role id,
    id ∈ (memoryMigrationPins m role).ids →
    id ∉ removedIdentities (memoryMigrationPins m role).target
  /-- Surviving Goal pins avoid the identities removed at cutover. -/
  goalPinsAvoidRemoved : ∀ g, g ∈ newGoals → ∀ role id,
    id ∈ (goalMigrationPins g role).ids →
    id ∉ removedIdentities (goalMigrationPins g role).target
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
  /-- Evidence in the retained graph resolves locally. -/
  evidenceValid : RetainedGoalEvidenceValid newGoals newMemories retainedTargets
  terminalCloseValid :
    RetainedGoalTerminalCloseFactValid newGoals newMemories retainedTargets
  /-- Dependencies resolve to retained Goal identities. -/
  retainedDependencyTargetsExist : ∀ g, g ∈ newGoals → ∀ id, id ∈ goal_dependencies g →
    retainedGoalReferenceTargetExists newGoals retainedTargets id
  /-- Write acts resolve to retained Memory identities. -/
  retainedWriteActTargetsExist : ∀ g, g ∈ newGoals → ∀ id, goal_write_act_t g = some id →
    retainedMemoryReferenceTargetExists newMemories retainedTargets id

/-- A pin resolves to an admission or erase witness in its own identity space. -/
def SchemaMigrationWitness.pinTargetExists (migration : SchemaMigrationWitness)
    (id : Id) : MigrationPinTarget → Prop
  | .memory => retainedMemoryReferenceTargetExists migration.newMemories migration.retainedTargets id
  | .goal => retainedGoalReferenceTargetExists migration.newGoals migration.retainedTargets id

/-- Every Memory and Goal pin in the retained migration graph resolves in its identity space and avoids removed identities. -/
theorem schema_migration_pins_resolve (migration : SchemaMigrationWitness) :
    (∀ m, m ∈ migration.newMemories → ∀ role id,
      id ∈ (memoryMigrationPins m role).ids →
      migration.pinTargetExists id (memoryMigrationPins m role).target ∧
      id ∉ migration.removedIdentities (memoryMigrationPins m role).target) ∧
    (∀ g, g ∈ migration.newGoals → ∀ role id,
      id ∈ (goalMigrationPins g role).ids →
      migration.pinTargetExists id (goalMigrationPins g role).target ∧
      id ∉ migration.removedIdentities (goalMigrationPins g role).target) := by
  constructor
  · intro m hm role id hpin
    refine ⟨?_, migration.memoryPinsAvoidRemoved m hm role id hpin⟩
    obtain ⟨origins, refs, forks, goalRefs⟩ := migration.retainedPinTargetsExist m hm
    cases role with
    | origins => exact origins id hpin
    | refs => exact refs id hpin
    | forkedFrom => exact forks id (Option.mem_toList.mp hpin)
    | goalRefs => exact goalRefs id hpin
    | reads => exact migration.retainedReadTargetsExist m hm id hpin
    | guidedBy =>
      have guided : id ∈ memory_guided_by m := hpin
      have perspective : memory_kind m = .Perspective := by
        by_cases perspective : memory_kind m = .Perspective
        · exact perspective
        · simp [memory_guided_by, m.guided_by_only_perspective perspective] at guided
      rcases migration.retainedGuidanceValid m hm perspective id guided with
        ⟨g, hg, target, _⟩ | witness
      · exact Or.inl ⟨g, hg, target⟩
      · exact Or.inr witness
  · intro g hg role id hpin
    refine ⟨?_, migration.goalPinsAvoidRemoved g hg role id hpin⟩
    cases role with
    | assignment =>
      have assigned : goal_assignment g = some id := Option.mem_toList.mp hpin
      rcases migration.assignmentsValid g hg with absent | ⟨m, hm, target, _⟩ |
        ⟨witness, hw, i, assignment, identity, kind⟩
      · simp [assigned] at absent
      · exact Or.inl ⟨m, hm, Option.some.inj (target.symm.trans assigned)⟩
      · exact Or.inr ⟨.Perspective, witness, hw,
          identity.trans (Option.some.inj (assignment.symm.trans assigned)), kind⟩
    | dependency => exact migration.retainedDependencyTargetsExist g hg id hpin
    | evidence =>
      rcases migration.evidenceValid g hg id hpin with ⟨m, hm, target, _⟩ |
        ⟨witness, hw, target, kind⟩
      · exact Or.inl ⟨m, hm, target⟩
      · rcases kind with fact | abstraction
        · exact Or.inr ⟨.Fact, witness, hw, target, fact⟩
        · exact Or.inr ⟨.Abstraction, witness, hw, target, abstraction⟩
    | motivation =>
      rcases migration.retainedMotivationsValid g hg id hpin with ⟨m, hm, target, _⟩ |
        ⟨witness, hw, target, kind⟩
      · exact Or.inl ⟨m, hm, target⟩
      · exact Or.inr ⟨.Perspective, witness, hw, target, kind⟩
    | closeFact =>
      have closed : goal_close_fact_t g = some id := Option.mem_toList.mp hpin
      have terminal : (goal_state g).terminal = true := by
        apply g.terminal_close_fact.mpr
        change (goal_close_fact_t g).isSome = true
        rw [closed]
        rfl
      rcases migration.terminalCloseValid g hg terminal with ⟨m, hm, target, _⟩ |
        ⟨witness, hw, target, kind⟩
      · exact Or.inl ⟨m, hm, Option.some.inj (target.symm.trans closed)⟩
      · exact Or.inr ⟨.Fact, witness, hw, Option.some.inj (target.symm.trans closed), kind⟩
    | writeAct => exact migration.retainedWriteActTargetsExist g hg id (Option.mem_toList.mp hpin)

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
