/-
Causa — Owner

The scoping primitive (doc 01 §Owner). Every Memory and Goal carries an Owner:
its access scope.

Ontology (realign 2026-06-28): **a resolved Owner is always a Group**, and a
Group maps each member to a `Role`. Stable persisted owner references are modeled
separately in `Causa.Identity` (`OwnerRef`); this file models the resolved
Leopard-style result. The one irreducible atom is `User` (a role-bearing
identity atom: human user, configured Agent, service actor); groups, resolved
owners, and roles are structural over it.

A `Role` is two independent capability ceilings over the access ladder
F < A < P < G — how high a member may READ and how high they may WRITE (read and
write are separate, so an `Ingest` owner role never widens into a general editor)
— plus a `manage` flag for META-MANAGEMENT of the group itself (membership and
role assignment). A "user" is the special case of a Group: the personal group,
automatically present for every user, in which that user holds the maximal
`personal` role and no one else is a member. Personal groups are auto-derived
and forbid meta-management (`personal_forbids_manage`).

**Org is not a kernel concept** (decision `2026-06-11-org-out-of-kernel.md`):
billing/quota attribution is engine metadata, never a kernel face; org-wide
visibility is a default `<org>-everyone` group.

The resolved Owner is the SINGLE owning Group used by authorization checks. Row
storage should point at a stable `OwnerRef`; the host resolves that reference to
this group-shaped `Owner` before the kernel rule runs. There is no separate
`is_home`/`reaches`/`entity_owner` reachability layer (removed with the share
set, D11): read-only sharing is a viewer-role membership in that one group, and
sharing beyond it is an owner-to-owner TRANSFER of the entity into another
group. There is no universal-read owner: an entity is exactly as readable as
its owning group. One entity, one owner group.
-/

import Causa.Prelude

namespace Causa

-- ============================================================
-- The one irreducible atom
-- ============================================================

/-- The identity atom: an engine-minted token (UUIDv7 in the engine).
    EQUALITY is all the kernel asks of it. This replaces the former opaque
    identity atom: the representation stays sealed (private constructor and
    field — no projection or construction outside this module), but equality
    is now computable and the mint `User.ofToken` is visible to proofs.
    Concrete user records (who exists, attributes, catalogs) remain inputs
    from the world — spiice/engine sidecar data, never kernel fields. -/
structure User where
  private mk ::
  private token : String
  deriving DecidableEq, Repr

/-- The only public way to obtain a `User`: mint from an engine token. -/
def User.ofToken (t : String) : User := ⟨t⟩

/-- Distinct tokens mint distinct users — identity IS the token. -/
theorem User.ofToken_inj {s t : String}
    (h : User.ofToken s = User.ofToken t) : s = t := by
  cases h
  rfl

/-- Kernel witness inhabitant — a proof artifact for closed non-vacuity
    witnesses, not a real identity. Real users are engine-minted. -/
instance : Inhabited User := ⟨User.ofToken "causa-kernel-witness"⟩

-- ============================================================
-- The access ladder and roles
-- ============================================================

/-- The three memory kinds, ranked for ACCESS. This is the access axis (who
    may touch what); it is distinct from the edge-directionality layer ℓ in
    `Causa.Edges`. A Goal is intent, not memory, and is not on this ladder:
    what a role may do with Goals is `Role.goals`. -/
inductive AccessKind where
  | fact | abstraction | perspective
  deriving DecidableEq, Repr

def AccessKind.rank : AccessKind → Nat
  | .fact => 0 | .abstraction => 1 | .perspective => 2

/-- What a role may do with the owner's Goals: nothing, read them, or work
    them (set, modify, transition). Its own chain, beside the memory ladder. -/
inductive GoalAccess where
  | none | reads | works
  deriving DecidableEq, Repr

def GoalAccess.rank : GoalAccess → Nat
  | .none => 0 | .reads => 1 | .works => 2

/-- The weaker of two. -/
def GoalAccess.meet (x y : GoalAccess) : GoalAccess :=
  if x.rank ≤ y.rank then x else y

/-- The stronger of two. -/
def GoalAccess.join (x y : GoalAccess) : GoalAccess :=
  if x.rank ≤ y.rank then y else x

/-- A role = two ceilings over the memory ladder and three capabilities.
    `read` / `write` are layer counts (0 = none, 1 = F, 2 = F+A, 3 = F+A+P);
    capability over a kind `k` is `k.rank < ceiling`. `goals` is what the role
    may do with the owner's Goals. `administer` is authority over the owner
    itself: transfer, erase, and achieving, decomposing and sharing its Goals.
    `manage` is the authority to write the GROUP's own role map (add/remove
    members, assign roles). No capability follows from a ceiling's position.
    `write_le_read` makes "write ⊆ read" a structural field, not an axiom — a
    member can always read at least what it may write. -/
