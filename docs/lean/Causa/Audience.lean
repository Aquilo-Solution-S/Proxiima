/-
Causa — Audience

Access is ONE rule: a role resolved for the caller, granted on an owner or on a
smaller object, where the subject of a grant may be a group. This file states
that rule over the resolved `Owner` of `Causa.Owner`; it adds no primitive. An
owner's own audience is its resolved group (its tree). Grant lists exist only on
smaller objects: one Goal, an exact version, a series. A capability on such an
object is held iff the owner gives it, or a grant gives it under the current
owner state (`flattened_gives`).

A grant's subject is a person or a group named by its owner id, nothing else can
be written. The subject is resolved when the question is asked (`Grant.gives`),
so removing a person from a group ends what a grant to the group gave them while
the grant stays the same value (`drop_ends_grant`). `flattened` is the read
path storage uses; it is not a subject.
-/

import Causa.Owner
import Causa.Authorization

namespace Causa

-- ============================================================
-- Capabilities a role can give
-- ============================================================

/-- What a role can give: each is one of the role's predicates. On a Goal
    grant, `manages` means changing that Goal's audience only, never the owning
    group's membership. -/
inductive Capability where
  | read (k : AccessKind)
  | write (k : AccessKind)
  | readsGoals
  | worksGoals
  | administers
  | manages

/-- The role has the capability. -/
def Role.has (r : Role) : Capability → Prop
  | .read k => r.mayRead k
  | .write k => r.mayWrite k
  | .readsGoals => r.readsGoals
  | .worksGoals => r.worksGoals
  | .administers => r.administers
  | .manages => r.manages

/-- The meet has a capability iff both roles have it. -/
theorem Role.has_meet (x y : Role) (c : Capability) :
    (x.meet y).has c ↔ x.has c ∧ y.has c := by
  cases c with
  | read k => simp only [Role.has, Role.mayRead, Role.meet]; omega
  | write k => simp only [Role.has, Role.mayWrite, Role.meet]; omega
  | readsGoals =>
    simp only [Role.has, Role.readsGoals, Role.meet]
    cases x.goals <;> cases y.goals <;> simp [GoalAccess.meet, GoalAccess.rank]
  | worksGoals =>
    simp only [Role.has, Role.worksGoals, Role.meet]
    cases x.goals <;> cases y.goals <;> simp [GoalAccess.meet, GoalAccess.rank]
  | administers => simp [Role.has, Role.administers, Role.meet]
  | manages => simp [Role.has, Role.manages, Role.meet]

/-- The join has a capability iff either role has it. -/
theorem Role.has_join (x y : Role) (c : Capability) :
    (x.join y).has c ↔ x.has c ∨ y.has c := by
  cases c with
  | read k => simp only [Role.has, Role.mayRead, Role.join]; omega
  | write k => simp only [Role.has, Role.mayWrite, Role.join]; omega
  | readsGoals =>
    simp only [Role.has, Role.readsGoals, Role.join]
    cases x.goals <;> cases y.goals <;> simp [GoalAccess.join, GoalAccess.rank]
  | worksGoals =>
    simp only [Role.has, Role.worksGoals, Role.join]
    cases x.goals <;> cases y.goals <;> simp [GoalAccess.join, GoalAccess.rank]
  | administers => simp [Role.has, Role.administers, Role.join]
  | manages => simp [Role.has, Role.manages, Role.join]

/-- A role that may write a kind may read it. -/
theorem Role.has_write_read (r : Role) (k : AccessKind) :
    r.has (.write k) → r.has (.read k) := fun h =>
  Nat.lt_of_lt_of_le h r.write_le_read

/-- The top role has every capability. -/
theorem Role.admin_has (c : Capability) : Role.admin.has c := by
  cases c with
  | read k => show k.rank < 3; cases k <;> decide
  | write k => show k.rank < 3; cases k <;> decide
  | readsGoals => show GoalAccess.works ≠ GoalAccess.none; decide
  | worksGoals => rfl
  | administers => rfl
  | manages => rfl

/-- Meeting with the top role loses no capability. (Not an equality of roles:
    ceilings are unbounded naturals, and the meet clamps them to 3.) -/
theorem Role.has_meet_admin (r : Role) (c : Capability) :
    (Role.admin.meet r).has c ↔ r.has c := by
  rw [Role.has_meet]
  exact ⟨fun h => h.2, fun h => ⟨Role.admin_has c, h⟩⟩

-- ============================================================
-- Groups give capabilities
-- ============================================================

/-- The user holds, in the group, a role that has the capability. -/
def Group.gives (g : Group) (u : User) (c : Capability) : Prop :=
  ∃ r, g u = some r ∧ r.has c

/-- Whoever a group gives anything to is a member. -/
theorem Group.visible_of_gives {g : Group} {u : User} {c : Capability}
    (h : g.gives u c) : visible g u := by
  obtain ⟨r, hr, _⟩ := h
  simp [visible, hr]

