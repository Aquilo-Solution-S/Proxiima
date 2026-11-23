import Proxiima.Identity

/-! # Owner authority -/

namespace Proxiima.OwnerAuthority

/-- The highest memory kind a role can read or write. -/
inductive AccessCeiling where
  | none | fact | abstraction | perspective
  deriving DecidableEq

/-- The ordinal of a ceiling, with zero covering no memory. -/
def AccessCeiling.rank : AccessCeiling → Nat
  | .none => 0
  | .fact => 1
  | .abstraction => 2
  | .perspective => 3

/-- The lowest ceiling covering each of the three memory kinds. -/
def coveringCeiling : Proxiima.Memory.Kind → AccessCeiling
  | .Fact => .fact
  | .Abstraction => .abstraction
  | .Perspective => .perspective

/-- A direction-specific ceiling covers the selected memory kind and every lower kind. -/
def covers : AccessCeiling → Proxiima.Memory.Kind → Bool
  | .perspective, .Fact | .perspective, .Abstraction | .perspective, .Perspective
  | .abstraction, .Fact | .abstraction, .Abstraction | .fact, .Fact => true
  | .none, .Fact | .none, .Abstraction | .none, .Perspective
  | .fact, .Abstraction | .fact, .Perspective | .abstraction, .Perspective => false

/-- Goal access is independent of memory access. -/
inductive GoalAccess where
  | none | reads | works
  deriving DecidableEq

/-- The ordering used to cap or join Goal access. -/
def GoalAccess.rank : GoalAccess → Nat
  | .none => 0
  | .reads => 1
  | .works => 2

/-- An owner role with Goal access reads the owner's Goals. -/
def GoalAccess.canRead : GoalAccess → Bool
  | .none => false
  | .reads | .works => true

/-- Only the work capability admits owner Goal work. -/
def GoalAccess.canWork : GoalAccess → Bool
  | .none | .reads => false
  | .works => true

/-- An owner role cannot write above its read ceiling. -/
def ceilingAdmitted : (read write : AccessCeiling) → Bool
  | .none, .none | .fact, .none | .fact, .fact
  | .abstraction, .none | .abstraction, .fact | .abstraction, .abstraction
  | .perspective, .none | .perspective, .fact | .perspective, .abstraction
  | .perspective, .perspective => true
  | .none, .fact | .none, .abstraction | .none, .perspective
  | .fact, .abstraction | .fact, .perspective | .abstraction, .perspective => false

/-- The role's memory ceilings, Goal access and independent administration and management bits. -/
structure RolePreset where
  read : AccessCeiling
  write : AccessCeiling
  goals : GoalAccess
  administers : Bool
  manages : Bool
  ok : ceilingAdmitted read write = true

/-- The names accepted for direct membership relations. -/
inductive Relation where
  | admin | editor | viewer | ingest
  deriving DecidableEq

/-- The lowercase storage label of a membership relation. -/
def relationLabel : Relation → String
  | .admin => "admin"
  | .editor => "editor"
  | .viewer => "viewer"
  | .ingest => "ingest"

/-- Whether member-add supplies a relation. -/
inductive MemberAddRelation where
  | omitted
  | supplied (relation : Relation)
  deriving DecidableEq

/-- Member-add uses the supplied relation and defaults an omitted relation to Viewer. -/
def memberAddRelation : MemberAddRelation → Relation
  | .omitted => .viewer
  | .supplied .admin => .admin
  | .supplied .editor => .editor
  | .supplied .viewer => .viewer
  | .supplied .ingest => .ingest

/-- How a personal role or one of the group presets is acquired. -/
inductive Preset where
  | personal
  | member (relation : Relation)
  deriving DecidableEq

/-- The derived personal role and the available group membership presets. -/
def rolePresets : Preset → RolePreset
  | .personal => ⟨.perspective, .perspective, .works, true, false, rfl⟩
  | .member .admin => ⟨.perspective, .perspective, .works, true, true, rfl⟩
  | .member .editor => ⟨.perspective, .perspective, .works, false, false, rfl⟩
  | .member .viewer => ⟨.perspective, .none, .works, false, false, rfl⟩
  | .member .ingest => ⟨.fact, .fact, .none, false, false, rfl⟩

/-- The key selecting the session's current memory space. -/
def currentSpaceKey : String := "current"

/-- A space key labels the actual row owner, with the current key reserved for the session's bound owner. -/
inductive SpaceKey where
  | current
  | ownerUuid
  deriving DecidableEq

/-- Write selection resolves the space and narrows to its owner before the concrete verb checks its own authority. -/
inductive SpaceWriteStep where
  | resolve
  | narrow
  | authorizeVerb
  deriving DecidableEq