structure Role where
  read  : Nat
  write : Nat
  goals : GoalAccess
  administer : Bool
  manage : Bool
  write_le_read : write ≤ read

def Role.mayRead  (x : Role) (k : AccessKind) : Prop := k.rank < x.read
def Role.mayWrite (x : Role) (k : AccessKind) : Prop := k.rank < x.write
/-- Reads the owner's Goals. -/
def Role.readsGoals (x : Role) : Prop := x.goals ≠ .none
/-- Works the owner's Goals: sets, modifies and transitions them. -/
def Role.worksGoals (x : Role) : Prop := x.goals = .works
/-- Administers the owner: transfer, erase, and achieving, decomposing and
    sharing its Goals. -/
def Role.administers (x : Role) : Prop := x.administer = true
/-- Meta-management authority: may add/remove members and assign roles in the
    group. A group-level rule still gates the use of it — personal groups
    forbid management entirely (see `personal_forbids_manage`). -/
def Role.manages (x : Role) : Prop := x.manage = true

/-- Working Goals includes reading them. -/
theorem Role.worksGoals_readsGoals (x : Role) (h : x.worksGoals) : x.readsGoals := by
  simp only [Role.worksGoals] at h
  simp [Role.readsGoals, h]

/-- Maximal authority over one's own scope, but NOT a managing role: the
    personal group is auto-derived and unmanageable, so `manage := false`. -/
def Role.personal : Role := ⟨3, 3, .works, true, false, by omega⟩
/-- Read every kind, write none, work Goals, no administration. -/
def Role.viewer   : Role := ⟨3, 0, .works, false, false, by omega⟩
/-- Source ingest: Facts in and out only, no Goals. -/
def Role.ingest   : Role := ⟨1, 1, .none, false, false, by omega⟩
/-- Author every memory kind and work Goals, no administration. -/
def Role.editor   : Role := ⟨3, 3, .works, false, false, by omega⟩
/-- Group admin: everything, including administration and management. -/
def Role.admin    : Role := ⟨3, 3, .works, true, true, by omega⟩

-- ============================================================
-- Role lattice — combine roles across membership paths
-- ============================================================

/-- Meet (cap): the WEAKER of two roles per capability. Used to cap a mounted
    sub-group's inherited authority. `write_le_read` is preserved (min is
    monotone). -/
def Role.meet (x y : Role) : Role :=
  ⟨min x.read y.read, min x.write y.write, x.goals.meet y.goals,
    x.administer && y.administer, x.manage && y.manage, by
    have hx := x.write_le_read; have hy := y.write_le_read; omega⟩

/-- Join: the STRONGER of two roles per capability. Used when a user is a member
    via two paths — the higher capability wins. `write_le_read` is preserved
    (max is monotone). -/
def Role.join (x y : Role) : Role :=
  ⟨max x.read y.read, max x.write y.write, x.goals.join y.goals,
    x.administer || y.administer, x.manage || y.manage, by
    have hx := x.write_le_read; have hy := y.write_le_read; omega⟩

/-- A cap that lacks a capability removes it: the meet works Goals,
    administers or manages only where the cap does. -/
theorem Role.meet_capped (x cap : Role) :
    ((x.meet cap).worksGoals → cap.worksGoals) ∧
    ((x.meet cap).administers → cap.administers) ∧
    ((x.meet cap).manages → cap.manages) := by
  refine ⟨?_, ?_, ?_⟩
  · simp only [Role.worksGoals, Role.meet, GoalAccess.meet]
    generalize x.goals = a
    generalize cap.goals = c
    cases a <;> cases c <;> simp [GoalAccess.rank]
  · simp only [Role.administers, Role.meet, Bool.and_eq_true]
    exact fun h => h.2
  · simp only [Role.manages, Role.meet, Bool.and_eq_true]
    exact fun h => h.2

-- ============================================================
-- Group and Owner
-- ============================================================

/-- A Group maps each member to their `Role`; `none` = not a member. This is the
    resolved authorization view, not the stable persisted owner-table handle.
    `abbrev` keeps it reducible. -/
abbrev Group : Type := User → Option Role

/-- A resolved Owner is always a Group (doc 01 §Owner). Entity rows eventually
    carry stable `OwnerRef`s; authorization uses the host-resolved `Owner`. -/
