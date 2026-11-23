import Causa.Edges
import Causa.Authorization
import Causa.Audience
import Proxiima.GoalCollaboration

/-! Causa — Goal collaboration authority -/

namespace Causa

open Proxiima.GoalCollaboration
  (PublicationUse boundWorkStateAdmitted grantSubjectAdmitted)

structure GoalControllerEpoch where
  goal_handle : Handle
  owner : Owner
  generation : Nat

structure GoalPublication where
  goal : GoalControllerEpoch
  memory_t : MemoryId
  source_owner : Owner
  source_generation : Nat
  controller_generation : Nat
  permitted_use : PublicationUse

/-- An audience grant scoped to a Goal controller epoch. -/
structure GoalAudienceGrant extends Grant where
  goal : GoalControllerEpoch
  controller_generation : Nat
  expires_at : Option Nat
  no_owner_administration : role.administer = false
  subject_admitted : grantSubjectAdmitted
    (match subject with | .person _ => .person | .group _ => .group) role.manage = true

/-- The epoch and expiry premises of `Proxiima.GoalCollaboration.grantInForce`. -/
def GoalAudienceGrant.inForce (g : GoalAudienceGrant) (goal : GoalControllerEpoch)
    (current_controller_generation now : Nat) : Prop :=
  g.goal = goal ∧ g.controller_generation = current_controller_generation ∧
    ∀ expiration ∈ g.expires_at, now < expiration

/-- Capability resolution through the owner or a Goal's audience. -/
def goal_route (s : OwnerState) (requester : User) (owner : Owner)
    (goal : GoalControllerEpoch) (generation now : Nat)
    (grants : List GoalAudienceGrant) (c : Capability) : Prop :=
  owner.gives requester c ∨
    ∃ guest, guest ∈ grants ∧ guest.inForce goal generation now ∧
      guest.toGrant.gives s requester c

structure GoalWorkBinding where
  goal : GoalControllerEpoch
  memory_handle : Handle
  source_owner : Owner
  source_generation : Nat
  controller_generation : Nat

/-- Audience authorization to read a published Memory version. -/
def goal_publication_read_admitted
    (s : OwnerState) (requester : User) (publication : GoalPublication)
    (grants : List GoalAudienceGrant) (current_controller_generation now : Nat)
    (memory_owner_generation : Nat) (memory : Memory) : Prop :=
  goal_route s requester publication.goal.owner publication.goal
    current_controller_generation now grants (.read memory.kind) ∧
  memory.forgotten = false ∧
  publication.memory_t = memory_t memory ∧
  publication.source_owner = memory_owner memory ∧
  publication.goal.generation = current_controller_generation ∧
  publication.controller_generation = current_controller_generation ∧
  publication.source_generation = memory_owner_generation

/-- Source consent to fork through a live publication for the exact readable admission. -/
def goal_publication_fork_admitted
    (s : OwnerState) (requester : User) (publication : GoalPublication)
    (grants : List GoalAudienceGrant) (current_controller_generation now : Nat)
    (memory_owner_generation : Nat) (memory : Memory) : Prop :=
  goal_publication_read_admitted s requester publication grants
    current_controller_generation now memory_owner_generation memory ∧
  publication.permitted_use = .Fork

/-- Authorization to read the Goal itself. -/
def goal_read_admitted (s : OwnerState) (requester : User) (controller : GoalControllerEpoch)
    (grants : List GoalAudienceGrant) (goal : Goal)
    (current_controller_generation now : Nat) : Prop :=
  controller.goal_handle = goal_handle goal ∧
  controller.owner = goal_owner goal ∧
  controller.generation = current_controller_generation ∧
  (may_read_goals requester (goal_owner goal) ∨
    ∃ guest, guest ∈ grants ∧
      guest.inForce controller current_controller_generation now ∧
      (s.subject guest.subject).gives requester .readsGoals)

