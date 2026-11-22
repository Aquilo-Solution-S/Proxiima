/-! # Forget and restore -/

namespace Proxiima.ForgetRestore

/-- The lifecycle operations that change an admission's mark. -/
inductive MarkChange where
  | forget
  | restore
  deriving DecidableEq

/-- The forgotten flag written by an admitted lifecycle operation. -/
def markAfter : MarkChange → Bool
  | .forget => true
  | .restore => false

/-- The stored parts preserved by forgetting, with exact pins preserved under `Causa.principle_5_no_transition_retargets_a_pin`. -/
inductive RetainedPart where
  | admission
  | stampedSidecars
  | sketch
  | requestReceipt
  | externalOrigins
  | citedObjects
  deriving DecidableEq

/-- Forget leaves the original admission and the data needed to restore or replay it in place. -/
def retainedOnForget : List RetainedPart :=
  [.admission, .stampedSidecars, .sketch, .requestReceipt, .externalOrigins, .citedObjects]

/-- Visibility within authorized owner scope, including sidecars and external origins reached through the admission. -/
def visibleInScope : (lifecycle forgotten : Bool) → Bool
  | true, true | true, false | false, false => true
  | false, true => false

/-- Lifecycle writes require the selected owner and a Memory kind within the permit's write ceiling. -/
def mutationReach : (selectedOwner withinWriteLimit : Bool) → Bool
  | true, true => true
  | true, false | false, true | false, false => false

/-- Restore refuses a retired Fact schema. -/
inductive RestoreRefusal where
  | retiredSchema
  deriving DecidableEq

/-- The effect of restore on an admission that passed the schema check. -/
inductive RestoreAction where
  | unchanged
  | rebuild
  deriving DecidableEq

/-- Schema retirement refuses restore before the live-row no-op or the forgotten-row rebuild. -/
def restoreAdmission : (retired forgotten : Bool) → Except RestoreRefusal RestoreAction
  | true, true | true, false => .error .retiredSchema
  | false, false => .ok .unchanged
  | false, true => .ok .rebuild

/-- The steps of restoring a forgotten admission after admission checks. -/
inductive RestoreStep where
  | clearMark
  | rebuildStampedProjectionsInStoredLanguage
  /-- Only for embeddable schemas and the spaces in the owner's current route. -/
  | enqueueRoutedEmbeddings
  | repairHead
  deriving DecidableEq

/-- Restore rebuilds derived state under the same transaction without republishing cancelled deliveries. -/
def restoreSteps : List RestoreStep :=
  [.clearMark, .rebuildStampedProjectionsInStoredLanguage, .enqueueRoutedEmbeddings, .repairHead]

/-- Which surviving version is the head after a lifecycle change. -/
inductive HeadChoice where
  | newestLive
  | newestForgotten
  deriving DecidableEq

/-- Prefer the newest live head, keep a forgotten head while only forgotten versions remain, and remove an empty series head. -/
def repairHead : (hasLive hasForgotten : Bool) → Option HeadChoice
  | true, true | true, false => some .newestLive
  | false, true => some .newestForgotten
  | false, false => none

/-- The lock footprint stages, shared by forget and restore. -/
inductive FootprintStep where
  | lockSourceAndNonFactDependerHandles
  | lockSourceAndNonFactDependerVersions
  | revalidateFootprint
  deriving DecidableEq

/-- Lock both live and forgotten origin and reference dependers and retry on footprint growth. -/
def footprintOrder : List FootprintStep :=
  [.lockSourceAndNonFactDependerHandles, .lockSourceAndNonFactDependerVersions, .revalidateFootprint]

/-- The order of derived-state deletes during forget. -/
inductive DerivedDelete where
  | searchProjections
  | embeddingJobs
  | otherGeneratedLegs
  deriving DecidableEq

/-- Forget deletes linked search projections and takes embedding jobs before the other generated deletes. -/
def derivedDeleteOrder : List DerivedDelete := [.searchProjections, .embeddingJobs, .otherGeneratedLegs]

/-- Surface declarations as classified by their Memory key and cascade evidence. -/
inductive SurfaceRule where
  | keep
  | retain (keyed : Bool)
  | retainCascadedDetail (matchingMemoryCascade : Bool)
  | delete (hasCascade keyed : Bool)
  deriving DecidableEq

/-- The leg resolved from a declared forget rule. -/
inductive SurfaceLeg where
  | kept
  | marked
  | cascaded
  | deleted
  | unreachable
  deriving DecidableEq

/-- Retained surfaces require a reachable key or matching Memory cascade; deletes prefer cascade evidence to a generated statement. -/
def classifySurface : SurfaceRule → SurfaceLeg
  | .keep => .kept
  | .retain true | .retainCascadedDetail true => .marked
  | .retain false | .retainCascadedDetail false | .delete false false => .unreachable
  | .delete true true | .delete true false => .cascaded
  | .delete false true => .deleted

/-- Whether a resolved leg emits a generated delete. -/
def generatesDelete : SurfaceLeg → Bool
  | .deleted => true
  | .kept | .marked | .cascaded | .unreachable => false

/-- Whether a declared surface has a leg the registry can freeze. -/
def reachableLeg : SurfaceLeg → Bool
  | .kept | .marked | .cascaded | .deleted => true
  | .unreachable => false

end Proxiima.ForgetRestore
