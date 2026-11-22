/-! # Operations -/

namespace Proxiima.Operations

/-- Whether a storage transaction has finished successfully. -/
inductive CommitOutcome where
  | committed | failed | pending
  deriving DecidableEq

/-- Wake local drains only once a transaction commits. -/
def wakeAfterCommit : CommitOutcome → Bool
  | .committed => true
  | .failed | .pending => false

/-- Where a drain looks for work after a commit. -/
inductive CommitOrigin where
  | localProcess | otherProcess
  deriving DecidableEq

/-- A local commit bumps a shared data-free epoch while discovery of other commits relies on polling. -/
def bumpsWakeEpoch : CommitOrigin → Bool
  | .localProcess => true
  | .otherProcess => false

/-- The statuses of a claim gate. -/
inductive GateStatus where
  | running | quiescing | quiescent | stopped
  deriving DecidableEq

/-- The gate events a host or its sole worker can initiate. -/
inductive GateEvent where
  | quiesce | resume | checkpoint | workerEnds
  deriving DecidableEq

/-- A pause closes the gate, the worker parks at a checkpoint, resume opens it and worker exit stops it. -/
def gateTransition : GateStatus → GateEvent → GateStatus
  | .stopped, .quiesce | .stopped, .resume | .stopped, .checkpoint
  | .running, .workerEnds | .quiescing, .workerEnds | .quiescent, .workerEnds
  | .stopped, .workerEnds => .stopped
  | .running, .quiesce | .quiescing, .quiesce => .quiescing
  | .quiescing, .checkpoint | .quiescent, .checkpoint | .quiescent, .quiesce => .quiescent
  | .running, .checkpoint | .running, .resume | .quiescing, .resume | .quiescent, .resume =>
      .running

/-- What a pause observes about the epoch it joined. -/
inductive PauseEpoch where
  | closing | parked | overtaken | reopened | stopped
  deriving DecidableEq

/-- What a waiting pause returns or does next. -/
inductive PauseAction where
  | wait | complete | refuseResumed | refuseStopped
  deriving DecidableEq

/-- A pending pause waits for its own epoch and distinguishes a resumed gate from a stopped one. -/
def pauseOutcome : PauseEpoch → PauseAction
  | .closing => .wait
  | .parked => .complete
  | .overtaken | .reopened => .refuseResumed
  | .stopped => .refuseStopped

/-- Whether a worker may proceed at a claim boundary. -/
inductive ClaimBoundary where
  | opened | closed | cancelled
  deriving DecidableEq

/-- A worker proceeds only through an open gate while it is active. -/
def proceedWithClaim : ClaimBoundary → Bool
  | .opened => true
  | .closed | .cancelled => false

/-- The outcome of a background pass. -/
inductive Pass where
  | worked | idle | failed
  deriving DecidableEq

/-- How the next worker pass is scheduled. -/
inductive PassWait where
  | immediate | idleBackoff | failureBackoff
  deriving DecidableEq

/-- Progress retries immediately, while idle and failed passes back off independently. -/
def passWait : Pass → PassWait
  | .worked => .immediate
  | .idle => .idleBackoff
  | .failed => .failureBackoff

/-- A commit interrupts only an idle wait of a worker subscribed to local writes. -/
def commitEndsWait : Pass → (subscribed : Bool) → Bool
  | .idle, true => true
  | .worked, true | .failed, true | .worked, false | .idle, false | .failed, false => false

/-- A worker's backoff multiplies the previous interval by this factor before applying its bounds. -/
def backoffMultiplier : Nat := 2

/-- Whether the bounds are reversed or where the doubled interval fits between them. -/
inductive BackoffCase where
  | reversedBounds | belowFloor | withinBounds | aboveCeiling
  deriving DecidableEq

/-- Which interval survives the backoff bounds. -/
inductive BackoffInterval where
  | floor | doubled | ceiling
  deriving DecidableEq

/-- The floor wins when it exceeds the ceiling; otherwise the doubled interval is bounded by both. -/
def backoffBound : BackoffCase → BackoffInterval
  | .reversedBounds | .belowFloor => .floor
  | .withinBounds => .doubled
  | .aboveCeiling => .ceiling

