/-! # Goals -/

namespace Proxiima.Goals

/-- The lifecycle state of a Goal version. -/
inductive GoalState where
  | Active
  | Paused
  | Achieved
  | Abandoned
  deriving DecidableEq

/-- Whether the series can take another version. -/
def GoalState.isOpen : GoalState → Bool
  | .Active => true
  | .Paused => true
  | .Achieved => false
  | .Abandoned => false

/-- Whether a Goal version requires a close Fact and ends its series. -/
def GoalState.terminal : GoalState → Bool
  | .Active | .Paused => false
  | .Achieved | .Abandoned => true

/-- The host's pause disposition for queued work, with running work allowed to finish. -/
inductive PendingWork where
  /-- Hold queued work for resume. -/
  | keep
  /-- Drop queued and retryable work. -/
  | discard
  deriving DecidableEq

/-- The operation that writes a Goal version. -/
inductive Event where
  | set
  | decompose
  | modify
  | pause (pending : PendingWork)
  | resume
  | achieve
  | abandon
  deriving DecidableEq

/-- Set and decomposition start each new series here. -/
def birthState : GoalState := .Active

/-- Why an operation cannot use the named series head. -/
inductive LifecycleRefusal where
  | wrongState
  deriving DecidableEq

/-- Admission by existing head state, returning the new version state or the new child state for decomposition. -/
def lifecycle : GoalState → Event → Except LifecycleRefusal GoalState
  | .Active, .set => .error .wrongState
  | .Active, .decompose => .ok birthState
  | .Active, .modify => .ok .Active
  | .Active, .pause .keep | .Active, .pause .discard => .ok .Paused
  | .Active, .resume => .error .wrongState
  | .Active, .achieve => .ok .Achieved
  | .Active, .abandon => .ok .Abandoned
  | .Paused, .set => .error .wrongState
  | .Paused, .decompose => .error .wrongState
  | .Paused, .modify => .error .wrongState
  | .Paused, .pause .keep | .Paused, .pause .discard => .error .wrongState
  | .Paused, .resume => .ok .Active
  | .Paused, .achieve => .error .wrongState
  | .Paused, .abandon => .ok .Abandoned
  | .Achieved, .set | .Achieved, .decompose | .Achieved, .modify
  | .Achieved, .pause .keep | .Achieved, .pause .discard
  | .Achieved, .resume | .Achieved, .achieve | .Achieved, .abandon => .error .wrongState
  | .Abandoned, .set | .Abandoned, .decompose | .Abandoned, .modify
  | .Abandoned, .pause .keep | .Abandoned, .pause .discard
  | .Abandoned, .resume | .Abandoned, .achieve | .Abandoned, .abandon => .error .wrongState

/-- The label projected on Goal reads. -/
def stateLabel : GoalState → String
  | .Active => "Active"
  | .Paused => "Paused"
  | .Achieved => "Achieved"
  | .Abandoned => "Abandoned"

/-- An abandoned child leaves the parent free to close. -/
theorem not_achieve_needs_an_achieved_child : GoalState.isOpen .Abandoned = false := by decide

/-- The largest trimmed Goal title in characters. -/
def titleMax : Nat := 240

/-- The largest trimmed Goal prompt in characters. -/
def textMax : Nat := 20000

/-- The largest number of children in one decompose request. -/
def decomposeChildMax : Nat := 50

/-- The root key character ceiling that leaves room for the child prefix, separators and largest index. -/
def decomposeRootKeyMax : Nat := 162

/-- The prefix of each child request key, followed by the root key and zero-based index. -/
def decomposeChildPrefix : String := "goal_decompose"

/-- Whether a revision retains or replaces a declaration. -/
inductive RevisionField where
  | carry
  | replace
  deriving DecidableEq

/-- Declaring writes supply a prompt and lifecycle changes carry the stored prompt byte for byte. -/
def promptRevision : Event → RevisionField
  | .set | .decompose | .modify => .replace
  | .pause .keep | .pause .discard | .resume | .achieve | .abandon => .carry

/-- A child keeps the parent series chosen at birth alongside its exact parent dependency under `Causa.GoalStep`. -/
def parentRevision : Event → RevisionField
  | .set | .decompose => .replace
  | .modify | .pause .keep | .pause .discard | .resume | .achieve | .abandon => .carry

/-- Which Goal operations can supply an evidence statement, always optional. -/
def mayDeclareEvidence : Event → Bool
  | .set | .decompose | .modify | .achieve => true
  | .pause .keep | .pause .discard | .resume | .abandon => false

/-- Goal evidence is read from the exact declaration whose pins are projected by `Causa.goalDeclaredTargetIds`. -/
def evidenceDeclaration : String := "goal.evidence_t"

/-- The authoritative field behind the reverse index, with targets governed by `Causa.goal_motivations_valid`. -/
def motivationDeclaration : String := "goal.motivated_by"

