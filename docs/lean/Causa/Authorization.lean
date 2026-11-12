/-
Causa — Authorization (group-ownership realign; role-graded access)

Access leaves the entity row entirely (spec §10). A Group maps each member to a
`Role` (Causa.Owner): two ceilings over the memory ladder F<A<P — how high one
may READ and how high one may WRITE — and three capabilities: what the role
may do with the owner's Goals, whether it administers the owner, whether it
manages the group. Read and write are independent, so an `Ingest` owner role
(write Facts only) never widens into a general editor, and no capability
follows from a ceiling.

Each entity has a SINGLE owning Group for the owner-scoped lane. There is no
`is_home`/`reaches`/`entity_owner` share layer in that lane: a read-only owner
share is a viewer-role membership in the owning group, and owner-to-owner
sharing is a TRANSFER. The host resolves the DATA — who populates each group
at what role — ONCE; the kernel fixes only the RULE (read/write = the role's
ceiling over the kind). There is no universal-read owner in this lane.

Goal collaboration is a separate, narrow exact-version route. A Goal
publication or work grant does not become an Owner role, does not add an owner
to a requester's OwnerScope, and does not replace owner transfer, membership,
erase, or forwarding authority. The predicates for that route live in
`Causa.EdgeAuthorization`; the owner laws below remain literal owner-scope
laws. The retired owner-space `AccessGrant` / `MemoryAction` layer is gone.
-/

import Causa.Prelude
import Causa.Owner
import Causa.Identity

namespace Causa

-- ============================================================
-- The gates (per kind) — spec §3 / §10 invariants 1–2
-- ============================================================

/- An entity has exactly ONE owner — a Group (Causa.Owner). Access is entirely
   the requester's role in that group: a read-only share is a viewer-role
   membership, write is an editor-or-higher role. There is NO share set above
   the owner, because the Group already IS the sharing mechanism — to share an
   entity you move it to (or create) a group holding the desired members at the
   desired roles. One owner per entity ⇒ the "single write owner" invariant is
   structural, and per-entity sharing is per-entity group choice. -/