/-- Maintenance lease state at the instant another holder tries to acquire it. -/
inductive LeaseState where
  | missing | expired | live
  deriving DecidableEq

/-- A maintenance family permits one live lease and can replace an expired lease. -/
def acquireLease : LeaseState → Bool
  | .missing | .expired => true
  | .live => false

/-- Renewal requires a live lease owned by the caller. -/
def renewLease : (sameHolder : Bool) → LeaseState → Bool
  | true, .live => true
  | false, .live | false, .missing | false, .expired | true, .missing | true, .expired => false

/-- Who holds the lease a caller is releasing. -/
inductive LeaseHolder where
  | caller | other | missing
  deriving DecidableEq

/-- Release deletes a lease only when it belongs to the caller. -/
def releaseLease : LeaseHolder → Bool
  | .caller => true
  | .other | .missing => false

/-- Seconds a maintenance lease lasts since the last database renewal. -/
def maintenanceLeaseSeconds : Nat := 60

/-- Seconds between renewals of a maintenance lease. -/
def maintenanceRenewSeconds : Nat := 20

/-- Runtime features in the vocabulary shared by startup and control. -/
inductive Feature where
  | mcp | spiiceWorkers | embeddingWorker | outboxPublisher | publishedRecordPrune | objectPurge
  deriving DecidableEq

/-- The configuration or contribution needed to start a feature. -/
inductive StartRequirement where
  | bindAddress | linkedWorker | embeddingRouter | outboxConsumer | retention | objectStore | positivePurgeInterval
  deriving DecidableEq

/-- Start a feature when every one of its requirements is present. -/
def featureRequirements : Feature → List StartRequirement
  | .mcp => [.bindAddress]
  | .spiiceWorkers => [.linkedWorker]
  | .embeddingWorker => [.embeddingRouter]
  | .outboxPublisher => [.outboxConsumer]
  | .publishedRecordPrune => [.outboxConsumer, .retention]
  | .objectPurge => [.objectStore, .positivePurgeInterval]

/-- The fixed order of the feature decisions reported per boot. -/
def bootReportOrder : List Feature :=
  [.mcp, .spiiceWorkers, .embeddingWorker, .outboxPublisher, .publishedRecordPrune, .objectPurge]

/-- Only started embedding, outbox and object purge features hand out a pause control. -/
def claimControl : Feature → (started : Bool) → Bool
  | .embeddingWorker, true | .outboxPublisher, true | .objectPurge, true => true
  | .mcp, true | .spiiceWorkers, true | .publishedRecordPrune, true
  | .mcp, false | .spiiceWorkers, false | .embeddingWorker, false | .outboxPublisher, false
  | .publishedRecordPrune, false | .objectPurge, false => false

/-- A stopped drain dominates, uniform running or parked drains agree and other combinations are quiescing. -/
def jointGateStatus : GateStatus → GateStatus → GateStatus
  | .stopped, .running | .stopped, .quiescing | .stopped, .quiescent | .stopped, .stopped
  | .running, .stopped | .quiescing, .stopped | .quiescent, .stopped => .stopped
  | .running, .running => .running
  | .quiescent, .quiescent => .quiescent
  | .running, .quiescing | .quiescing, .running | .running, .quiescent | .quiescent, .running
  | .quiescing, .quiescing | .quiescent, .quiescing | .quiescing, .quiescent => .quiescing

/-- The listener choice for a runtime with MCP configured. -/
inductive ServeMode where
  | build | run
  deriving DecidableEq

/-- Build hands out a router and run binds a listener when MCP is configured. -/
def bindsListener : ServeMode → (configured : Bool) → Bool
  | .run, true => true
  | .run, false | .build, true | .build, false => false

/-- A running listener is ready while its runtime is active and the database answers. -/
def ready : (shuttingDown : Bool) → (databaseAvailable : Bool) → Bool
  | false, true => true
  | false, false | true, false | true, true => false

/-- How an observed worker exit is reported. -/
inductive WorkerExitReport where
  | degraded | joinDiagnostic | normal
  deriving DecidableEq

