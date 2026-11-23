import Proxiima.Goals
import Proxiima.OwnerAuthority

/-! # Goal collaboration -/

namespace Proxiima.GoalCollaboration

open Proxiima.Goals (GoalState)
open Proxiima.OwnerAuthority (GoalAccess)

/-- A relationship with a Goal, classified by the scope of access it conveys. -/
inductive Relationship where
  | context
  | publication
  | workBinding
  deriving DecidableEq

/-- The identity at which a Goal audience can read Memory. -/
inductive AudienceReadTarget where
  | admission
  | series
  deriving DecidableEq

/-- Context conveys no read; publication selects one admission and a work binding selects a series. -/
def audienceReadTarget : Relationship → Option AudienceReadTarget
  | .context => none
  | .publication => some .admission
  | .workBinding => some .series

/-- Why an audience grant's role is refused. -/
inductive GrantRefusal where
  | ownerAdministration
  deriving DecidableEq

/-- An audience grant raises absent Goal access to read and refuses owner administration. -/
def storedGuestGoalAccess : (administers : Bool) → GoalAccess → Except GrantRefusal GoalAccess
  | true, .none | true, .reads | true, .works => .error .ownerAdministration
  | false, .none | false, .reads => .ok .reads
  | false, .works => .ok .works

/-- An owner role contributes at most Goal read to the audience route unless it administers. -/
def ownerGoalAccessOnAudienceRoute : (administers : Bool) → GoalAccess → GoalAccess
  | true, .none | false, .none => .none
  | true, .reads | false, .reads | false, .works => .reads
  | true, .works => .works

/-- The registered kind of an audience grant's subject. -/
inductive GrantSubjectKind where
  | person
  | group
  deriving DecidableEq

/-- A group audience grant cannot convey Goal management. -/
def grantSubjectAdmitted : GrantSubjectKind → (manages : Bool) → Bool
  | .person, true | .person, false | .group, false => true
  | .group, true => false

/-- A grant is in force while it is unexpired and of the current Goal controller epoch. -/
def grantInForce : (currentEpoch unexpired : Bool) → Bool
  | true, true => true
  | true, false | false, true | false, false => false

/-- A newer independently readable owner head takes precedence over the latest audience-visible version. -/
def preferOwnerHead : (newer independentlyReadable : Bool) → Bool
  | true, true => true
  | true, false | false, true | false, false => false

/-- A request to add or withdraw exact publication or series binding consent. -/
inductive ConsentChange where
  | admit
  | withdraw
  deriving DecidableEq

/-- Adding consent requires Goal management and source administration; either may withdraw it. -/
def sharingConsentAdmitted : ConsentChange → (goalManager sourceAdministrator : Bool) → Bool
  | .admit, true, true
  | .withdraw, true, true | .withdraw, true, false | .withdraw, false, true => true
  | .admit, true, false | .admit, false, true | .admit, false, false
  | .withdraw, false, false => false

/-- Whether the request reads through a work binding or writes into the bound series. -/
inductive BoundWorkAccess where
  | read
  | write
  deriving DecidableEq

/-- A binding can be read in any Goal state, and the series can be worked while the Goal is active. -/
def boundWorkStateAdmitted : GoalState → BoundWorkAccess → Bool
  | .Active, .read | .Paused, .read | .Achieved, .read | .Abandoned, .read
  | .Active, .write => true
  | .Paused, .write | .Achieved, .write | .Abandoned, .write => false

/-- The source owner's permission recorded on one exact publication. -/
inductive PublicationUse where
  | Read
  | Fork
  deriving DecidableEq

/-- The destination path of a fork. -/
inductive ForkDestination where
  | sameOwner
  | differentOwner
  | workingGoal
  deriving DecidableEq

/-- Source administration or a direct same-owner destination suffices without publication consent. -/
def forkNeedsPublication : ForkDestination → (sourceAdministrator : Bool) → Bool
  | .sameOwner, true | .sameOwner, false
  | .differentOwner, true | .workingGoal, true => false
  | .differentOwner, false | .workingGoal, false => true

/-- A permitted Goal work unit command may write its declared Facts without pins or citations. -/
def companionFactsAdmitted : (commandPermitted hasPins hasCitation : Bool) → Bool
  | true, false, false => true
  | true, true, true | true, true, false | true, false, true
  | false, true, true | false, true, false | false, false, true | false, false, false => false

end Proxiima.GoalCollaboration