/-- Authorization to edit a Goal under its current controller. -/
def goal_edit_admitted (s : OwnerState) (requester : User) (controller : GoalControllerEpoch)
    (grants : List GoalAudienceGrant) (goal : Goal)
    (current_controller_generation now : Nat) : Prop :=
  controller.goal_handle = goal_handle goal ∧
  controller.owner = goal_owner goal ∧
  controller.generation = current_controller_generation ∧
  goal_route s requester (goal_owner goal) controller
    current_controller_generation now grants .worksGoals

/-- Goal read authorization follows from edit authorization. -/
theorem goal_edit_implies_read (s : OwnerState) (requester : User)
    (controller : GoalControllerEpoch) (grants : List GoalAudienceGrant)
    (goal : Goal) (generation now : Nat) :
    goal_edit_admitted s requester controller grants goal generation now →
      goal_read_admitted s requester controller grants goal generation now := by
  rintro ⟨hhandle, howner, hgen, hroute⟩
  refine ⟨hhandle, howner, hgen, ?_⟩
  unfold goal_route at hroute
  cases hroute with
  | inl member =>
      obtain ⟨x, hx, hw⟩ := member
      exact Or.inl ⟨x, hx, x.worksGoals_readsGoals hw⟩
  | inr guest =>
      obtain ⟨g, hmem, hforce, hgives⟩ := guest
      exact Or.inr ⟨g, hmem, hforce,
        Group.gives_mono (c := .worksGoals) (d := .readsGoals)
          (fun r => r.worksGoals_readsGoals)
          ((Grant.gives_iff _ _ _ _).1 hgives).2⟩

/-- Authorization to achieve a Goal. -/
def goal_achieve_admitted (s : OwnerState) (requester : User) (controller : GoalControllerEpoch)
    (grants : List GoalAudienceGrant) (goal : Goal)
    (current_controller_generation now : Nat) : Prop :=
  controller.goal_handle = goal_handle goal ∧
  controller.owner = goal_owner goal ∧
  controller.generation = current_controller_generation ∧
  (may_administer requester (goal_owner goal) ∨
    ∃ guest, guest ∈ grants ∧
      guest.inForce controller current_controller_generation now ∧
      guest.toGrant.gives s requester .worksGoals)

/-- Authorization to write Memory through a Goal work binding. -/
def goal_work_write_admitted
    (s : OwnerState) (requester : User) (binding : GoalWorkBinding)
    (grants : List GoalAudienceGrant) (goal : Goal)
    (current_controller_generation memory_owner_generation now : Nat)
    (memory : Memory) : Prop :=
  goal_route s requester (goal_owner goal) binding.goal
    current_controller_generation now grants (.write memory.kind) ∧
  binding.goal.goal_handle = goal_handle goal ∧
  binding.source_owner = goal_owner goal ∧
  memory.forgotten = false ∧
  binding.memory_handle = memory_handle memory ∧
  binding.source_owner = memory_owner memory ∧
  binding.goal.generation = current_controller_generation ∧
  binding.controller_generation = current_controller_generation ∧
  binding.source_generation = memory_owner_generation ∧
  boundWorkStateAdmitted (goal_state goal) .write = true

/-- Authorization to read Memory through a Goal work binding. -/
def goal_work_read_admitted
    (s : OwnerState) (requester : User) (binding : GoalWorkBinding)
    (grants : List GoalAudienceGrant) (goal : Goal)
    (current_controller_generation memory_owner_generation now : Nat)
    (memory : Memory) : Prop :=
  goal_route s requester (goal_owner goal) binding.goal
    current_controller_generation now grants (.read memory.kind) ∧
  binding.goal.goal_handle = goal_handle goal ∧
  binding.source_owner = goal_owner goal ∧
  memory.forgotten = false ∧
  binding.memory_handle = memory_handle memory ∧
  binding.source_owner = memory_owner memory ∧
  binding.goal.generation = current_controller_generation ∧
  binding.controller_generation = current_controller_generation ∧
  binding.source_generation = memory_owner_generation