/-- Premature worker exit is reported as degraded without stopping or restarting its peers. -/
def reportWorkerExit : (shuttingDown : Bool) → (failedJoin : Bool) → WorkerExitReport
  | false, false | false, true => .degraded
  | true, true => .joinDiagnostic
  | true, false => .normal

/-- Fields projected for an ended worker in anonymous readiness. -/
inductive EndedWorkerField where
  | staticLabel | count
  deriving DecidableEq

/-- Anonymous readiness groups ended workers by their first-ended label and shows each label's count. -/
def endedWorkerProjection : List EndedWorkerField := [.staticLabel, .count]

/-- Ordered responsibilities when a runtime explicitly shuts down. -/
inductive ShutdownStep where
  | cancel | drainListener | joinWorkers
  deriving DecidableEq

/-- Cancellation propagates to child workers and listener before explicit shutdown finishes joining workers. -/
def shutdownOrder : List ShutdownStep := [.cancel, .drainListener, .joinWorkers]

/-- The possible outcomes when a contributed worker is asked to stop. -/
inductive ContributedWorkerExit where
  | joined | graceElapsed
  deriving DecidableEq

/-- Abort a contributed task when its join exceeds the cancellation grace. -/
def contributedWorkerShutdown : ContributedWorkerExit → Bool
  | .joined => false
  | .graceElapsed => true

/-- Seconds a contributed worker has to finish after runtime cancellation. -/
def workerShutdownGraceSeconds : Nat := 5

/-- The components that use the metrics API or install its implementation. -/
inductive MetricComponent where
  | library | servingHost
  deriving DecidableEq

/-- The serving host owns the metrics SDK and exporter; libraries use the instruments API. -/
def installsMetricSdk : MetricComponent → Bool
  | .servingHost => true
  | .library => false

/-- Stages of the stock host's metric provider lifetime. -/
inductive MeterStep where
  | installProvider | serveRuntime | shutdownProvider
  deriving DecidableEq

/-- A configured OTLP provider wraps the runtime lifetime. -/
def meterLifecycle : List MeterStep := [.installProvider, .serveRuntime, .shutdownProvider]

/-- The order in which the stock host chooses its service name. -/
inductive ServiceNameSource where
  | explicit | resourceAttributes | stock
  deriving DecidableEq

/-- An explicit service name takes precedence over resource attributes and then the stock name. -/
def serviceNamePrecedence : List ServiceNameSource := [.explicit, .resourceAttributes, .stock]

/-- The stock host's service name when the environment supplies none. -/
def defaultServiceName : String := "proxiima-mcp"

/-- A collection starts a sample only if no read is outstanding. -/
def requestSample : (collected : Bool) → (outstanding : Bool) → Bool
  | true, false => true
  | true, true | false, false | false, true => false

/-- Possible outcomes of an in-flight metric read. -/
inductive SampleOutcome where
  | success | failure | shutdown
  deriving DecidableEq

/-- A successful metrics read stores the new sample, failure clears it and shutdown cancels the read. -/
inductive SampleAction where
  | store | clear | cancelRead
  deriving DecidableEq

/-- The action taken on the in-flight metric read. -/
def finishSample : SampleOutcome → SampleAction
  | .success => .store
  | .failure => .clear
  | .shutdown => .cancelRead

/-- The tool profiles the stock host accepts. -/
inductive ToolProfile where
  | memory | full
  deriving DecidableEq

/-- Mandatory served reads take precedence over a stock profile's selection. -/
def servedRead : (mandatory : Bool) → (selected : Bool) → Bool
  | true, true | true, false | false, true => true
  | false, false => false

/-- The stage events recorded when stage timing is enabled. -/
inductive StageEvent where
  | start | finish | cancel
  deriving DecidableEq

/-- The possible results of dropping a timed future's guard. -/
inductive StageEnd where
  | finished | cancelled
  deriving DecidableEq

/-- Dropping the timing guard ends the trace whether the awaited future finished or was cancelled. -/
def stageTiming : StageEnd → List StageEvent
  | .finished => [.start, .finish]
  | .cancelled => [.start, .cancel]

/-- The tracing target for opt-in stage timing. -/
def stageTimingTarget : String := "proxiima::stage_timing"

end Proxiima.Operations
