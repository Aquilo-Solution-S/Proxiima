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
def GoalState.terminal (state : GoalState) : Bool := !state.isOpen

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
  | pause
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
  | .Active, .pause => .ok .Paused
  | .Active, .resume => .error .wrongState
  | .Active, .achieve => .ok .Achieved
  | .Active, .abandon => .ok .Abandoned
  | .Paused, .set => .error .wrongState
  | .Paused, .decompose => .error .wrongState
  | .Paused, .modify => .error .wrongState
  | .Paused, .pause => .error .wrongState
  | .Paused, .resume => .ok .Active
  | .Paused, .achieve => .error .wrongState
  | .Paused, .abandon => .ok .Abandoned
  | .Achieved, .set | .Achieved, .decompose | .Achieved, .modify | .Achieved, .pause
  | .Achieved, .resume | .Achieved, .achieve | .Achieved, .abandon => .error .wrongState
  | .Abandoned, .set | .Abandoned, .decompose | .Abandoned, .modify | .Abandoned, .pause
  | .Abandoned, .resume | .Abandoned, .achieve | .Abandoned, .abandon => .error .wrongState

/-- Allowed predecessors and successors on the plain write path. -/
def mayTransitionTo : GoalState → GoalState → Bool
  | .Active, .Active => true
  | .Active, .Paused => true
  | .Active, .Achieved => false
  | .Active, .Abandoned => true
  | .Paused, .Active => true
  | .Paused, .Paused => false
  | .Paused, .Achieved => false
  | .Paused, .Abandoned => true
  | .Achieved, .Active | .Achieved, .Paused | .Achieved, .Achieved | .Achieved, .Abandoned => false
  | .Abandoned, .Active | .Abandoned, .Paused | .Abandoned, .Achieved | .Abandoned, .Abandoned => false

/-- The label projected on Goal reads. -/
def stateLabel : GoalState → String
  | .Active => "Active"
  | .Paused => "Paused"
  | .Achieved => "Achieved"
  | .Abandoned => "Abandoned"

/-- Who owns execution and the interpretation of a pause disposition. -/
inductive Execution where
  | host
  | core
  deriving DecidableEq

/-- Execution belongs to the host. -/
def execution : Execution := .host

/-- Core scheduling would assign Goal effects to the persistence layer. -/
theorem not_core_execution : execution ≠ .core := by decide

/-- Whether a sub-goal in this state keeps its parent from closing, achieved or abandoned alike. -/
def blocksClose (child : GoalState) : Bool := child.isOpen

/-- A parent whose sub-goals were all abandoned could only be abandoned, though its work may be done without them. -/
theorem not_achieve_needs_an_achieved_child : blocksClose .Abandoned = false := by decide

/-- Whether close-Fact presence agrees exactly with terminal state. -/
def closeFactPresence : GoalState → Bool → Bool
  | state, true => state.terminal
  | state, false => state.isOpen

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

/-- Whether an edit omits its pin list or supplies its replacement, including an empty list. -/
inductive PinEdit where
  | omitted
  | supplied
  deriving DecidableEq

/-- Whether a revision retains or replaces a declaration. -/
inductive RevisionField where
  | carry
  | replace
  deriving DecidableEq

/-- Omitted motivation and evidence carry forward and supplied lists replace them. -/
def editPins : PinEdit → RevisionField
  | .omitted => .carry
  | .supplied => .replace

/-- Declaring writes supply a prompt and lifecycle changes carry the stored prompt byte for byte. -/
def promptRevision : Event → RevisionField
  | .set | .decompose | .modify => .replace
  | .pause | .resume | .achieve | .abandon => .carry

/-- A child keeps the parent series chosen at birth alongside its exact parent dependency under `Causa.GoalStep`. -/
def parentRevision : Event → RevisionField
  | .set | .decompose => .replace
  | .modify | .pause | .resume | .achieve | .abandon => .carry

/-- Source classes considered when admitting Goal evidence. -/
inductive EvidenceSource where
  | local
  | otherStore
  deriving DecidableEq

/-- Where a Goal evidence target's live kind is checked. -/
inductive EvidenceCheck where
  | ownerTransaction
  | callerReadAtSource
  deriving DecidableEq

/-- Exact live Facts and Abstractions are eligible as Goal evidence under `Causa.RetainedGoalEvidenceValid`. -/
def evidenceAdmission : EvidenceSource → EvidenceCheck
  | .local => .ownerTransaction
  | .otherStore => .callerReadAtSource

/-- Which Goal operations can supply an evidence statement, always optional. -/
def mayDeclareEvidence : Event → Bool
  | .set | .decompose | .modify | .achieve => true
  | .pause | .resume | .abandon => false

/-- Goal evidence is read from the exact declaration whose pins are projected by `Causa.goalDeclaredTargetIds`. -/
def evidenceDeclaration : String := "goal.evidence_t"

