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

/-- The frozen participant has a nonempty identity and a nonempty set of distinct declared state surfaces. -/
inductive ParticipantRequirement where
  | namedParticipant
  | atLeastOneSurface
  | namedSurfaces
  | distinctSurfaces
  | declaredStateSurfaces
  deriving DecidableEq

/-- A command names the frozen participant and a nonempty, duplicate-free subset of its registered state surfaces. -/
inductive CommandRequirement where
  | sameParticipant
  | atLeastOneSurface
  | distinctSurfaces
  | registeredSurfaces
  deriving DecidableEq

/-- A dispatched request agrees exactly with the stamped participant and command table list. -/
inductive RequestBinding where
  | participant
  | commandTables
  deriving DecidableEq

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

/-- Maintenance authority belongs to one engine boot and its frozen participant, and each opened unit fixes one owner. -/
inductive MaintenanceBinding where
  | boot
  | participant
  | owner
  deriving DecidableEq

/-- A maintenance unit exposes only host-state commands and commit. -/
inductive MaintenanceOperation where
  | applyHostState
  | commit
  deriving DecidableEq

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

/-- Host-state work and cognitive writes share the unit's transaction and its completion. -/
inductive SharedWriteSession where
  | liveDispatch
  | commitTogether
  | rollbackTogether
  deriving DecidableEq

/-- Lifecycle callback registration binds the participant and exactly its frozen managed surfaces. -/
inductive LifecycleRegistration where
  | sameParticipant
  | exactManagedSurfaceSet
  | subsetOfParticipantSurfaces
  deriving DecidableEq

/-- Host lifecycle operations distinguish an erase from an export. -/
inductive LifecycleOperation where
  | erase
  | export
  deriving DecidableEq

/-- A lifecycle callback's transaction context. -/
inductive LifecycleContext where
  | eraseTransaction
  | readOnlyExportSnapshot
  deriving DecidableEq

/-- The callback participates in the authorized erase transaction or in the read-only owner export snapshot. -/
def lifecycleContext : LifecycleOperation → LifecycleContext
  | .erase => .eraseTransaction
  | .export => .readOnlyExportSnapshot

/-- The checks on a host erase receipt before the shared erase can commit. -/
inductive EraseReceiptCheck where
  | sameParticipant
  | sameOwner
  | sameScope
  | samePhysicalFacts
  | sameOriginalCopies
  | uniqueExactTables
  | zeroCountsForRetainedTables
  | countsFitBigint
  deriving DecidableEq

/-- The checks and count source for a host export receipt. -/
inductive ExportReceiptRule where
  | sameParticipant
  | sameOwner
  | uniqueExactTables
  | declaredRowShape
  | countReturnedRows
  deriving DecidableEq

end Proxiima.HostState
