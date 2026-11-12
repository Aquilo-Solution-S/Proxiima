/-
Causa — Goal collaboration authority

Access on one exact Goal: the owner's group gives the capability, or a grant
in force on that Goal does (`goal_route`). Editing a Goal, reading a
publication and working a bound Memory are stated over that route. Reading
the Goal itself is wider: it waives the grant's own Goal ceiling
(`goal_read_admitted`). Achieving a Goal and forking a source version have
their own admissions (`goal_achieve_admitted`, `fork_admitted`).
-/

import Causa.Edges
import Causa.Authorization
import Causa.Audience

namespace Causa

def MemoryKind.accessKind : MemoryKind → AccessKind
  | .Fact => .fact
  | .Abstraction => .abstraction
  | .Perspective => .perspective

/- ================================================================
   Goal collaboration authority

   These predicates are deliberately separate from `may_read` and
   `may_write`. A Goal audience is an additional, exact-version route; it is
   never inserted into an `Owner` or used to manufacture owner authority.
   ================================================================ -/

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

/-- A grant on one Goal: a `Grant` (a person or a group, and a role) on one
    exact controller epoch. -/
structure GoalAudienceGrant extends Grant where
  goal : GoalControllerEpoch
  controller_generation : Nat

/-- The grant is on this exact Goal and carries the current controller
    generation. A grant past its expiry is not among the Goal's grants; the
    fence and the expiry are both facts checked again where the operation
    commits. -/
def GoalAudienceGrant.inForce (g : GoalAudienceGrant) (goal : GoalControllerEpoch)
    (current_controller_generation : Nat) : Prop :=
  g.goal = goal ∧ g.controller_generation = current_controller_generation

/-- The route every Goal operation shares: the owner's group gives the
    capability, or a grant in force on this exact Goal does. -/
def goal_route (s : OwnerState) (requester : User) (owner : Owner)
    (goal : GoalControllerEpoch) (generation : Nat)
    (grants : List GoalAudienceGrant) (c : Capability) : Prop :=
  owner.gives requester c ∨
    ∃ guest, guest ∈ grants ∧ guest.inForce goal generation ∧
      guest.toGrant.gives s requester c

structure GoalWorkBinding where
  goal : GoalControllerEpoch
  memory_handle : Handle
  source_owner : Owner
  source_generation : Nat
  controller_generation : Nat

/-- The audience route reads one exact admission.  A Goal's terminal state is
    intentionally absent: completion does not revoke a publication. -/
def goal_publication_read_admitted
    (s : OwnerState) (requester : User) (publication : GoalPublication)
    (grants : List GoalAudienceGrant) (current_controller_generation : Nat)
    (memory_owner_generation : Nat) (memory : Memory) : Prop :=
  goal_route s requester publication.goal.owner publication.goal
    current_controller_generation grants (.read memory.kind.accessKind) ∧
  publication.memory_t = memory_t memory ∧
  publication.source_owner = memory_owner memory ∧
  publication.goal.generation = current_controller_generation ∧
  publication.controller_generation = current_controller_generation ∧
  publication.source_generation = memory_owner_generation

/-- Reading the Goal itself: a role that reads the owner's Goals, or a current
    grant on this exact Goal whose subject gives the requester a role that
    reads Goals. This is not the uniform audience rule: it waives the grant's
    own Goal ceiling, so every guest reads the Goal it was granted on whatever
    the grant's role says about Goals. -/
def goal_read_admitted (s : OwnerState) (requester : User) (controller : GoalControllerEpoch)
    (grants : List GoalAudienceGrant) (goal : Goal)
    (current_controller_generation : Nat) : Prop :=
  controller.goal_handle = goal_handle goal ∧
  controller.owner = goal_owner goal ∧
  controller.generation = current_controller_generation ∧
  (may_read_goals requester (goal_owner goal) ∨
    ∃ guest, guest ∈ grants ∧
      guest.inForce controller current_controller_generation ∧
      (s.subject guest.subject).gives requester .readsGoals)

/-- Modifying or transitioning an existing Goal is Goal work, under one rule
    for members and guests: the role works Goals. A guest's grant counts for
    this exact stable Goal and its current controller generation. Achieving
    the Goal is `goal_achieve_admitted`. -/
def goal_edit_admitted (s : OwnerState) (requester : User) (controller : GoalControllerEpoch)
    (grants : List GoalAudienceGrant) (goal : Goal)
    (current_controller_generation : Nat) : Prop :=
  controller.goal_handle = goal_handle goal ∧
  controller.owner = goal_owner goal ∧
  controller.generation = current_controller_generation ∧
  goal_route s requester (goal_owner goal) controller
    current_controller_generation grants .worksGoals