/-- Operators use their authorship category in the durable command and its replay identity. -/
def operatorAuthorship : String := "system_operator"

/-- The durable request comparison made under the owner and request key. -/
inductive RequestMatch where
  | fresh
  | exact
  | different
  | missingDeclaration
  deriving DecidableEq

/-- Whether the replay may be returned, must be refused, or needs fresh admission. -/
inductive ReplayAction where
  | freshAdmission
  | returnOriginal
  | conflict
  deriving DecidableEq

/-- Fresh requests enter admission, exact replays return the original, and differing or undeclared requests conflict. -/
def requestReplay : RequestMatch → ReplayAction
  | .fresh => .freshAdmission
  | .exact => .returnOriginal
  | .different => .conflict
  | .missingDeclaration => .conflict

/-- The replay status across a decomposition's complete request set. -/
inductive DecomposeReplay where
  | allFresh
  | allExact
  | mixed
  | conflicting
  deriving DecidableEq

/-- Decomposition admits the whole request set or answers its whole replay. -/
def decomposeReplay : DecomposeReplay → ReplayAction
  | .allFresh => .freshAdmission
  | .allExact => .returnOriginal
  | .mixed => .conflict
  | .conflicting => .conflict

/-- The durable parts compared for replay, separate from an episode's write-act binding. -/
inductive ReplayField where
  | command
  | expectedHead
  | authorCategory
  | sessionSelf
  | writeAct
  deriving DecidableEq

/-- Replay seals the command, including any pause disposition, and excludes attempt attribution. -/
def replayIdentity : ReplayField → Bool
  | .command | .expectedHead | .authorCategory => true
  | .sessionSelf | .writeAct => false

/-- The outcomes of comparing the predecessor under the locked Goal head. -/
inductive HeadComparison where
  | unchanged
  | namedStale
  | unnamedDrift
  deriving DecidableEq

/-- The commit decision after comparing the predecessor. -/
inductive CommitDecision where
  | proceed
  | conflict
  | retry
  deriving DecidableEq

/-- A stale named predecessor conflicts and unnamed prediction drift retries the complete transaction. -/
def predecessorArbitration : HeadComparison → CommitDecision
  | .unchanged => .proceed
  | .namedStale => .conflict
  | .unnamedDrift => .retry

/-- The steps of Goal revision persistence, from owner fencing to target revalidation. -/
inductive AdmissionStep where
  | fenceOwners
  | reserveFactIdentity
  | lockMemoryHandles
  | lockLifecycleUnion
  | lockGoalHeads
  | revalidateTargets
  deriving DecidableEq

/-- Goal revision persistence steps, in order. -/
def admissionOrder : List AdmissionStep :=
  [.fenceOwners, .reserveFactIdentity, .lockMemoryHandles, .lockLifecycleUnion,
   .lockGoalHeads, .revalidateTargets]

/-- The publication admission for a fresh Goal revision. -/
inductive ChangeFactAdmission where
  | skip
  | publish
  | missingStamp
  deriving DecidableEq

/-- A listenable fresh revision emits a change Fact when stamped and is refused without a stamp. -/
def publishChangeFact : (listenable : Bool) → (stamped : Bool) → ChangeFactAdmission
  | false, false | false, true => .skip
  | true, false => .missingStamp
  | true, true => .publish

/-- The variant-specific payload fields of a Goal change. -/
inductive ChangeFields where
  | plain
  | parent
  | pending
  deriving DecidableEq

/-- Decomposition alone carries the parent revision and pause alone carries a pending disposition. -/
def changeFields : Event → ChangeFields
  | .set | .modify | .resume | .achieve | .abandon => .plain
  | .decompose => .parent
  | .pause .keep | .pause .discard => .pending

/-- The title or prompt’s length after trimming surrounding Unicode White_Space. -/
inductive TrimmedDisplayLength where
  | blank
  | withinBound
  | oversized
  deriving DecidableEq

/-- The Rust input boundary uses str::trim, counts the remaining Unicode scalar values and admits a title or prompt with length from one through its field limit. -/
def displayAdmitted : TrimmedDisplayLength → Bool
  | .withinBound => true
  | .blank | .oversized => false

/-- An edit uses the named Goal's owner and an explicit space can only narrow that selection. -/
def editSpaceMatches : (explicitSpace : Bool) → (sameOwner : Bool) → Bool
  | false, false | false, true | true, true => true
  | true, false => false

/-- Goal queries select every state when the filter is omitted, otherwise the union of the supplied labels. -/
def stateFilter : (filterOmitted : Bool) → (inCanonicalSet : Bool) → Bool
  | true, false | true, true | false, true => true
  | false, false => false

/-- Recall includes unmotivated Active Goals and Active Goals motivated by a recalled Perspective head. -/
def recallActiveGoal : (hasMotivation : Bool) → (perspectiveMatches : Bool) → Bool
  | false, true | false, false | true, true => true
  | true, false => false

end Proxiima.Goals