/-- A group gives the user `d` wherever each role that has `c` has `d`. -/
theorem Group.gives_mono {g : Group} {u : User} {c d : Capability}
    (hcd : ∀ r : Role, r.has c → r.has d) (h : g.gives u c) : g.gives u d := by
  obtain ⟨r, hr, hc⟩ := h
  exact ⟨r, hr, hcd r hc⟩

/-- A mount gives what the group gives and the cap has. -/
theorem Group.gives_mount (g : Group) (cap : Role) (u : User) (c : Capability) :
    (g.mount cap).gives u c ↔ g.gives u c ∧ cap.has c := by
  unfold Group.gives Group.mount
  cases g u with
  | none => simp
  | some x => simp [Role.has_meet]

/-- A union gives what either side gives. -/
theorem Group.gives_union (a b : Group) (u : User) (c : Capability) :
    (a.union b).gives u c ↔ a.gives u c ∨ b.gives u c := by
  unfold Group.gives Group.union
  cases a u <;> cases b u <;> simp [Role.has_join]

-- ============================================================
-- Subjects and grants
-- ============================================================

/-- The group whose one member is `u`. Its `Role.admin` is the top role only up
    to capabilities (`Role.has_meet_admin`). -/
def Group.only (u : User) : Group :=
  fun x => if x = u then some Role.admin else none

/-- Who a grant is for: one person, or a group named by its owner id. -/
inductive Subject where
  | person (u : User)
  | group (o : OwnerRef)

/-- The one subject resolver: a person is the one-member group, a group is the
    owner its id names. Storage names both by an owner id, and a user's own id
    denotes the person. As an OWNER that id resolves to the personal group,
    which cannot manage (`personal_resolves`); as a SUBJECT it does not, or
    every grant to a person would lose `manage`. -/
def OwnerState.subject (s : OwnerState) : Subject → Group
  | .person u => Group.only u
  | .group o => s.resolve o

/-- A grant names a subject and the role it hands that subject. The object the
    grant is ON (one Goal, an exact version, a series) is not a field. -/
structure Grant where
  subject : Subject
  role : Role

/-- What a grant gives under the current owner state. The subject is resolved
    when the question is asked; a grant never stores a resolved membership. -/
def Grant.gives (g : Grant) (s : OwnerState) (u : User) (c : Capability) : Prop :=
  ((s.subject g.subject).mount g.role).gives u c

/-- A grant gives a capability iff its role has it and its subject gives it. -/
theorem Grant.gives_iff (g : Grant) (s : OwnerState) (u : User) (c : Capability) :
    g.gives s u c ↔ g.role.has c ∧ (s.subject g.subject).gives u c := by
  unfold Grant.gives
  rw [Group.gives_mount]
  exact and_comm

/-- The flattened lookup storage reads: the owner's group joined with each
    grant's resolved subject mounted at the grant's role. -/
def flattened (s : OwnerState) (o : Owner) (gs : List Grant) : Group :=
  gs.foldl (fun acc g => acc.union ((s.subject g.subject).mount g.role)) o

/-- The flattened lookup gives exactly what the rule gives: the owner's group
    gives it, or one grant does. -/
