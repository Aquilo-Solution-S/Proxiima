/-
Causa — Pins (no Edge table)

There is no index table: `origins[]`, `refs[]` and `reads[]` live on the
Memory row. The kinds:

  origin    = made-from   (origins)
  reference = points-at   (refs onto Memory, goal_refs onto Goal)
  read      = read-before (reads: memories the host certified the writing
              session read before this write). Context, not grounding:
              it never feeds origins or refs, and a Fact may carry it.

Rebuildability is identity: the pin set IS node content. Goal pins live on
the Goal row (`assignment_t`, `dependency_t`, `evidence_t`, `close_fact_t`,
`write_act_t`) and never appear in `memory.refs`. `parent_handle` names the
parent's series (the goal tree, #514); it is not a pin.

E1–E7 rebase:
E1 existence   — origin and refs t are a stored Memory.t, live or forgotten;
                goal_refs resolves against the Goal spine. Retained historical
                pins may also resolve through a kinded erase witness.
  E2 ownership   — the pin is on the declaring row; no separate owner column
  E3 layering    — OriginKindValid (UML CHECKs)
  E4 kind follows — origins vs refs; no verb writes a pin
  E5 / E6        — there is no pin row, so no pin id and no pin payload
  E7 rebuild     — derivePins m = (m.origins, memory_reference_ids m, m.goal_refs, m.guided_by, m.reads)
-/

import Causa.Prelude
import Causa.Owner
import Causa.Identity
import Causa.Memory
import Causa.Goals

namespace Causa

inductive EdgeKind where
  | origin
  | reference
  | read
  deriving DecidableEq, Repr

/-- Pins extracted from a node — THE derivation, and it is identity.
    Five source-owned columns: the Goal spine has ordinary references and
    explicit guidance references, kept separate from F/A/P origins; the
    host-certified read set is a fifth column beside them. -/
def derivePins (m : Memory) :
    List MemoryId × List MemoryId × List GoalId × List GoalId × List MemoryId :=
  (memory_origins m, memory_reference_ids m,
    memory_goal_refs m, memory_guided_by m, memory_reads m)

/- The new causal fields are still ordinary source-owned Reference pins, but
   remain separate from payload references and F/A/P grounding. -/
def deriveCausalReferences (memories : Set Memory) (goals : Set Goal) :
    Set (Id × Id) :=
  fun pair => (∃ m : Memory, m ∈ memories ∧
      ∃ id : GoalId, id ∈ memory_guided_by m ∧
        pair = (memory_t m, id)) ∨
    (∃ g : Goal, g ∈ goals ∧
      ∃ id : MemoryId, id ∈ goal_motivated_by g ∧
        pair = (goal_t g, id))

/- A legacy/live reference carrier can still name either spine. The storage
   split is reflected by `goalReferenceTargetExists`; this union remains the
   target-existence predicate for the pre-split/live graph relation. -/
def referenceTargetExists
    (memories : Set Memory) (goals : Set Goal)
    (id : Id) : Prop :=
  pinExists memories id ∨
  ∃ g : Goal, g ∈ goals ∧ goal_t g = id

/- A reference may point at a Goal, but no longer from the same column as a
   Memory reference. `refs` resolves against the Memory spine (`pinExists`)
   and `goal_refs` against the Goal spine; the column IS the target's kind,
   so nothing downstream has to re-derive it. Goal-DECLARED pins remain on
   the Goal row and are a separate thing again. -/
def goalReferenceTargetExists (goals : Set Goal) (id : GoalId) : Prop :=
  ∃ g : Goal, g ∈ goals ∧ goal_t g = id

/-- Historical reference existence: a stored Memory (live or forgotten), a live Goal, or a
    database-only erase witness. The mixed predicate is legacy-only; the
    typed predicates below are the retained graph contract after the split. -/
def retainedReferenceTargetExists
    (memories : Set Memory) (goals : Set Goal)
    (targets : Set ErasedPinTarget) (id : Id) : Prop :=
  referenceTargetExists memories goals id ∨
  erasedPinTargetExists targets id

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

/-- Read-side resolution after the Goal-reference split. `goal_refs` is the STORED typing; what
    a given reader may see is a separate question, so before projection the
    Goals this reader cannot read are moved back into the untyped `refs`
    carrier, where they redact exactly as an unreadable Memory does. The
    stored discriminant must not survive into the projection: knowing that
    a withheld target is a Goal is itself disclosure. -/
def resolveVisibleGoalRefs (readable : GoalId → Bool) (m : Memory) :
    List MemoryId × List GoalId :=
  (memory_refs m ++ (memory_goal_refs m).filter (fun g => !readable g),
   (memory_goal_refs m).filter readable)

def resolveVisibleGuidance (readable : GoalId → Bool) (m : Memory) : List GoalId :=
  (memory_guided_by m).filter readable

/-- The non-disclosure invariant. A row carrying an unreadable Goal
    reference resolves to exactly the carrier of a row that had that id in
    `refs` all along — so no reader downstream can tell the two apart, and
    the split buys its typing without widening what a reader learns. -/
theorem unreadable_goal_reference_resolves_like_a_memory_reference
    (readable : GoalId → Bool) (m n : Memory) (g : GoalId)
    (hmg : memory_goal_refs m = [g]) (hng : memory_goal_refs n = [])
    (hrefs : memory_refs n = memory_refs m ++ [g])
    (hg : readable g = false) :
    resolveVisibleGoalRefs readable m = resolveVisibleGoalRefs readable n := by
  simp [resolveVisibleGoalRefs, hmg, hng, hrefs, hg]

/-- Interpretation is a Perspective with empty origins that refs a subject. -/
def interpretationOf (p : Memory) (subject : Memory) : Prop :=
  memory_kind p = .Perspective ∧
  memory_origins p = [] ∧
  memory_t subject ∈ memory_refs p

/-- info: 'Causa.unreadable_goal_reference_resolves_like_a_memory_reference' depends on axioms: [propext] -/
#guard_msgs in
#print axioms unreadable_goal_reference_resolves_like_a_memory_reference

end Causa
