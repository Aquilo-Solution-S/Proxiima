import Causa.Prelude
import Proxiima.OwnerAuthority

/-! Causa — Owner -/

namespace Causa

open Proxiima.OwnerAuthority (RolePreset rolePresets)

/-- The opaque identity of a user, agent or service actor. -/
structure User where
  private mk ::
  private token : String
  deriving DecidableEq, Repr

/-- The synthetic identity used by closed proof witnesses. -/
instance : Inhabited User := ⟨⟨"causa-kernel-witness"⟩⟩

/-- The memory kinds ranked by `Proxiima.OwnerAuthority.coveringCeiling`. -/
abbrev AccessKind := Proxiima.Memory.Kind

/-- The Goal capability used by the proof lattice. -/
abbrev GoalAccess := Proxiima.OwnerAuthority.GoalAccess

/-- The lower Goal capability. -/
def GoalAccess.meet (x y : GoalAccess) : GoalAccess :=
  if Proxiima.OwnerAuthority.GoalAccess.rank x ≤ Proxiima.OwnerAuthority.GoalAccess.rank y then x else y

/-- The higher Goal capability. -/
def GoalAccess.join (x y : GoalAccess) : GoalAccess :=
  if Proxiima.OwnerAuthority.GoalAccess.rank x ≤ Proxiima.OwnerAuthority.GoalAccess.rank y then y else x

/-- The capabilities supplied by a resolved membership. -/
structure Role where
  read  : Nat
  write : Nat
  goals : GoalAccess
  administer : Bool
  manage : Bool
  write_le_read : write ≤ read

def Role.mayRead  (x : Role) (k : AccessKind) : Prop := (Proxiima.OwnerAuthority.coveringCeiling k).rank ≤ x.read
def Role.mayWrite (x : Role) (k : AccessKind) : Prop := (Proxiima.OwnerAuthority.coveringCeiling k).rank ≤ x.write
/-- The capability to read the owner's Goals. -/
def Role.readsGoals (x : Role) : Prop := x.goals.canRead = true
/-- The capability to work on the owner's Goals. -/
def Role.worksGoals (x : Role) : Prop := x.goals.canWork = true
/-- The capability to administer an owner. -/
def Role.administers (x : Role) : Prop := x.administer = true
/-- The capability to change membership and roles. -/
def Role.manages (x : Role) : Prop := x.manage = true

/-- Goal work includes Goal read access. -/
theorem Role.worksGoals_readsGoals (x : Role) (h : x.worksGoals) : x.readsGoals := by
  cases hgoals : x.goals <;>
    simp_all [Role.worksGoals, Role.readsGoals, Proxiima.OwnerAuthority.GoalAccess.canWork,
      Proxiima.OwnerAuthority.GoalAccess.canRead]

/-- The numerical proof projection of a ledger role preset. -/
def Role.ofPreset (preset : RolePreset) : Role :=
  ⟨preset.read.rank, preset.write.rank, preset.goals, preset.administers, preset.manages, by
    obtain ⟨read, write, _, _, _, ok⟩ := preset
    change write.rank ≤ read.rank
    cases read <;> cases write <;> first | decide | contradiction⟩

/-- The role derived from personal ownership. -/
def Role.personal : Role := .ofPreset (rolePresets .personal)
/-- The memory viewer role. -/
def Role.viewer : Role := .ofPreset (rolePresets (.member .viewer))
/-- The Fact ingest role. -/
def Role.ingest : Role := .ofPreset (rolePresets (.member .ingest))
/-- The memory editor role. -/
def Role.editor : Role := .ofPreset (rolePresets (.member .editor))
/-- The unrestricted group role. -/
def Role.admin : Role := .ofPreset (rolePresets (.member .admin))

/-- The intersection of capabilities used to cap inherited authority. -/
def Role.meet (x y : Role) : Role :=
  ⟨min x.read y.read, min x.write y.write, x.goals.meet y.goals,
    x.administer && y.administer, x.manage && y.manage, by
    have hx := x.write_le_read; have hy := y.write_le_read; omega⟩

/-- The union of capabilities used to combine membership paths. -/
def Role.join (x y : Role) : Role :=
  ⟨max x.read y.read, max x.write y.write, x.goals.join y.goals,
    x.administer || y.administer, x.manage || y.manage, by
    have hx := x.write_le_read; have hy := y.write_le_read; omega⟩