/-- The admission order of a write into a selected space. -/
def spaceWriteOrder : List SpaceWriteStep := [.resolve, .narrow, .authorizeVerb]

/-- A stored owner reference is classified by the owner registry. -/
inductive OwnerKind where
  | personal | group
  deriving DecidableEq

/-- Ordinary admission registers unknown owners as personal, and groups require explicit registration. -/
inductive Registration where
  | admit
  | createGroup
  deriving DecidableEq

/-- The registration state of an owner ID. -/
inductive RegistrationState where
  | unregistered
  | registered (kind : OwnerKind)
  deriving DecidableEq

/-- The first owner registration fixes the kind. Explicit group creation refuses a personal ID. -/
def registerOwner : RegistrationState → Registration → Option OwnerKind
  | .unregistered, .admit | .registered .personal, .admit => some .personal
  | .unregistered, .createGroup | .registered .group, .admit
  | .registered .group, .createGroup => some .group
  | .registered .personal, .createGroup => none

/-- The invariants checked when an edge is written to the group forest. -/
inductive GroupForestInvariant where
  /-- A subgroup has at most one parent. -/
  | singleParent
  /-- A group cannot contain itself. -/
  | distinctEndpoints
  /-- A parent cannot be a descendant of its own child. -/
  | acyclic
  deriving DecidableEq

/-- The invariants of a group containment edge. -/
def groupForest : List GroupForestInvariant := [.singleParent, .distinctEndpoints, .acyclic]

/-- Authorization facts obtained without changing the caller's transaction or treating the facts as permits. -/
inductive AuthorityFact where
  | currentPersonRole
  | directRelations
  | registeredOwnerKind
  | containment
  deriving DecidableEq

/-- An authority session fences the declared targets before returning role, registry and topology facts. -/
def authorityFacts : List AuthorityFact :=
  [.currentPersonRole, .directRelations, .registeredOwnerKind, .containment]

/-- The steps that protect a declared membership authority footprint in a caller-owned read-committed transaction. -/
inductive AuthorityFenceStep where
  | lockGroupTrees
  /-- Verify that the captured roots still match after locking the trees. -/
  | verifyRoots
  | lockOwnerKinds
  | lockPersonProjections
  /-- Re-read direct groups and refuse any root outside the locked footprint. -/
  | verifyDirectGroupRoots
  deriving DecidableEq

/-- Membership authority fence steps, in order. -/
def authorityFenceOrder : List AuthorityFenceStep :=
  [.lockGroupTrees, .verifyRoots, .lockOwnerKinds, .lockPersonProjections, .verifyDirectGroupRoots]

/-- The closed effects of group membership administration. -/
inductive MembershipChange where
  /-- Host-only bootstrap with an atomic refusal when an Admin already exists. -/
  | bootstrapFirstAdmin
  /-- Insert the requested person relation or attach a subgroup under its cap. -/
  | add
  /-- Remove all direct relations for the member. -/
  | removeAll
  /-- Remove only the named relation, succeeding on absence. -/
  | removeOne
  /-- Require the old relation and run remove then add vetoes before replacing it atomically. -/
  | replace
  deriving DecidableEq

/-- The role gates for a membership change once the member kind is resolved. -/
inductive MembershipConsent where
  | hostBootstrap
  | parent
  /-- Administration and management on the parent, and management on the subgroup. -/
  | bothGroups
  /-- Administration and management on the selected endpoint, with the parent tried first. -/
  | eitherGroup
  deriving DecidableEq

/-- Adding or changing a group edge needs both groups. Detaching needs either group, and person edits need the parent. -/
def membershipConsent : MembershipChange → OwnerKind → MembershipConsent
  | .bootstrapFirstAdmin, .personal | .bootstrapFirstAdmin, .group => .hostBootstrap
  | .add, .group | .replace, .group => .bothGroups
  | .removeOne, .group | .removeAll, .group => .eitherGroup
  | .add, .personal | .replace, .personal | .removeOne, .personal | .removeAll, .personal => .parent

/-- An engine membership removal or replacement preserves a previously managed group. -/
def preservesManager : (hadManager hasManager : Bool) → Bool
  | true, false => false
  | false, false | false, true | true, true => true

/-- The components of a member-page cursor. -/
inductive MemberPageKey where
  | memberId
  | relation
  deriving DecidableEq

/-- The ordered key of a member page. -/
def memberPageOrder : List MemberPageKey := [.memberId, .relation]

/-- A resolved owner gate checks the effect before running ordered host vetoes and observing the decision. -/
inductive OwnerGateStep where
  | resolve
  | checkEffect
  | hostVeto
  | observe
  deriving DecidableEq

/-- Owner gate steps, in order. -/
def ownerGateOrder : List OwnerGateStep := [.resolve, .checkEffect, .hostVeto, .observe]

