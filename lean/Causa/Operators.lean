import Causa.Prelude
import Causa.Owner
import Causa.Identity
import Causa.Memory
import Causa.Goals
import Causa.Edges
import Proxiima.Memory
import Proxiima.Edges

/-! Causa — Operators -/

namespace Causa

open Proxiima.Memory (OperatorPhase operatorAdmitsInput operatorOutput memoryKind EntityKind)
open Proxiima.Edges (pinField)

/-- Memory operators admit exactly the origin target kinds of their output layer. -/
theorem operator_inputs_are_origin_targets (k : EntityKind) :
    operatorAdmitsInput .ToA k = decide (k ∈ (pinField .origins).abstraction) ∧
    operatorAdmitsInput .AtoP k = decide (k ∈ (pinField .origins).perspective) := by
  cases k <;> decide

def OperatorPhase.inputKind (phase : OperatorPhase) : MemoryKind → Prop
  | .Fact => operatorAdmitsInput phase .Fact = true
  | .Abstraction => operatorAdmitsInput phase .Abstraction = true
  | .Perspective => operatorAdmitsInput phase .Perspective = true

def OperatorPhase.outputMemoryKind (phase : OperatorPhase) (kind : MemoryKind) : Prop :=
  memoryKind (operatorOutput phase) = some kind

def OperatorPhase.outputGoalAllowed (phase : OperatorPhase) : Prop :=
  operatorOutput phase = .Goal

structure OperatorInvocation where
  phase : OperatorPhase
  operatorId : OperatorId
  inputContractId : InputContract
  inputs : Set Memory
  outputMemories : Set Memory
  outputGoals : Set Goal

structure InvocationShapeValid (inv : OperatorInvocation) : Prop where
  inputsShape : ∀ m : Memory, m ∈ inv.inputs →
    OperatorPhase.inputKind inv.phase (memory_kind m)
  outputMemoriesShape : ∀ m : Memory, m ∈ inv.outputMemories →
    OperatorPhase.outputMemoryKind inv.phase (memory_kind m)
  outputGoalsShape : ∀ g : Goal, g ∈ inv.outputGoals →
    OperatorPhase.outputGoalAllowed inv.phase

/-- Input coverage by the provenance pins of an operator's outputs. -/
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
  intro hf
  have h := hshape.outputMemoriesShape m hm
  rw [hf] at h
  cases hphase : inv.phase <;>
    simp [OperatorPhase.outputMemoryKind, hphase, operatorOutput, memoryKind] at h

end Causa