/-- Preservation of capability bounds under a role cap. -/
theorem Role.meet_capped (x cap : Role) :
    ((x.meet cap).worksGoals → cap.worksGoals) ∧
    ((x.meet cap).administers → cap.administers) ∧
    ((x.meet cap).manages → cap.manages) := by
  refine ⟨?_, ?_, ?_⟩
  · simp only [Role.worksGoals, Role.meet, GoalAccess.meet]
    generalize x.goals = a
    generalize cap.goals = c
    cases a <;> cases c <;> simp [Proxiima.OwnerAuthority.GoalAccess.rank,
      Proxiima.OwnerAuthority.GoalAccess.canWork]
  · simp only [Role.administers, Role.meet, Bool.and_eq_true]
    exact fun h => h.2
  · simp only [Role.manages, Role.meet, Bool.and_eq_true]
    exact fun h => h.2

/-- Commutativity of the Goal capability meet. -/
theorem GoalAccess.meet_comm (x y : GoalAccess) : x.meet y = y.meet x := by
  cases x <;> cases y <;> rfl
theorem GoalAccess.meet_assoc (x y z : GoalAccess) :
    (x.meet y).meet z = x.meet (y.meet z) := by
  cases x <;> cases y <;> cases z <;> rfl
theorem GoalAccess.meet_join_distrib (x y z : GoalAccess) :
    x.meet (y.join z) = (x.meet y).join (x.meet z) := by
  cases x <;> cases y <;> cases z <;> rfl

/-- Role equality determined by capability fields. -/
theorem Role.ext {x y : Role} (hr : x.read = y.read) (hw : x.write = y.write)
    (hg : x.goals = y.goals) (ha : x.administer = y.administer)
    (hm : x.manage = y.manage) : x = y := by
  cases x; cases y
  simp only at hr hw hg ha hm
  subst hr hw hg ha hm
  rfl

theorem Role.meet_comm (x y : Role) : x.meet y = y.meet x :=
  Role.ext (Nat.min_comm _ _) (Nat.min_comm _ _) (GoalAccess.meet_comm _ _)
    (Bool.and_comm _ _) (Bool.and_comm _ _)

theorem Role.meet_assoc (x y z : Role) : (x.meet y).meet z = x.meet (y.meet z) :=
  Role.ext (Nat.min_assoc _ _ _) (Nat.min_assoc _ _ _) (GoalAccess.meet_assoc _ _ _)
    (Bool.and_assoc _ _ _) (Bool.and_assoc _ _ _)

/-- Distributivity of a role cap over a membership join. -/
theorem Role.meet_join_distrib (x y z : Role) :
    x.meet (y.join z) = (x.meet y).join (x.meet z) :=
  Role.ext (by simp only [Role.join, Role.meet]; omega)
    (by simp only [Role.join, Role.meet]; omega)
    (GoalAccess.meet_join_distrib _ _ _)
    (by simp only [Role.join, Role.meet]
        cases x.administer <;> cases y.administer <;> cases z.administer <;> rfl)
    (by simp only [Role.join, Role.meet]
        cases x.manage <;> cases y.manage <;> cases z.manage <;> rfl)

/-- The resolved group membership view. -/
abbrev Group : Type := User → Option Role

/-- The group whose roles govern owner-scoped authorization. -/
abbrev Owner : Type := Group

/-- The group derived from a personal identity. -/
def Owner.ofUser (u : User) : Owner :=
  fun x => if x = u then some Role.personal else none

/-- Recognition of a personal group. -/
def Owner.isPersonal (o : Owner) : Prop := ∃ u : User, o = Owner.ofUser u

/-- Visibility through owner membership. -/
def visible (o : Owner) (requester : User) : Prop := o requester ≠ none

instance (o : Owner) (u : User) : Decidable (visible o u) :=
  match h : o u with
  | some _ => .isTrue (by simp [visible, h])
  | none   => .isFalse (by simp [visible, h])

/-- The membership of a subgroup under a role cap. -/
def Group.mount (g : Group) (cap : Role) : Group :=
  fun u => (g u).map (fun x => Role.meet x cap)

/-- The combined membership of two groups. -/
def Group.union (a b : Group) : Group :=
  fun u =>
    match a u, b u with
    | some x, some y => some (Role.join x y)
    | some x, none   => some x
    | none,   some y => some y
    | none,   none   => none

/-- Membership removal for a single user. -/
def Group.drop (g : Group) (u : User) : Group :=
  fun x => if x = u then none else g x

end Causa
