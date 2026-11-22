import Causa.Owner
import Causa.Authorization

/-! Causa — Audience -/

namespace Causa

/-- Capabilities a role can convey. -/
inductive Capability where
  | read (k : AccessKind)
  | write (k : AccessKind)
  | readsGoals
  | worksGoals
  | administers
  | manages

/-- A role's capability predicate. -/
def Role.has (r : Role) : Capability → Prop
  | .read k => r.mayRead k
  | .write k => r.mayWrite k
  | .readsGoals => r.readsGoals
  | .worksGoals => r.worksGoals
  | .administers => r.administers
  | .manages => r.manages

/-- Capability intersection for a role cap. -/
theorem Role.has_meet (x y : Role) (c : Capability) :
    (x.meet y).has c ↔ x.has c ∧ y.has c := by
  cases c with
  | read k => simp only [Role.has, Role.mayRead, Role.meet]; omega
  | write k => simp only [Role.has, Role.mayWrite, Role.meet]; omega
  | readsGoals =>
    simp only [Role.has, Role.readsGoals, Role.meet]
    cases x.goals <;> cases y.goals <;> simp [GoalAccess.meet, Proxiima.OwnerAuthority.GoalAccess.rank]
  | worksGoals =>
    simp only [Role.has, Role.worksGoals, Role.meet]
    cases x.goals <;> cases y.goals <;> simp [GoalAccess.meet, Proxiima.OwnerAuthority.GoalAccess.rank]
  | administers => simp [Role.has, Role.administers, Role.meet]
  | manages => simp [Role.has, Role.manages, Role.meet]

/-- Capability union across roles. -/
theorem Role.has_join (x y : Role) (c : Capability) :
    (x.join y).has c ↔ x.has c ∨ y.has c := by
  cases c with
  | read k => simp only [Role.has, Role.mayRead, Role.join]; omega
  | write k => simp only [Role.has, Role.mayWrite, Role.join]; omega
  | readsGoals =>
    simp only [Role.has, Role.readsGoals, Role.join]
    cases x.goals <;> cases y.goals <;> simp [GoalAccess.join, Proxiima.OwnerAuthority.GoalAccess.rank]
  | worksGoals =>
    simp only [Role.has, Role.worksGoals, Role.join]
    cases x.goals <;> cases y.goals <;> simp [GoalAccess.join, Proxiima.OwnerAuthority.GoalAccess.rank]
  | administers => simp [Role.has, Role.administers, Role.join]
  | manages => simp [Role.has, Role.manages, Role.join]

/-- Read access follows from write access at the same memory kind. -/
theorem Role.has_write_read (r : Role) (k : AccessKind) :
    r.has (.write k) → r.has (.read k) := fun h =>
  Nat.le_trans h r.write_le_read

/-- The capabilities of the unrestricted group role. -/
theorem Role.admin_has (c : Capability) : Role.admin.has c := by
  cases c with
  | read k => change (Proxiima.OwnerAuthority.coveringCeiling k).rank ≤ 3; cases k <;> decide
  | write k => change (Proxiima.OwnerAuthority.coveringCeiling k).rank ≤ 3; cases k <;> decide
  | readsGoals => change Proxiima.OwnerAuthority.GoalAccess.works ≠ .none; decide
  | worksGoals => rfl
  | administers => rfl
  | manages => rfl

/-- A capability inherited through group membership. -/
def Group.gives (g : Group) (u : User) (c : Capability) : Prop :=
  ∃ r, g u = some r ∧ r.has c

/-- Lifting capability implication from roles to groups. -/
theorem Group.gives_mono {g : Group} {u : User} {c d : Capability}
    (hcd : ∀ r : Role, r.has c → r.has d) (h : g.gives u c) : g.gives u d := by
  obtain ⟨r, hr, hc⟩ := h
  exact ⟨r, hr, hcd r hc⟩

/-- Capability resolution under a subgroup cap. -/
theorem Group.gives_mount (g : Group) (cap : Role) (u : User) (c : Capability) :
    (g.mount cap).gives u c ↔ g.gives u c ∧ cap.has c := by
  unfold Group.gives Group.mount
  cases g u with
  | none => simp
  | some x => simp [Role.has_meet]

/-- Capability resolution across group memberships. -/
theorem Group.gives_union (a b : Group) (u : User) (c : Capability) :
    (a.union b).gives u c ↔ a.gives u c ∨ b.gives u c := by
  unfold Group.gives Group.union
  cases a u <;> cases b u <;> simp [Role.has_join]

/-- The synthetic group used to resolve an individual grant subject. -/
def Group.only (u : User) : Group :=
  fun x => if x = u then some Role.admin else none

/-- The recipient of an audience grant. -/
inductive Subject where
  | person (u : User)
  | group (o : OwnerRef)

