/-! # Host state -/

namespace Proxiima.HostState

/-- Participant registration accumulated across runtime configuration layers. -/
inductive Registration where
  | absent
  | single
  | duplicate
  deriving DecidableEq

/-- A participant is optional, and a second registration anywhere is a duplicate. -/
def mergeRegistration : Registration → Registration → Registration
  | .absent, .absent => .absent
  | .single, .absent | .absent, .single => .single
  | .single, .single
  | .duplicate, .absent | .duplicate, .single | .duplicate, .duplicate
  | .absent, .duplicate | .single, .duplicate => .duplicate

/-- The unit's authority at the ordinary host-state entry point. -/
inductive UnitKind where
  | ordinary
  | maintenance
  | goalWork
  | system
  deriving DecidableEq

/-- The gate used by the ordinary host-state entry point. -/
inductive Admission where
  | factWrite
  | fixedOwner
  | refuse
  deriving DecidableEq

/-- Ordinary commands require Fact-write authority, maintenance commands require the fixed owner, and other units use their own verbs. -/
def commandAdmission : UnitKind → Admission
  | .ordinary => .factWrite
  | .maintenance => .fixedOwner
  | .goalWork | .system => .refuse

/-- Results of dispatch after the write session has opened. -/
inductive DispatchResult where
  | permitted
  | alreadyApplied
  | refused
  | storageFault
  | invalidReply
  | cancelled
  deriving DecidableEq

/-- Business outcomes leave the shared unit usable; storage faults, a malformed reply or cancellation poison it. -/
def dispatchLeavesUnitUsable : DispatchResult → Bool
  | .permitted | .alreadyApplied | .refused => true
  | .storageFault | .invalidReply | .cancelled => false

end Proxiima.HostState