/-- Whoever edits a Goal reads it. -/
theorem goal_edit_implies_read (s : OwnerState) (requester : User)
    (controller : GoalControllerEpoch) (grants : List GoalAudienceGrant)
    (goal : Goal) (generation : Nat) :
    goal_edit_admitted s requester controller grants goal generation →
      goal_read_admitted s requester controller grants goal generation := by
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

/-- Achieving a Goal closes it. Over the owner that is administration; on the
    Goal's own route it belongs to the grant that works this Goal. This is not
    the uniform audience rule: it asks the owner route for administration and
    the guest route for Goal work. A member's Goal work alone achieves
    nothing. -/
def goal_achieve_admitted (s : OwnerState) (requester : User) (controller : GoalControllerEpoch)
    (grants : List GoalAudienceGrant) (goal : Goal)
    (current_controller_generation : Nat) : Prop :=
  controller.goal_handle = goal_handle goal ∧
  controller.owner = goal_owner goal ∧
  controller.generation = current_controller_generation ∧
  (may_administer requester (goal_owner goal) ∨
    ∃ guest, guest ∈ grants ∧
      guest.inForce controller current_controller_generation ∧
      guest.toGrant.gives s requester .worksGoals)

/-- A work route is same-owner and write-ceiling bounded.  It grants no Goal
    administration, transfer, erase, or owner-membership capability. -/
def goal_work_write_admitted
    (s : OwnerState) (requester : User) (binding : GoalWorkBinding)
    (grants : List GoalAudienceGrant) (goal : Goal)
    (current_controller_generation memory_owner_generation : Nat)
    (memory : Memory) : Prop :=
  goal_route s requester (goal_owner goal) binding.goal
    current_controller_generation grants (.write memory.kind.accessKind) ∧
  binding.goal.goal_handle = goal_handle goal ∧
  binding.source_owner = goal_owner goal ∧
  binding.memory_handle = memory_handle memory ∧
  binding.source_owner = memory_owner memory ∧
  binding.goal.generation = current_controller_generation ∧
  binding.controller_generation = current_controller_generation ∧
  binding.source_generation = memory_owner_generation ∧
  goal_state goal = .Active

/-- Working-draft reads use the same exact binding but do not require the Goal
    to remain Active; completing a Goal is not an implicit access revocation. -/
def goal_work_read_admitted
    (s : OwnerState) (requester : User) (binding : GoalWorkBinding)
    (grants : List GoalAudienceGrant) (goal : Goal)
    (current_controller_generation memory_owner_generation : Nat)
    (memory : Memory) : Prop :=
  goal_route s requester (goal_owner goal) binding.goal
    current_controller_generation grants (.read memory.kind.accessKind) ∧
  binding.goal.goal_handle = goal_handle goal ∧
  binding.source_owner = goal_owner goal ∧
  binding.memory_handle = memory_handle memory ∧
  binding.source_owner = memory_owner memory ∧
  binding.goal.generation = current_controller_generation ∧
  binding.controller_generation = current_controller_generation ∧
  binding.source_generation = memory_owner_generation

/-- One rule: for grants all in force, `goal_route` is exactly the flattened
    lookup of the owner joined with those grants. -/
theorem goal_route_is_flattened (s : OwnerState) (owner : Owner)
    (goal : GoalControllerEpoch) (generation : Nat)
    (grants : List GoalAudienceGrant)
    (hforce : ∀ g ∈ grants, g.inForce goal generation)
    (requester : User) (c : Capability) :
    goal_route s requester owner goal generation grants c ↔
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
    (controller_generation memory_generation : Nat) (memory : Memory) :
    goal_work_write_admitted s requester binding grants goal
      controller_generation memory_generation memory →
    goal_work_read_admitted s requester binding grants goal
      controller_generation memory_generation memory := by
  intro h
  rcases h with ⟨hroute, hgoal, howner, hhandle, hmemoryowner,
    hgoalgen, hbindctrl, hsource, _active⟩
  have hread :
      goal_route s requester (goal_owner goal) binding.goal
        controller_generation grants (.read memory.kind.accessKind) := by
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
  exact ⟨hread, hgoal, howner, hhandle, hmemoryowner,
    hgoalgen, hbindctrl, hsource⟩

structure ForkSourcePermission where
  requester : User
  source : Memory
  destination_owner : Owner
  source_generation : Nat
  destination_generation : Nat
  source_use : User → Prop
  destination_write : User → Prop

/-- An independent adaptation is a fresh A/P series anchored at one exact
    source version. Facts remain immutable admissions; Goals are not Memory
    fork targets. -/
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
  may_read requester (memory_owner source) source.kind.accessKind

end Causa