theorem flattened_gives (s : OwnerState) (o : Owner) (gs : List Grant)
    (u : User) (c : Capability) :
    (flattened s o gs).gives u c ↔ o.gives u c ∨ ∃ g, g ∈ gs ∧ g.gives s u c := by
  induction gs generalizing o with
  | nil => simp [flattened]
  | cons g gs ih =>
    have h := ih (o.union ((s.subject g.subject).mount g.role))
    simp only [flattened, List.foldl_cons] at h ⊢
    rw [h, Group.gives_union]
    constructor
    · rintro ((ho | hg) | ⟨g', hg', hs'⟩)
      · exact Or.inl ho
      · exact Or.inr ⟨g, List.mem_cons_self _ _, hg⟩
      · exact Or.inr ⟨g', List.mem_cons_of_mem _ hg', hs'⟩
    · rintro (ho | ⟨g', hg', hs'⟩)
      · exact Or.inl (Or.inl ho)
      · cases List.mem_cons.1 hg' with
        | inl e => subst e; exact Or.inl (Or.inr hs')
        | inr m => exact Or.inr ⟨g', m, hs'⟩

-- ============================================================
-- Algebra of nesting
-- ============================================================

/-- Caps along a path compose by meet. -/
theorem Group.mount_mount (g : Group) (a b : Role) :
    (g.mount a).mount b = g.mount (a.meet b) := by
  funext u
  simp only [Group.mount]
  cases g u <;> simp [Role.meet_assoc]

/-- Resolving level by level equals resolving path by path. -/
theorem Group.mount_union (a b : Group) (cap : Role) :
    (a.union b).mount cap = (a.mount cap).union (b.mount cap) := by
  funext u
  simp only [Group.union, Group.mount]
  cases a u <;> cases b u <;>
    simp [Role.meet_comm, Role.meet_join_distrib]

-- ============================================================
-- The person case
-- ============================================================

/-- A grant to one person gives exactly the person the role's capabilities. -/
theorem grant_to_person (s : OwnerState) (u requester : User) (r : Role)
    (c : Capability) :
    (Grant.mk (.person u) r).gives s requester c ↔ requester = u ∧ r.has c := by
  unfold Grant.gives OwnerState.subject
  rw [Group.gives_mount]
  unfold Group.gives Group.only
  by_cases h : requester = u
  · simp [h, Role.admin_has]
  · simp [h]

/-- A member whose team role does not cap the grant holds exactly what a
    directly invited person holds. -/
theorem uncapped_member_as_person (s : OwnerState) (o : OwnerRef) (u : User)
    (x r : Role) (c : Capability) (hmember : s.resolve o u = some x)
    (hcap : r.has c → x.has c) :
    (Grant.mk (.group o) r).gives s u c ↔ (Grant.mk (.person u) r).gives s u c := by
  rw [grant_to_person]
  unfold Grant.gives OwnerState.subject
  rw [Group.gives_mount]
  constructor
  · exact fun h => ⟨rfl, h.2⟩
  · exact fun h => ⟨⟨x, hmember, hcap h.2⟩, h.2⟩

/-- Same members, only the cap differs. -/
theorem only_visible_iff_ofUser (u r : User) :
    visible (Group.only u) r ↔ visible (Owner.ofUser u) r := by
  rw [visible_personal]
  simp only [visible, Group.only]
  split <;> simp_all

-- ============================================================
-- Revocation
-- ============================================================

/-- Dropping a user from a group gives them nothing through it. -/
theorem Group.drop_gives_self (g : Group) (u : User) (c : Capability) :
    ¬ (g.drop u).gives u c := by
  rintro ⟨r, hr, _⟩
  simp [Group.drop] at hr

/-- Dropping a user changes nobody else's capabilities. -/
theorem Group.drop_gives_other (g : Group) {u x : User} (h : x ≠ u) (c : Capability) :
    (g.drop u).gives x c ↔ g.gives x c := by
  simp [Group.gives, Group.drop, h]

/-- Removing a person from a group ends what a grant to that group gave them.
    The grant is the same value before and after. -/
theorem drop_ends_grant (s s' : OwnerState) (o : OwnerRef) (u : User) (r : Role)
    (c : Capability) (h : s'.resolve o = (s.resolve o).drop u) :
    ¬ (Grant.mk (.group o) r).gives s' u c := by
  intro hg
  rw [Grant.gives_iff] at hg
  have e : s'.subject (.group o) = (s.resolve o).drop u := h
  exact Group.drop_gives_self _ _ _ (e ▸ hg.2)

/-- The same removal leaves every other user's capabilities through the grant
    unchanged. -/
theorem drop_keeps_others (s s' : OwnerState) (o : OwnerRef) {u x : User}
    (hx : x ≠ u) (r : Role) (c : Capability)
    (h : s'.resolve o = (s.resolve o).drop u) :
    (Grant.mk (.group o) r).gives s' x c ↔ (Grant.mk (.group o) r).gives s x c := by
  rw [Grant.gives_iff, Grant.gives_iff]
  have e : s'.subject (.group o) = (s.resolve o).drop u := h
  have e' : s.subject (.group o) = s.resolve o := rfl
  show _ ∧ (s'.subject (.group o)).gives x c ↔ _ ∧ (s.subject (.group o)).gives x c
  rw [e, e', Group.drop_gives_other _ hx]

-- ============================================================
-- One rule, not two vocabularies
-- ============================================================

theorem may_read_iff_gives (r : User) (o : Owner) (k : AccessKind) :
    may_read r o k ↔ o.gives r (.read k) := Iff.rfl

theorem may_write_iff_gives (r : User) (o : Owner) (k : AccessKind) :
    may_write r o k ↔ o.gives r (.write k) := Iff.rfl

theorem may_read_goals_iff_gives (r : User) (o : Owner) :
    may_read_goals r o ↔ o.gives r .readsGoals := Iff.rfl

theorem may_work_goals_iff_gives (r : User) (o : Owner) :
    may_work_goals r o ↔ o.gives r .worksGoals := Iff.rfl

theorem may_administer_iff_gives (r : User) (o : Owner) :
    may_administer r o ↔ o.gives r .administers := Iff.rfl

/-- The personal-group exclusion in `may_manage` is implied: the only role a
    personal owner holds is `Role.personal`, which does not manage. -/
theorem may_manage_iff_gives (r : User) (o : Owner) :
    may_manage r o ↔ o.gives r .manages := by
  constructor
  · rintro ⟨_, x, hx, hm⟩
    exact ⟨x, hx, hm⟩
  · rintro ⟨x, hx, hm⟩
    refine ⟨?_, x, hx, hm⟩
    rintro ⟨u, rfl⟩
    by_cases h : r = u
    · simp [Owner.ofUser, h] at hx
      subst hx
      simp [Role.has, Role.manages, Role.personal] at hm
    · simp [Owner.ofUser, h] at hx

end Causa