abbrev Owner : Type := Group

/-- A user/agent as an Owner: the personal group, in which only that identity
    is a member and holds the maximal `personal` role. -/
def Owner.ofUser (u : User) : Owner :=
  fun x => if x = u then some Role.personal else none

/-- A personal owner contains its own user at the personal role. -/
theorem Owner.ofUser_self (u : User) :
    Owner.ofUser u u = some Role.personal := by
  simp [Owner.ofUser]

/-- A personal owner contains no other user. -/
theorem Owner.ofUser_other {u x : User} (h : x ≠ u) :
    Owner.ofUser u x = none := by
  simp [Owner.ofUser, h]

/-- An Owner is personal iff it is some user's personal group. -/
def Owner.isPersonal (o : Owner) : Prop := ∃ u : User, o = Owner.ofUser u

-- ============================================================
-- Membership
-- ============================================================

/-- ES-2 — a requester is a member of (visible to) an Owner iff it holds any
    role there. Read/write authority is then the role's ceiling over the kind
    (see `Causa.Authorization`). For a personal owner, membership reduces to
    identity (`visible_personal`): "only the user can join it". A definition,
    not an axiom. ES-3: org does not exist at this layer. -/
def visible (o : Owner) (requester : User) : Prop := o requester ≠ none

/-- Computable membership test — the Bool face of `visible`. -/
def Group.member? (g : Group) (u : User) : Bool := (g u).isSome

instance (o : Owner) (u : User) : Decidable (visible o u) :=
  match h : o u with
  | some _ => .isTrue (by simp [visible, h])
  | none   => .isFalse (by simp [visible, h])

/-- The personal-owner reduction: membership in a user's own group is exactly
    being that user. -/
theorem visible_personal (u requester : User) :
    visible (Owner.ofUser u) requester ↔ requester = u := by
  simp only [visible, Owner.ofUser]
  split <;> simp_all

-- ============================================================
-- Group composition (nesting) — Level 2
-- ============================================================

/- Nesting needs NO new kernel primitive: a nested group RESOLVES to an ordinary
   `Owner` (`User → Option Role`), and the gates are total over it. Storage
   materializes `Group.mount` and `Group.union` for a tree of groups: each group
   has at most one parent, roles climb through meet-capped mounts, and multiple
   person relations or paths join. The kernel sees only the resolved `Owner`,
   never the storage tree. Group-as-member is not a kernel notion, and a
   recursive `Group → Option Role` field would be non-strictly-positive. -/

/-- The empty group: no user has a role. -/
def Group.empty : Group := fun _ => none

/-- The universal group at a fixed role: every user has that role. -/
def Group.everyone (r : Role) : Group := fun _ => some r

/-- The universal group at any role is distinct from the empty group. -/
theorem Group.everyone_ne_empty (r : Role) : Group.everyone r ≠ Group.empty := by
  intro h
  have hval := congrFun h default
  simp [Group.everyone, Group.empty] at hval

/-- Mount sub-group `g` into a parent at role `cap`: every member of `g` joins at
    their g-role capped by `cap` (lattice meet). Produces an ordinary `Owner`;
    the no-escalation guarantee is `mount_cannot_escalate` (Authorization). -/
def Group.mount (g : Group) (cap : Role) : Group :=
  fun u => (g u).map (fun x => Role.meet x cap)

/-- Union of two groups: a user's role is the join of their roles in each (the
    higher capability when a member of both). -/
def Group.union (a b : Group) : Group :=
  fun u =>
    match a u, b u with
    | some x, some y => some (Role.join x y)
    | some x, none   => some x
    | none,   some y => some y
    | none,   none   => none

/-- Drop a user from a group — the membership change behind sharing-removal and
    compliance erasure: the user loses their role, everyone else is unchanged.
    This is the ONLY mutable face of ownership; the entity row that names this
    group never changes (cognitive content stays append-only). When the last
    member is dropped the group is abandoned (`Causa.Compliance.abandoned`). -/
def Group.drop (g : Group) (u : User) : Group :=
  fun x => if x = u then none else g x

/-- Dropping a user removes that user's own role. -/
theorem Group.drop_self (g : Group) (u : User) :
    Group.drop g u u = none := by
  simp [Group.drop]

/-- Dropping one user leaves all other users unchanged. -/
theorem Group.drop_other {g : Group} {u x : User} (h : x ≠ u) :
    Group.drop g u x = g x := by
  simp [Group.drop, h]

end Causa