/-- AUTH-READ (invariant #2) — a request may READ an entity of kind `k` iff the
    requester holds a role in the entity's owning group whose read ceiling
    covers `k`. -/
def may_read (r : User) (o : Owner) (k : AccessKind) : Prop :=
  ∃ x : Role, o r = some x ∧ x.mayRead k

/-- AUTH-WRITE (invariant #1, "single write owner") — a request may WRITE an
    entity of kind `k` iff the requester holds a role in its owning group whose
    write ceiling covers `k`. One owner group, so there is one write scope. -/
def may_write (r : User) (o : Owner) (k : AccessKind) : Prop :=
  ∃ x : Role, o r = some x ∧ x.mayWrite k

/-- A request may READ the owner's Goals iff its role there reads Goals. -/
def may_read_goals (r : User) (o : Owner) : Prop :=
  ∃ x : Role, o r = some x ∧ x.readsGoals

/-- A request may WORK the owner's Goals — set, modify and transition them —
    iff its role there works Goals. The write ceiling bounds the owner's
    memory, not its work. -/
def may_work_goals (r : User) (o : Owner) : Prop :=
  ∃ x : Role, o r = some x ∧ x.worksGoals

/-- A request may ADMINISTER the owner — transfer, erase, and achieving,
    decomposing and sharing its Goals — iff its role there administers. -/
def may_administer (r : User) (o : Owner) : Prop :=
  ∃ x : Role, o r = some x ∧ x.administers

/-- Server-resolved read gate: rows store `OwnerRef`; the trusted host resolves
    it through `OwnerState` before the pure kernel rule runs. -/
def may_read_in (s : OwnerState) (r : User) (o : OwnerRef) (k : AccessKind) : Prop :=
  may_read r (s.resolve o) k

/-- Server-resolved write gate: callers never supply the resolved Owner map. -/
def may_write_in (s : OwnerState) (r : User) (o : OwnerRef) (k : AccessKind) : Prop :=
  may_write r (s.resolve o) k

/-- AUTH-MANAGE — a request may META-MANAGE a group `o` (write its membership /
    role map) iff `o` is not a personal group AND the requester holds a role
    there with management authority. The personal-group exclusion is a group
    property, not merely a role one: personal groups are auto-derived and
    immutable. -/
def may_manage (r : User) (o : Owner) : Prop :=
  ¬ Owner.isPersonal o ∧ ∃ x : Role, o r = some x ∧ x.manages

/-- Server-resolved management gate over stable owner references. -/
def may_manage_in (s : OwnerState) (r : User) (o : OwnerRef) : Prop :=
  may_manage r (s.resolve o)

/-- AUTH-1 — write implies read: whoever may write an entity may read it.
    THEOREM — same owning group, and `Role.write_le_read` (read ceiling ≥ write
    ceiling). -/
theorem may_write_implies_read :
    ∀ (r : User) (o : Owner) (k : AccessKind),
      may_write r o k → may_read r o k := by
  rintro r o k ⟨x, hx, hw⟩
  exact ⟨x, hx, Nat.lt_of_lt_of_le hw x.write_le_read⟩

-- ============================================================
-- Verified request forwarding, separate from stored membership
-- ============================================================

/-- The closed forwarding ceilings. -/
inductive ForwardingRole where
  | viewer | ingest | editor

/-- What no forwarded request exceeds: it reads Goals at most, and neither
    administers nor manages. -/
def Role.forwardingCap : Role := ⟨3, 3, .reads, false, false, by omega⟩

def ForwardingRole.preset : ForwardingRole → Role
  | .viewer => Role.viewer
  | .ingest => Role.ingest
  | .editor => Role.editor

/-- A forwarding role is its preset under the forwarding cap. -/
def ForwardingRole.toRole (f : ForwardingRole) : Role :=
  f.preset.meet Role.forwardingCap

/-- The trusted policy and registry inputs at a request boundary. No policy
    role is a stored membership. -/
structure ForwardingPolicy where
  roleFor : User → Option ForwardingRole
  registered : OwnerRef → Prop

/-- A claim produced by credential verification. Cryptographic verification is
    a boundary input, not an axiom of this authorization model. -/
structure VerifiedForwardedOwner where
  subject : User
  owner : OwnerRef

/-- Each request uses ordinary membership or its verified forwarding claim.
    These alternatives never union their roles. -/
inductive RequestAuthority where
  | membership
  | forwarded (claim : VerifiedForwardedOwner)

/-- The requester stays the authenticated subject. A forwarded role exists
    only at the exact claimed, registered owner for a configured forwarder. -/
def request_has_role (s : OwnerState) (p : ForwardingPolicy)
    (requester : User) (authority : RequestAuthority) (owner : OwnerRef) (role : Role) : Prop :=
  match authority with
  | .membership => s.resolve owner requester = some role
  | .forwarded claim =>
      claim.subject = requester ∧ claim.owner = owner ∧ p.registered owner ∧
      (p.roleFor requester).map ForwardingRole.toRole = some role

def request_may_read (s : OwnerState) (p : ForwardingPolicy) (r : User)
    (a : RequestAuthority) (o : OwnerRef) (k : AccessKind) : Prop :=
  ∃ role : Role, request_has_role s p r a o role ∧ role.mayRead k

def request_may_write (s : OwnerState) (p : ForwardingPolicy) (r : User)
    (a : RequestAuthority) (o : OwnerRef) (k : AccessKind) : Prop :=
  ∃ role : Role, request_has_role s p r a o role ∧ role.mayWrite k

/-- Management always uses ordinary membership, never forwarding policy. -/
def request_may_manage (s : OwnerState) (r : User)
    (a : RequestAuthority) (o : OwnerRef) : Prop :=
  match a with
  | .membership => may_manage_in s r o
  | .forwarded _ => False

/-- Durable delegation requires ordinary membership authority. -/
def request_may_delegate (a : RequestAuthority) : Prop :=
  match a with
  | .membership => True
  | .forwarded _ => False

/-- A forwarded role is some role under the forwarding cap. -/
theorem forwarding_role_capped (s : OwnerState) (p : ForwardingPolicy)
    (r : User) (c : VerifiedForwardedOwner) (o : OwnerRef) (role : Role)
    (h : request_has_role s p r (.forwarded c) o role) :
    ∃ x : Role, role = x.meet Role.forwardingCap := by
  have hrole := h.2.2.2
  cases hp : p.roleFor r with
  | none => simp [hp] at hrole
  | some policyRole =>
      rw [hp] at hrole
      exact ⟨policyRole.preset, (Option.some.inj hrole).symm⟩

theorem forwarding_role_no_management (s : OwnerState) (p : ForwardingPolicy)
    (r : User) (c : VerifiedForwardedOwner) (o : OwnerRef) (role : Role)
    (h : request_has_role s p r (.forwarded c) o role) : ¬ role.manages := by
  obtain ⟨x, rfl⟩ := forwarding_role_capped s p r c o role h
  intro hm
  have := (Role.meet_capped x Role.forwardingCap).2.2 hm
  simp [Role.manages, Role.forwardingCap] at this

theorem forwarding_role_no_administration (s : OwnerState) (p : ForwardingPolicy)
    (r : User) (c : VerifiedForwardedOwner) (o : OwnerRef) (role : Role)
    (h : request_has_role s p r (.forwarded c) o role) : ¬ role.administers := by
  obtain ⟨x, rfl⟩ := forwarding_role_capped s p r c o role h
  intro ha
  have := (Role.meet_capped x Role.forwardingCap).2.1 ha
  simp [Role.administers, Role.forwardingCap] at this

theorem forwarding_role_no_goal_work (s : OwnerState) (p : ForwardingPolicy)
    (r : User) (c : VerifiedForwardedOwner) (o : OwnerRef) (role : Role)
    (h : request_has_role s p r (.forwarded c) o role) : ¬ role.worksGoals := by
  obtain ⟨x, rfl⟩ := forwarding_role_capped s p r c o role h
  intro hw
  have := (Role.meet_capped x Role.forwardingCap).1 hw
  simp [Role.worksGoals, Role.forwardingCap] at this

/-- Reading the owner's Goals. -/
def request_may_read_goals (s : OwnerState) (p : ForwardingPolicy) (r : User)
    (a : RequestAuthority) (o : OwnerRef) : Prop :=
  ∃ role : Role, request_has_role s p r a o role ∧ role.readsGoals

/-- Goal work — setting, modifying and transitioning a Goal of the owner.
    It is the role's own capability: the write ceiling bounds the owner's
    memory, not its work. -/
def request_may_work_goals (s : OwnerState) (p : ForwardingPolicy) (r : User)
    (a : RequestAuthority) (o : OwnerRef) : Prop :=
  ∃ role : Role, request_has_role s p r a o role ∧ role.worksGoals

/-- Administering the owner: transfer, erase, and achieving, decomposing and
    sharing its Goals. -/
def request_may_administer (s : OwnerState) (p : ForwardingPolicy) (r : User)
    (a : RequestAuthority) (o : OwnerRef) : Prop :=
  ∃ role : Role, request_has_role s p r a o role ∧ role.administers

/-- A Goal work unit — one host-state command declared Goal work on a Goal
    of the owner — writes Facts on that owner only where the command declares
    them, under the Goal work rule. It writes no other kind, and it is no
    memory write permission: `request_may_write` is unchanged. -/
def request_may_write_goal_work (s : OwnerState) (p : ForwardingPolicy) (r : User)
    (a : RequestAuthority) (o : OwnerRef) (declared : Bool) (k : AccessKind) : Prop :=
  k = .fact ∧ declared = true ∧ request_may_work_goals s p r a o

end Causa
