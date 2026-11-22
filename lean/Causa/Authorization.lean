import Proxiima.Authentication
import Causa.Prelude
import Causa.Owner
import Causa.Identity

/-! Causa — Authorization -/

namespace Causa

/-- Owner-scoped memory read authorization. -/
def may_read (r : User) (o : Owner) (k : AccessKind) : Prop :=
  ∃ x : Role, o r = some x ∧ x.mayRead k

/-- Owner-scoped memory write authorization. -/
def may_write (r : User) (o : Owner) (k : AccessKind) : Prop :=
  ∃ x : Role, o r = some x ∧ x.mayWrite k

/-- Owner-scoped Goal read authorization. -/
def may_read_goals (r : User) (o : Owner) : Prop :=
  ∃ x : Role, o r = some x ∧ x.readsGoals

/-- Authorization to administer an owner. -/
def may_administer (r : User) (o : Owner) : Prop :=
  ∃ x : Role, o r = some x ∧ x.administers

/-- Authorization to change a group's membership and roles. -/
def may_manage (r : User) (o : Owner) : Prop :=
  ¬ Owner.isPersonal o ∧ ∃ x : Role, o r = some x ∧ x.manages

/-- Owner-scoped writes imply reads at the same memory kind. -/
theorem may_write_implies_read :
    ∀ (r : User) (o : Owner) (k : AccessKind),
      may_write r o k → may_read r o k := by
  rintro r o k ⟨x, hx, hw⟩
  exact ⟨x, hx, Nat.le_trans hw x.write_le_read⟩

/-- Roles available to verified forwarders. -/
abbrev ForwardingRole := Proxiima.Authentication.ForwardingRole

/-- The upper bound on a forwarded role. -/
def Role.forwardingCap : Role := .ofPreset Proxiima.Authentication.forwardingCap

def ForwardingRole.preset : ForwardingRole → Role
  | .viewer => Role.viewer
  | .ingest => Role.ingest
  | .editor => Role.editor

/-- The effective role of a verified forwarder. -/
def ForwardingRole.toRole (f : ForwardingRole) : Role :=
  f.preset.meet Role.forwardingCap

/-- Trusted policy and registry inputs for request forwarding. -/
structure ForwardingPolicy where
  roleFor : User → Option ForwardingRole
  registered : OwnerRef → Prop

/-- The authenticated subject's verified owner claim. -/
structure VerifiedForwardedOwner where
  subject : User
  owner : OwnerRef

/-- The source of a request's authority. -/
inductive RequestAuthority where
  | membership
  | forwarded (claim : VerifiedForwardedOwner)

/-- Role resolution for a request at one owner. -/
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

/-- A forwarded role is bounded by `Causa.Role.forwardingCap`. -/
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
  simp [Role.manages, Role.forwardingCap, Role.ofPreset, Proxiima.Authentication.forwardingCap] at this

theorem forwarding_role_no_administration (s : OwnerState) (p : ForwardingPolicy)
    (r : User) (c : VerifiedForwardedOwner) (o : OwnerRef) (role : Role)
    (h : request_has_role s p r (.forwarded c) o role) : ¬ role.administers := by
  obtain ⟨x, rfl⟩ := forwarding_role_capped s p r c o role h
  intro ha
  have := (Role.meet_capped x Role.forwardingCap).2.1 ha
  simp [Role.administers, Role.forwardingCap, Role.ofPreset, Proxiima.Authentication.forwardingCap] at this

theorem forwarding_role_no_goal_work (s : OwnerState) (p : ForwardingPolicy)
    (r : User) (c : VerifiedForwardedOwner) (o : OwnerRef) (role : Role)
    (h : request_has_role s p r (.forwarded c) o role) : ¬ role.worksGoals := by
  obtain ⟨x, rfl⟩ := forwarding_role_capped s p r c o role h
  intro hw
  have := (Role.meet_capped x Role.forwardingCap).1 hw
  simp [Role.worksGoals, Role.forwardingCap, Role.ofPreset, Proxiima.Authentication.forwardingCap] at this

/-- Owner-scoped Goal work authorization. -/
def request_may_work_goals (s : OwnerState) (p : ForwardingPolicy) (r : User)
    (a : RequestAuthority) (o : OwnerRef) : Prop :=
  ∃ role : Role, request_has_role s p r a o role ∧ role.worksGoals

/-- Authorization for Fact effects declared by a host-state Goal work command. -/
def request_may_write_goal_work (s : OwnerState) (p : ForwardingPolicy) (r : User)
    (a : RequestAuthority) (o : OwnerRef) (declared : Bool) (k : AccessKind) : Prop :=
  k = .Fact ∧ declared = true ∧ request_may_work_goals s p r a o

end Causa