/-- The request paths with different owner Goal-work requirements. -/
inductive GoalWorkPath where
  | hostBearer
  | delegated
  | system
  deriving DecidableEq

/-- Goal work requires its own capability, with administration also required for delegated and system work. -/
def goalWorkAdmitted : GoalWorkPath → (works administers : Bool) → Bool
  | .hostBearer, true, true | .hostBearer, true, false
  | .delegated, true, true | .system, true, true => true
  | .hostBearer, false, true | .hostBearer, false, false
  | .delegated, false, true | .delegated, false, false
  | .system, false, true | .system, false, false
  | .delegated, true, false | .system, true, false => false

/-- Owner lists put the subject's owner first and then sort by owner ID. -/
inductive OwnerListKey where
  | personalFirst
  | ownerId
  deriving DecidableEq

/-- Ordering across read, write and Goal owner lists. -/
def ownerListOrder : List OwnerListKey := [.personalFirst, .ownerId]

/-- Restrictions that keep a scoped role grant from widening. -/
inductive ScopeRestriction where
  /-- Expired grants authorize nothing and expiry can only move earlier. -/
  | expiry
  /-- Explicit confinement admits only one owner and cannot add raw group roles. -/
  | confinement
  /-- A verified, sealed scope cannot add raw group roles. -/
  | sealed
  deriving DecidableEq

/-- The endpoint whose role gives consent to an owner transfer. -/
inductive TransferEndpoint where
  | source
  | destination
  deriving DecidableEq

/-- The source of the role offered for transfer consent. -/
inductive TransferRoleSource where
  | personalRole
  | groupRole
  deriving DecidableEq

/-- Consent needed at one endpoint of an owner transfer. -/
inductive TransferConsent where
  | administer
  | administerAndManage
  | refuse
  deriving DecidableEq

/-- A source administers and group endpoints must also manage. Receiving consent requires the group role. -/
def transferConsent : TransferEndpoint → TransferRoleSource → TransferConsent
  | .source, .personalRole => .administer
  | .source, .groupRole | .destination, .groupRole => .administerAndManage
  | .destination, .personalRole => .refuse

/-- Request authority considered when a transfer looks up destination consent. -/
inductive DestinationAuthority where
  | rolePresent
  | eligibleSubjectWithoutRole
  /-- Delegated and verified forwarded requests cannot use stored destination membership. -/
  | ineligibleSubjectWithoutRole
  deriving DecidableEq

/-- The role route used for destination consent. -/
inductive DestinationRoute where
  | requestRole
  | storedMembership
  | refuse
  deriving DecidableEq

/-- Stored membership supplies destination consent only for an eligible subject whose request carries no destination role. -/
def destinationRoute : DestinationAuthority → DestinationRoute
  | .rolePresent => .requestRole
  | .eligibleSubjectWithoutRole => .storedMembership
  | .ineligibleSubjectWithoutRole => .refuse

/-- A memory transfer may reach these admission checks in order. -/
inductive TransferStep where
  | checkEntityKind
  | refuseOwnPersonalDestination
  /-- Find the source among the caller's readable owners, including a selected forgotten version. -/
  | findVisibleSource
  | authorizeSource
  | authorizeDestination
  | checkSchema
  | refuseSameOwner
  | applyVetoes
  | resolveDestinationEmbeddings
  /-- Ensure the destination owner row and move the series in the same transaction. -/
  | moveSeries
  deriving DecidableEq

/-- Owner transfer admission steps, in order. -/
def transferOrder : List TransferStep :=
  [.checkEntityKind, .refuseOwnPersonalDestination, .findVisibleSource, .authorizeSource, .authorizeDestination,
   .checkSchema, .refuseSameOwner, .applyVetoes, .resolveDestinationEmbeddings, .moveSeries]

/-- The Memory series eligibility checked by transfer admission. -/
inductive TransferClass where
  | schemaRefuses
  | linkedToGoal
  | eligibleMemory
  deriving DecidableEq

/-- A Memory series with Goal causality or a refusal in its schema cannot transfer. -/
def transferAdmitted : TransferClass → Bool
  | .eligibleMemory => true
  | .schemaRefuses | .linkedToGoal => false

/-- The storage checks that keep a captured transfer bound to its original series. -/
inductive TransferFence where
  | lockBothOwners
  | lockSeriesVersions
  | requireSelectedVersion
  | compareHeadOwner
  deriving DecidableEq

/-- A stale capture cannot transfer a replacement series at the same handle. -/
def capturedTransfer : List TransferFence :=
  [.lockBothOwners, .lockSeriesVersions, .requireSelectedVersion, .compareHeadOwner]

end Proxiima.OwnerAuthority
