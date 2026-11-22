import Causa.Prelude
import Causa.Owner
import Causa.Identity
import Causa.Memory
import Causa.Goals

/-! Causa — Pins -/

namespace Causa

/-- The relationship pins carried by a Memory admission. -/
def derivePins (m : Memory) :
    List MemoryId × List MemoryId × List GoalId × List GoalId × List MemoryId :=
  (memory_origins m, memory_reference_ids m,
    memory_goal_refs m, memory_guided_by m, memory_reads m)

def goalReferenceTargetExists (goals : Set Goal) (id : GoalId) : Prop :=
  ∃ g : Goal, g ∈ goals ∧ goal_t g = id

def retainedMemoryReferenceTargetExists
    (memories : Set Memory)
    (targets : Set ErasedPinTarget) (id : MemoryId) : Prop :=
  pinExists memories id ∨ recordedMemoryTargetExists targets id

def retainedGoalReferenceTargetExists
    (goals : Set Goal) (targets : Set ErasedPinTarget) (id : GoalId) : Prop :=
  goalReferenceTargetExists goals id ∨ recordedGoalTargetExists targets id

def retainedOriginTargetExists
    (memories : Set Memory)
    (targets : Set ErasedPinTarget) (id : MemoryId) : Prop :=
  pinExists memories id ∨ recordedMemoryTargetExists targets id

/-- Read projection of Goal references under the requester's visibility. -/
def resolveVisibleGoalRefs (readable : GoalId → Bool) (m : Memory) :
    List MemoryId × List GoalId :=
  (memory_refs m ++ (memory_goal_refs m).filter (fun g => !readable g),
   (memory_goal_refs m).filter readable)

/-- Indistinguishability of unreadable Goal and Memory references after projection. -/
theorem unreadable_goal_reference_resolves_like_a_memory_reference
    (readable : GoalId → Bool) (m n : Memory) (g : GoalId)
    (hmg : memory_goal_refs m = [g]) (hng : memory_goal_refs n = [])
    (hrefs : memory_refs n = memory_refs m ++ [g])
    (hg : readable g = false) :
    resolveVisibleGoalRefs readable m = resolveVisibleGoalRefs readable n := by
  simp [resolveVisibleGoalRefs, hmg, hng, hrefs, hg]

/-- info: 'Causa.unreadable_goal_reference_resolves_like_a_memory_reference' depends on axioms: [propext] -/
#guard_msgs in
#print axioms unreadable_goal_reference_resolves_like_a_memory_reference

end Causa