/-- The authoritative field behind the reverse index, with targets governed by `Causa.goal_motivations_valid`. -/
def motivationDeclaration : String := "goal.motivated_by"

/-- Which Goal identity a read surface selects. -/
inductive ReadGoal where
  | read
  | list
  | declaredReference
  deriving DecidableEq

/-- The revision selected by an authorized read. -/
inductive ReadRevision where
  | readableHead
  | exact
  deriving DecidableEq

/-- Goal reads and lists select readable heads while declared references keep their exact revision. -/
def readRevision : ReadGoal → ReadRevision
  | .read => .readableHead
  | .list => .readableHead
  | .declaredReference => .exact

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

/-- After authority, exact replay returns the original before mutable targets and creates no second write. -/
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

/-- Transient transaction refusals on the pool-scoped idempotent write path. -/
inductive TransactionRefusal where
  | serialization
  | deadlock
  | other
  deriving DecidableEq

/-- Whether to retry the complete pool-scoped Goal transaction. -/
def retryTransaction : TransactionRefusal → Bool
  | .serialization | .deadlock => true
  | .other => false

/-- The steps of Goal revision persistence, from owner fencing to target revalidation. -/
inductive AdmissionStep where
  | fenceOwners
  | reserveFactIdentity
  | lockMemoryHandles
  | lockLifecycleUnion
  | lockGoalHeads
  | revalidateTargets
  deriving DecidableEq

/-- Goal revision persistence reserves lifecycle Facts before locking the sorted Memory and lifecycle sets and comparing the prepared head. -/
def admissionOrder : List AdmissionStep :=
  [.fenceOwners, .reserveFactIdentity, .lockMemoryHandles, .lockLifecycleUnion,
   .lockGoalHeads, .revalidateTargets]

/-- A Goal write requires each discovered source to pass a read revalidated under the writing transaction's owner and Goal route locks. -/
def sourceAdmission : (footprintHeld : Bool) → (readValid : Bool) → Bool
  | true, true => true
  | true, false | false, true | false, false => false

/-- The only Fact admission path allowed to use a reserved Goal-change identity. -/
inductive FactAdmission where
  | goalTransaction
  | ordinaryFact
  deriving DecidableEq

/-- A Goal-change Fact uses its reserved identity under the pre-held lock union, with no declared scope or read set. -/
def reserveChangeFact : FactAdmission → Bool
  | .goalTransaction => true
  | .ordinaryFact => false

/-- The publication admission for a fresh Goal revision. -/
inductive ChangeFactAdmission where
  | skip
  | publish
  | missingStamp
  deriving DecidableEq

/-- A listenable fresh revision requires a stamp and emits its change Fact after the Goal in the same transaction. -/
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
  | .pause => .pending

/-- The Goal's prompt and title are trimmed, nonblank and within their character bounds at admission. -/
def displayAdmitted : (nonblank : Bool) → (withinBound : Bool) → Bool
  | true, true => true
  | true, false | false, true | false, false => false

/-- An edit uses the named Goal's owner and an explicit space can only narrow that selection. -/
def editSpaceMatches : (explicitSpace : Bool) → (sameOwner : Bool) → Bool
  | false, false | false, true | true, true => true
  | true, false => false

/-- A state filter is a union of case-insensitive labels, and binds its canonical set into the cursor. -/
def stateFilter : (filterOmitted : Bool) → (inCanonicalSet : Bool) → Bool
  | true, false | true, true | false, true => true
  | false, false => false

/-- Fact Goal filters match references to any readable revision of a selected series before page limiting. -/
def factGoalFilter : (referencesReadableRevision : Bool) → (matchesOtherFilters : Bool) → Bool
  | true, true => true
  | true, false | false, true | false, false => false

/-- The Goal evidence vector is returned as declared when its exact version is visible. -/
def readEvidence : (goalVisible : Bool) → Bool
  | true => true
  | false => false

/-- Earlier assignment belongs only to the historical version that declared it. -/
def reusesAssignment : Event → Bool
  | .set | .decompose | .modify | .pause | .resume | .achieve | .abandon => false

/-- Recall includes unmotivated Active Goals and Active Goals motivated by a recalled Perspective head. -/
def recallActiveGoal : (hasMotivation : Bool) → (perspectiveMatches : Bool) → Bool
  | false, true | false, false | true, true => true
  | true, false => false

/-- A Goal remains owned by its original owner throughout its series. -/
def transferGoal : Bool := false

/-- The two clocks projected by a Goal version read. -/
inductive GoalClock where
  | setAt
  | changedAt
  deriving DecidableEq

/-- Which revision identity supplies a projected time. -/
inductive GoalVersionTime where
  | firstVersion
  | selectedVersion
  deriving DecidableEq

/-- A Goal reports its series birth and the selected version's own write time. -/
def clockSource : GoalClock → GoalVersionTime
  | .setAt => .firstVersion
  | .changedAt => .selectedVersion

end Proxiima.Goals