/-- Goal audience resolution agrees with `Causa.flattened` for grants in force. -/
theorem goal_route_is_flattened (s : OwnerState) (owner : Owner)
    (goal : GoalControllerEpoch) (generation now : Nat)
    (grants : List GoalAudienceGrant)
    (hforce : ∀ g ∈ grants, g.inForce goal generation now)
    (requester : User) (c : Capability) :
    goal_route s requester owner goal generation now grants c ↔
      (flattened s owner (grants.map (·.toGrant))).gives requester c := by
  rw [flattened_gives]
  unfold goal_route
  constructor
  · rintro (h | ⟨g, hg, _, hs⟩)
    · exact Or.inl h
    · exact Or.inr ⟨g.toGrant, List.mem_map.2 ⟨g, hg, rfl⟩, hs⟩
  · rintro (h | ⟨_, hmap, hs⟩)
    · exact Or.inl h
    · obtain ⟨g, hg, rfl⟩ := List.mem_map.1 hmap
      exact Or.inr ⟨g, hg, hforce g hg, hs⟩

theorem goal_work_write_implies_read
    (s : OwnerState) (requester : User) (binding : GoalWorkBinding)
    (grants : List GoalAudienceGrant) (goal : Goal)
    (controller_generation memory_generation now : Nat) (memory : Memory) :
    goal_work_write_admitted s requester binding grants goal
      controller_generation memory_generation now memory →
    goal_work_read_admitted s requester binding grants goal
      controller_generation memory_generation now memory := by
  intro h
  rcases h with ⟨hroute, hgoal, howner, hlive, hhandle, hmemoryowner,
    hgoalgen, hbindctrl, hsource, _active⟩
  have hread :
      goal_route s requester (goal_owner goal) binding.goal
        controller_generation now grants (.read memory.kind) := by
    unfold goal_route at hroute ⊢
    cases hroute with
    | inl owner => exact Or.inl (may_write_implies_read _ _ _ owner)
    | inr guest =>
        rcases guest with ⟨g, hmem, hforce, hgives⟩
        exact Or.inr ⟨g, hmem, hforce,
          (Grant.gives_iff _ _ _ _).2
            ⟨Role.has_write_read _ _ ((Grant.gives_iff _ _ _ _).1 hgives).1,
              Group.gives_mono (fun r => Role.has_write_read r _)
                ((Grant.gives_iff _ _ _ _).1 hgives).2⟩⟩
  exact ⟨hread, hgoal, howner, hlive, hhandle, hmemoryowner,
    hgoalgen, hbindctrl, hsource⟩

structure ForkSourcePermission where
  requester : User
  source : Memory
  destination_owner : Owner
  source_generation : Nat
  destination_generation : Nat
  source_use : User → Prop
  destination_write : User → Prop

/-- Authorization for an independent fork of a Memory version. -/
def fork_admitted (permission : ForkSourcePermission)
    (source target : Memory)
    (current_source_generation current_destination_generation : Nat) : Prop :=
  memory_t source = memory_t permission.source ∧
  (memory_kind target = .Abstraction ∨ memory_kind target = .Perspective) ∧
  memory_kind source = memory_kind target ∧
  memory_handle source ≠ memory_handle target ∧
  memory_owner target = permission.destination_owner ∧
  permission.source_generation = current_source_generation ∧
  permission.destination_generation = current_destination_generation ∧
  memory_forked_from target = some (memory_t source) ∧
  memory_origins target = memory_origins source ∧
  memory_refs target = memory_refs source ∧
  memory_goal_refs target = memory_goal_refs source ∧
  memory_guided_by target = [] ∧
  permission.source_use permission.requester ∧
  permission.destination_write permission.requester

def fork_source_ancestor_read_admitted (requester : User) (source : Memory) : Prop :=
  may_read requester (memory_owner source) source.kind

end Causa
