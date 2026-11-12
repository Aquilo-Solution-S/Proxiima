/-
Causa — Operators

Production shapes. Provenance is the output row's `origins[]` (or a Goal's
`evidence_t`). No edge ledger. No source-batch gate on Memory (CN-8 retired:
visit is a Fact + refs). Recipe metadata is sidecar, not a Memory column.
-/

import Causa.Prelude
import Causa.Owner
import Causa.Identity
import Causa.Memory
import Causa.Goals
import Causa.Edges

namespace Causa

inductive OperatorPhase where
  | toa
  | atop
  | atogoal
  deriving DecidableEq, Repr

def OperatorPhase.inputKind : OperatorPhase → MemoryKind → Prop
  | .toa, k => k = .Fact ∨ k = .Abstraction
  | .atop, k => k = .Abstraction
  | .atogoal, k => k = .Abstraction

def OperatorPhase.outputMemoryKind : OperatorPhase → MemoryKind → Prop
  | .toa, k => k = .Abstraction
  | .atop, k => k = .Perspective
  | .atogoal, _ => False

def OperatorPhase.outputGoalAllowed : OperatorPhase → Prop
  | .atogoal => True
  | _ => False

structure OperatorInvocation where
  phase : OperatorPhase
  operatorId : OperatorId
  inputContractId : InputContract
  inputs : Set Memory
  outputMemories : Set Memory
  outputGoals : Set Goal

structure InvocationShapeValid (inv : OperatorInvocation) : Prop where
  inputsShape : ∀ m : Memory, m ∈ inv.inputs →
    inv.phase.inputKind (memory_kind m)
  outputMemoriesShape : ∀ m : Memory, m ∈ inv.outputMemories →
    inv.phase.outputMemoryKind (memory_kind m)
  outputGoalsShape : ∀ g : Goal, g ∈ inv.outputGoals →
    inv.phase.outputGoalAllowed

/-- Completeness: each memory output names every input in `origins`;
    each Goal output names every input in `evidence_t`. -/
structure InvocationProvenanceComplete (inv : OperatorInvocation) : Prop where
  memoryInputs : ∀ out : Memory, out ∈ inv.outputMemories →
    ∀ inp : Memory, inp ∈ inv.inputs →
      memory_t inp ∈ memory_origins out
  goalInputs : ∀ g : Goal, g ∈ inv.outputGoals →
    ∀ inp : Memory, inp ∈ inv.inputs →
      memory_t inp ∈ goal_evidence g

theorem operator_memory_output_not_fact :
    ∀ (inv : OperatorInvocation) (m : Memory),
      InvocationShapeValid inv → m ∈ inv.outputMemories → memory_kind m ≠ .Fact := by
  intro inv m hshape hm
  have h := hshape.outputMemoriesShape m hm
  cases hphase : inv.phase with
  | toa => rw [hphase] at h; rw [h]; intro hf; exact (nomatch hf)
  | atop => rw [hphase] at h; rw [h]; intro hf; exact (nomatch hf)
  | atogoal => rw [hphase] at h; exact absurd h (by simp [OperatorPhase.outputMemoryKind])

end Causa