/-- The current membership of a grant subject. -/
def OwnerState.subject (s : OwnerState) : Subject → Group
  | .person u => Group.only u
  | .group o => s.resolve o

/-- A role assigned to an audience subject. -/
structure Grant where
  subject : Subject
  role : Role

/-- The capability conveyed by a grant under current membership. -/
def Grant.gives (g : Grant) (s : OwnerState) (u : User) (c : Capability) : Prop :=
  ((s.subject g.subject).mount g.role).gives u c

/-- The two conditions for a grant to convey a capability. -/
theorem Grant.gives_iff (g : Grant) (s : OwnerState) (u : User) (c : Capability) :
    g.gives s u c ↔ g.role.has c ∧ (s.subject g.subject).gives u c := by
  unfold Grant.gives
  rw [Group.gives_mount]
  exact and_comm

/-- The resolved group used for audience lookup. -/
def flattened (s : OwnerState) (o : Owner) (gs : List Grant) : Group :=
  gs.foldl (fun acc g => acc.union ((s.subject g.subject).mount g.role)) o

/-- Equivalence of flattened lookup and per-grant authorization. -/
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
      · exact Or.inr ⟨g, List.mem_cons_self, hg⟩
      · exact Or.inr ⟨g', List.mem_cons_of_mem _ hg', hs'⟩
    · rintro (ho | ⟨g', hg', hs'⟩)
      · exact Or.inl (Or.inl ho)
      · cases List.mem_cons.1 hg' with
        | inl e => subst e; exact Or.inl (Or.inr hs')
        | inr m => exact Or.inr ⟨g', m, hs'⟩

/-- Composition of nested subgroup caps. -/
theorem Group.mount_mount (g : Group) (a b : Role) :
    (g.mount a).mount b = g.mount (a.meet b) := by
  funext u
  simp only [Group.mount]
  cases g u <;> simp [Role.meet_assoc]

/-- Distribution of a subgroup cap over a group union. -/
theorem Group.mount_union (a b : Group) (cap : Role) :
    (a.union b).mount cap = (a.mount cap).union (b.mount cap) := by
  funext u
  simp only [Group.union, Group.mount]
  cases a u <;> cases b u <;>
    simp [Role.meet_comm, Role.meet_join_distrib]

/-- Capability resolution for a grant addressed directly to a person. -/
theorem grant_to_person (s : OwnerState) (u requester : User) (r : Role)
    (c : Capability) :
    (Grant.mk (.person u) r).gives s requester c ↔ requester = u ∧ r.has c := by
  unfold Grant.gives OwnerState.subject
  rw [Group.gives_mount]
  unfold Group.gives Group.only
  by_cases h : requester = u
  · simp [h, Role.admin_has]
  · simp [h]

/-- Revocation of the removed member's group capabilities. -/
theorem Group.drop_gives_self (g : Group) (u : User) (c : Capability) :
    ¬ (g.drop u).gives u c := by
  rintro ⟨r, hr, _⟩
  simp [Group.drop] at hr

/-- Preservation of other members' capabilities on removal. -/
theorem Group.drop_gives_other (g : Group) {u x : User} (h : x ≠ u) (c : Capability) :
    (g.drop u).gives x c ↔ g.gives x c := by
  simp [Group.gives, Group.drop, h]

/-- Revocation through a group grant after membership removal. -/
theorem drop_ends_grant (s s' : OwnerState) (o : OwnerRef) (u : User) (r : Role)
    (c : Capability) (h : s'.resolve o = (s.resolve o).drop u) :
    ¬ (Grant.mk (.group o) r).gives s' u c := by
  intro hg
  rw [Grant.gives_iff] at hg
  have e : s'.subject (.group o) = (s.resolve o).drop u := h
  exact Group.drop_gives_self _ _ _ (e ▸ hg.2)

/-- Preservation of other recipients' grant capabilities after membership removal. -/
theorem drop_keeps_others (s s' : OwnerState) (o : OwnerRef) {u x : User}
    (hx : x ≠ u) (r : Role) (c : Capability)
    (h : s'.resolve o = (s.resolve o).drop u) :
    (Grant.mk (.group o) r).gives s' x c ↔ (Grant.mk (.group o) r).gives s x c := by
  rw [Grant.gives_iff, Grant.gives_iff]
  have e : s'.subject (.group o) = (s.resolve o).drop u := h
  have e' : s.subject (.group o) = s.resolve o := rfl
  show _ ∧ (s'.subject (.group o)).gives x c ↔ _ ∧ (s.subject (.group o)).gives x c
  rw [e, e', Group.drop_gives_other _ hx]

/-- Group management expressed as a resolved capability. -/
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
      simp [Role.has, Role.manages, Role.personal, Role.ofPreset, Proxiima.OwnerAuthority.rolePresets] at hm
    · simp [Owner.ofUser, h] at hx

end Causa
