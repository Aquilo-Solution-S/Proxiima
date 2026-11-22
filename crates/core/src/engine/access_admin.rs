use crate::MemoryKind;
use crate::access::{EntityId, Relation};
use crate::authz::{
    AuthPath, AuthzContext, AuthzInput, AuthzOperation, AuthzOutcome, MembershipChange,
};
use crate::error::ProtocolError;
use crate::owner_inverse::OwnerEraseTarget;
use crate::spiice::TransferRule;
use crate::storage::StorageError;
use crate::storage_ports::{
    MembershipEndpointAuthority, MembershipMemberAuthority, MembershipWritePermit,
};
use crate::{GroupId, OwnerId, UserId};

use super::Engine;

/// One page of group members from [`Engine::list_members`], in the
/// keyset total order `(member_user_id, relation)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupMemberPage {
    pub members: Vec<(OwnerId, Relation)>,
    /// More members exist past this page; resume from the last returned
    /// `(member, relation)` pair.
    pub has_more: bool,
}

impl Engine {
    /// Seed the first Admin membership for a fresh group.
    ///
    /// Host-only bootstrap: this verb is not registered as an MCP tool. It
    /// admits exactly one Admin row only when the group currently has no Admin;
    /// later membership mutations must use [`Self::add_member`].
    ///
    /// # Errors
    ///
    /// Returns `Forbidden` when the caller lacks owner-erase authority
    /// authority or an authorization veto denies the bootstrap,
    /// `InvalidArgument` when the group already has an Admin, and `Internal`
    /// for storage failures.
    pub async fn bootstrap_group_admin(
        &self,
        authz: &AuthzContext,
        group: GroupId,
        first_admin: UserId,
    ) -> Result<(), ProtocolError> {
        let target = OwnerEraseTarget::GroupOwner { group_id: group };
        if !self.erase_authority_grants(authz, &target).await? {
            return Err(ProtocolError::forbidden(
                "owner-erase authority authorization required",
            ));
        }

        let bootstrap_member_authz = AuthzContext::for_subject(first_admin, AuthPath::System);
        let group_owner = OwnerId::from(group);
        let member_principal = OwnerId::from(first_admin);
        self.veto_and_observe_access_admin(
            &bootstrap_member_authz,
            &group_owner,
            &group_owner,
            AuthzOperation::Membership {
                change: MembershipChange::Add,
                group,
                member: member_principal,
                relation: Relation::Admin,
            },
        )?;
        self.storage()
            .access_admin
            .owner_membership_admin
            .bootstrap_group_admin(group, first_admin, first_admin.into_inner())
            .await
            .map_err(bootstrap_storage_error)
    }

    /// Add a user to a group with one relation.
    ///
    /// # Errors
    ///
    /// Returns `Forbidden` when the caller lacks admin on the group and
    /// `Internal` for storage failures.
    pub async fn add_member(
        &self,
        authz: &AuthzContext,
        group: GroupId,
        member: OwnerId,
        relation: Relation,
    ) -> Result<(), ProtocolError> {
        let group_owner = OwnerId::from(group);
        let permit = self.authorize_owner_admin(authz, &group_owner).await?;
        require_group_manage(authz, &group_owner)?;
        let member_principal = member;
        self.veto_and_observe_access_admin(
            authz,
            &group_owner,
            permit.owner(),
            AuthzOperation::Membership {
                change: MembershipChange::Add,
                group,
                member: member_principal,
                relation,
            },
        )?;
        let membership_permit = MembershipWritePermit::new(
            permit.owner_write_permit(),
            group_owner,
            member_manage_authority(authz, member),
            MembershipEndpointAuthority::Parent,
        );
        self.storage()
            .access_admin
            .owner_membership_admin
            .add_group_member(
                &membership_permit,
                group,
                member,
                relation,
                actor_uuid(authz),
            )
            .await
            .map_err(|err| membership_storage_error("add_group_member", err))
    }

    /// Remove all membership rows for a user in one group.
    ///
    /// # Errors
    ///
    /// Returns `Forbidden` when the caller lacks admin on the group and
    /// `Internal` for storage failures.
    pub async fn remove_member(
        &self,
        authz: &AuthzContext,
        group: GroupId,
        member: OwnerId,
    ) -> Result<(), ProtocolError> {
        let group_owner = OwnerId::from(group);
        let (permit, endpoint_authority) = self
            .authorize_membership_detach(authz, group_owner, member)
            .await?;
        let member_principal = member;
        let current_relations = self
            .storage()
            .access_admin
            .owner_membership_admin
            .list_group_members(authz.owner_scope(), group)
            .await
            .map_err(|err| storage_error("list_group_members", &err))?
            .into_iter()
            .filter_map(|(candidate, relation)| (candidate == member).then_some(relation))
            .collect::<Vec<_>>();
        for relation in current_relations {
            self.veto_and_observe_access_admin(
                authz,
                &group_owner,
                permit.owner(),
                AuthzOperation::Membership {
                    change: MembershipChange::Remove,
                    group,
                    member: member_principal,
                    relation,
                },
            )?;
        }
        let membership_permit = with_parent_caller(
            MembershipWritePermit::new(
                permit.owner_write_permit(),
                group_owner,
                member_manage_authority(authz, member),
                endpoint_authority,
            ),
            authz,
        );
        self.storage()
            .access_admin
            .owner_membership_admin
            .remove_group_member(&membership_permit, group, member)
            .await
            .map_err(|err| membership_storage_error("remove_group_member", err))
    }

    /// Remove one relation of a user in one group; the user's other relations
    /// stay. An absent row is `Ok`, as [`Self::remove_member`] is for a user
    /// with no rows.
    ///
    /// Runs one `Remove` through the authorization hooks. Removing one relation
    /// is not a downgrade: a member who holds only that relation is left with
    /// no role. Use [`Self::replace_member_relation`] to change a relation.
    ///
    /// # Errors
    ///
    /// Returns `Forbidden` when the caller lacks manage on the group or a
    /// veto denies the removal, and `Internal` for storage failures.
    pub async fn remove_member_relation(
        &self,
        authz: &AuthzContext,
        group: GroupId,
        member: OwnerId,
        relation: Relation,
    ) -> Result<(), ProtocolError> {
        let group_owner = OwnerId::from(group);
        let (permit, endpoint_authority) = self
            .authorize_membership_detach(authz, group_owner, member)
            .await?;
        self.veto_and_observe_access_admin(
            authz,
            &group_owner,
            permit.owner(),
            AuthzOperation::Membership {
                change: MembershipChange::Remove,
                group,
                member,
                relation,
            },
        )?;
        let membership_permit = with_parent_caller(
            MembershipWritePermit::new(
                permit.owner_write_permit(),
                group_owner,
                member_manage_authority(authz, member),
                endpoint_authority,
            ),
            authz,
        );
        self.storage()
            .access_admin
            .owner_membership_admin
            .remove_group_member_relation(&membership_permit, group, member, relation)
            .await
            .map_err(|err| membership_storage_error("remove_group_member_relation", err))
    }

    /// Replace the `from` relation of a user in one group with `to`,
    /// atomically: the user holds a role throughout and no reader sees them
    /// with neither relation. The user's other relations stay.
    ///
    /// Runs the authorization hooks for `Remove(from)` and then `Add(to)`,
    /// both before the storage call; a veto on either leaves the membership
    /// unchanged.
    ///
    /// # Errors
    ///
    /// Returns `Forbidden` when the caller lacks manage on the group or a
    /// veto denies either change, `InvalidArgument` when `from == to` or the
    /// user does not hold `from`, and `Internal` for storage failures.
    pub async fn replace_member_relation(
        &self,
        authz: &AuthzContext,
        group: GroupId,
        member: OwnerId,
        from: Relation,
        to: Relation,
    ) -> Result<(), ProtocolError> {
        let group_owner = OwnerId::from(group);
        let permit = self.authorize_owner_admin(authz, &group_owner).await?;
        require_group_manage(authz, &group_owner)?;
        if from == to {
            return Err(ProtocolError::invalid_argument(
                "to",
                "must differ from the relation being replaced",
            ));
        }
        let member_principal = member;
        for (change, relation) in [
            (MembershipChange::Remove, from),
            (MembershipChange::Add, to),
        ] {
            self.veto_and_observe_access_admin(
                authz,
                &group_owner,
                permit.owner(),
                AuthzOperation::Membership {
                    change,
                    group,
                    member: member_principal,
                    relation,
                },
            )?;
        }
        let membership_permit = with_parent_caller(
            MembershipWritePermit::new(
                permit.owner_write_permit(),
                group_owner,
                member_manage_authority(authz, member),
                MembershipEndpointAuthority::Parent,
            ),
            authz,
        );
        self.storage()
            .access_admin
            .owner_membership_admin
            .replace_group_member_relation(
                &membership_permit,
                group,
                member,
                from,
                to,
                actor_uuid(authz),
            )
            .await
            .map_err(replace_storage_error)
    }

    /// List one page of members for one group, in the keyset total order
    /// `(member_user_id, relation)`.
    ///
    /// # Errors
    ///
    /// Returns `Forbidden` when the caller lacks admin on the group and
    /// `Internal` for storage failures.
    pub async fn list_members(
        &self,
        authz: &AuthzContext,
        group: GroupId,
        limit: u32,
        after: Option<(OwnerId, Relation)>,
    ) -> Result<GroupMemberPage, ProtocolError> {
        let group_owner = OwnerId::from(group);
        self.authorize_owner_admin(authz, &group_owner).await?;
        let fetch = i64::from(limit).saturating_add(1);
        let mut members = self
            .storage()
            .access_admin
            .owner_membership_admin
            .list_group_members_page(authz.owner_scope(), group, after, fetch)
            .await
            .map_err(|err| storage_error("list_group_members_page", &err))?;
        let has_more = members.len() > usize::try_from(limit).unwrap_or(usize::MAX);
        members.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        Ok(GroupMemberPage { members, has_more })
    }

    /// `Proxiima.OwnerAuthority.membershipConsent`.
    async fn authorize_membership_detach(
        &self,
        authz: &AuthzContext,
        parent: OwnerId,
        member: OwnerId,
    ) -> Result<(super::pipeline::WritePermit, MembershipEndpointAuthority), ProtocolError> {
        if authz.may_manage(&parent) {
            return Ok((
                self.authorize_owner_admin(authz, &parent).await?,
                MembershipEndpointAuthority::Parent,
            ));
        }
        if authz.may_manage(&member) {
            return Ok((
                self.authorize_owner_admin(authz, &member).await?,
                MembershipEndpointAuthority::Member,
            ));
        }
        Err(ProtocolError::forbidden(
            "requires manage on the parent or attached group",
        ))
    }

    /// Transfer one memory's owner to `to_owner`. This is an owner
    /// TRANSFER, not an ACL flag or a share row: the series moves in place
    /// (`MemoryHeadAligned`) and leaves the prior owner's view entirely.
    ///
    /// Goals do not transfer, so a Goal entity is refused here, before any
    /// owner lookup or storage call. The `goal_head_t_only` trigger is the
    /// DDL backstop for the same rule.
    ///
    /// A memory whose schema declares `TransferRule::NotTransferable` is
    /// refused here too, kernel or Spiice, with the reason the schema
    /// declared, once both sides authorize and before storage is reached;
    /// the storage transfer refuses it again for a caller that bypasses the
    /// engine.
    ///
    /// Authorization is **admin on both sides**:
    /// * Owner admin on the entity's CURRENT owner — for a personal
    ///   owner that is the subject's own `Role::personal()`; for a group
    ///   owner it is a member holding `Role::admin()` (`manage = true`),
    ///   re-checked through `require_group_manage`. This is the side the
    ///   request is scoped to: the write permit that carries the transfer
    ///   into storage is the SOURCE owner's, so a caller gives a memory
    ///   away while acting AS its owner.
    /// * The caller's own personal ID is refused as `to_owner`. Every other
    ///   destination requires administration and management on that owner,
    ///   which only a group role carries on the served path.
    ///
    /// Storage registers an unseen destination as personal inside the transfer
    /// transaction (`ensure_owner_row`), so destination-owned rows' `owner_id`
    /// foreign keys hold without a migration-seeded owner.
    ///
    /// The ownership oracle is closed by resolving the home only through the
    /// caller's readable-owner set. Missing and foreign-owned entities are
    /// therefore both invisible (`NotFound`) before the source manage gate;
    /// the `to_owner == current_owner` refusal fires only AFTER both sides
    /// authorize, so an authorized caller still gets the useful distinction.
    ///
    /// See `Proxiima.OwnerAuthority.transferOrder` and
    /// `Proxiima.OwnerAuthority.transferConsent`.
    ///
    /// # Errors
    ///
    /// Returns `InvalidArgument` for a Goal entity, for a memory whose schema
    /// declares `NotTransferable`, or for the caller's own personal ID as
    /// destination. Returns `NotFound` when the entity has no visible home
    /// owner. Returns `Forbidden` when the caller lacks
    /// admin/manage authority on either side. Returns `InvalidArgument`
    /// when an authorized caller names the current owner as the
    /// destination. Returns `Internal` for storage failures and when the
    /// host cannot route the destination's embeddings, and
    /// `NotFound` if the storage transfer finds no matching row (owner
    /// changed concurrently between the lookup and the write).
    pub async fn transfer_to_owner(
        &self,
        authz: &AuthzContext,
        entity: EntityId,
        to_owner: OwnerId,
    ) -> Result<(), ProtocolError> {
        self.operation_authority(authz)?;
        // `Proxiima.Goals.transferGoal`.
        if matches!(entity, EntityId::Goal(_)) {
            return Err(ProtocolError::invalid_argument(
                "entity",
                "goals do not transfer",
            ));
        }
        if OwnerId::from(authz.subject()) == to_owner {
            return Err(ProtocolError::invalid_argument(
                "to_owner",
                "transfer destination must be a group owner: receiving-side consent is \
                 group-manage authority, which no personal owner can grant",
            ));
        }
        // Find the source among readable owners before authorization so a refusal cannot disclose its home.
        let read = authz.readable_owners(MemoryKind::Fact);
        let (current_owner, _, schema) = self
            .storage()
            .access_admin
            .owner_access_read
            .visible_home_owner(authz.owner_scope(), entity, &read)
            .await
            .map_err(|err| storage_error("visible_home_owner", &err))?
            .ok_or_else(|| ProtocolError::not_found("entity not found"))?;
        let mut permit = self.authorize_owner_admin(authz, &current_owner).await?;
        if OwnerId::from(authz.subject()) != current_owner {
            require_group_manage(authz, &current_owner)?;
        }
        self.authorize_transfer_destination(authz, &to_owner)
            .await?;

        // `Proxiima.OwnerAuthority.transferAdmitted`.
        let surfaces = self.owner_surfaces();
        if let Some(schema) = &schema
            && let Some(why) = surfaces.untransferable_schema(schema.as_str())
        {
            return Err(ProtocolError::invalid_argument(
                "entity",
                format!(
                    "memories of schema {schema} do not transfer: {}",
                    TransferRule::refusal_headline(why)
                ),
            ));
        }

        permit.authorize_transfer_to(to_owner);

        // Report a same-owner destination only after both sides authorize, since it reveals the entity's home.
        if current_owner == to_owner {
            return Err(ProtocolError::invalid_argument(
                "to_owner",
                "transfer destination is already the current owner",
            ));
        }

        self.veto_and_observe_access_admin(
            authz,
            &current_owner,
            permit.owner(),
            AuthzOperation::EntityTransfer { entity, to_owner },
        )?;

        let embedding_spaces = self.write_route(&to_owner).await?.write_spaces();
        // Old-owner readers lose future access unless they hold a destination role, and past reads are never retracted.
        let transferred = self
            .storage()
            .access_admin
            .owner_transfer
            .transfer_to_owner(
                permit.owner_write_permit(),
                entity,
                to_owner,
                &surfaces,
                &embedding_spaces,
            )
            .await
            .map_err(|err| storage_error("transfer_to_owner", &err))?;

        if !transferred {
            return Err(ProtocolError::not_found(
                "entity owner changed concurrently",
            ));
        }
        Ok(())
    }

    /// Receiving-side role precedence follows `Proxiima.OwnerAuthority.destinationRoute`.
    async fn authorize_transfer_destination(
        &self,
        authz: &AuthzContext,
        to_owner: &OwnerId,
    ) -> Result<(), ProtocolError> {
        if authz.role_for_owner(to_owner).is_some() {
            self.authorize_owner_admin(authz, to_owner).await?;
            return require_group_manage(authz, to_owner);
        }
        self.authorize_transfer_destination_out_of_band(authz, to_owner)
            .await
    }

    /// Resolve receiving-side consent for a subject eligible to use storage membership.
    async fn authorize_transfer_destination_out_of_band(
        &self,
        authz: &AuthzContext,
        to_owner: &OwnerId,
    ) -> Result<(), ProtocolError> {
        let refused = || ProtocolError::forbidden("requires manage on this owner");
        // Delegated grants decode with `manage = false`; stored membership cannot supply that bit.
        if authz.auth_path() == AuthPath::Delegated || authz.verified_forwarded_owner().is_some() {
            return Err(refused());
        }
        let subject = authz.subject();
        let memberships = self
            .storage()
            .access_admin
            .owner_access_read
            .resolve_membership(authz.owner_scope(), &OwnerId::from(subject))
            .await
            .map_err(|err| storage_error("resolve_membership", &err))?;
        let consents = memberships.iter().any(|row| {
            if OwnerId::from(row.group) != *to_owner {
                return false;
            }
            row.role.administers() && row.role.manages()
        });
        if consents { Ok(()) } else { Err(refused()) }
    }

    fn veto_and_observe_access_admin(
        &self,
        authz: &AuthzContext,
        requested: &OwnerId,
        resolved: &OwnerId,
        operation: AuthzOperation,
    ) -> Result<(), ProtocolError> {
        let input = AuthzInput {
            authz,
            requested,
            resolved,
            operation,
        };
        match self.registry.run_authorization_vetoes(&input) {
            Ok(()) => {
                self.registry
                    .run_authorization_observers(&input, AuthzOutcome::Allowed);
                Ok(())
            }
            Err(err) => {
                self.registry
                    .run_authorization_observers(&input, AuthzOutcome::DeniedVeto);
                Err(err)
            }
        }
    }
}

/// Membership-admin and group transfer require the `manage` bit, not merely
/// owner admin. `authorize_owner_admin` is satisfied by any role that
/// administers the owner, and a host may resolve one that administers
/// without managing. Consult `may_manage` explicitly so only roles that
/// manage (e.g. `Role::admin`) mutate membership or move a group-owned
/// entity to another owner.
fn require_group_manage(authz: &AuthzContext, group_owner: &OwnerId) -> Result<(), ProtocolError> {
    if authz.may_manage(group_owner) {
        Ok(())
    } else {
        Err(ProtocolError::forbidden("requires manage on this owner"))
    }
}

fn member_manage_authority(authz: &AuthzContext, member: OwnerId) -> MembershipMemberAuthority {
    if authz.may_manage(&member) {
        MembershipMemberAuthority::CallerManagesMember
    } else {
        MembershipMemberAuthority::CallerDoesNotManageMember
    }
}

fn actor_uuid(authz: &AuthzContext) -> uuid::Uuid {
    authz.subject().into_inner()
}

fn storage_error(context: &str, err: &StorageError) -> ProtocolError {
    super::errors::internal_storage_error(context, err)
}

fn membership_storage_error(context: &str, err: StorageError) -> ProtocolError {
    super::errors::map_write_storage_error(err, "membership", context)
}

fn bootstrap_storage_error(err: StorageError) -> ProtocolError {
    super::errors::map_write_storage_error(err, "group", "group not found")
}

/// `Conflict` is the member not holding `from`: the caller's argument, so
/// `invalid_argument` as for the bootstrap's existing Admin.
fn replace_storage_error(err: StorageError) -> ProtocolError {
    super::errors::map_write_storage_error(err, "from", "group not found")
}

/// Route the parent-authorized mutation's commit-time management recheck.
fn with_parent_caller<'a>(
    permit: MembershipWritePermit<'a>,
    authz: &AuthzContext,
) -> MembershipWritePermit<'a> {
    // `Proxiima.OwnerAuthority.MembershipEndpoint`.
    match permit.endpoint_authority() {
        MembershipEndpointAuthority::Parent => permit.with_caller(authz.subject()),
        MembershipEndpointAuthority::Member => permit,
    }
}

#[cfg(test)]
mod tests {
    use crate::authz::{AuthPath, AuthzContext};
    use crate::{ErrorCode, GroupId, OwnerId, SpiiceRegistry, UserId};
    use uuid::Uuid;

    #[test]
    fn access_admin_bootstrap_group_admin_is_not_mcp_exposed() {
        let frozen = SpiiceRegistry::default().freeze_or_panic_for_tests();

        for tool in frozen.list_mcp_tools() {
            assert_ne!(
                tool.name, "bootstrap_group_admin",
                "bootstrap_group_admin must remain host-only"
            );
            for spec in tool.action_arg_specs {
                assert_ne!(
                    spec.action, "bootstrap_group_admin",
                    "bootstrap_group_admin must not be exposed as an MCP action"
                );
            }
            assert!(
                !tool
                    .args_schema
                    .to_string()
                    .contains("bootstrap_group_admin"),
                "bootstrap_group_admin must not appear in MCP schemas"
            );
        }
    }

    #[tokio::test]
    async fn access_admin_bootstrap_group_admin_requires_operator_authority() {
        let engine = crate::Engine::new(
            SpiiceRegistry::new().freeze_or_panic_for_tests(),
            crate::StoragePorts::rejecting_for_tests(),
        );
        let group = GroupId::new(Uuid::now_v7());
        let first_admin = UserId::new(Uuid::now_v7());
        let authz = AuthzContext::for_subject(first_admin, AuthPath::HostBearer);

        let err = engine
            .bootstrap_group_admin(&authz, group, first_admin)
            .await
            .expect_err("non-operator caller must not bootstrap group admin");
        assert_eq!(err.code, ErrorCode::Forbidden);
    }

    #[tokio::test]
    async fn access_admin_transfer_refuses_goal_entities_before_owner_lookup() {
        let engine = crate::Engine::new(
            SpiiceRegistry::new().freeze_or_panic_for_tests(),
            crate::StoragePorts::rejecting_for_tests(),
        );
        let caller = UserId::new(Uuid::now_v7());
        let authz = AuthzContext::for_subject(caller, AuthPath::HostBearer);
        let goal = crate::EntityId::Goal(crate::GoalId::new(Uuid::now_v7()));
        let destination = OwnerId::from(GroupId::new(Uuid::now_v7()));

        // The default engine has no storage: reaching the owner lookup would
        // surface Internal, so InvalidArgument proves the gate fired first.
        let err = engine
            .transfer_to_owner(&authz, goal, destination)
            .await
            .expect_err("goals do not transfer");
        assert_eq!(err.code, ErrorCode::InvalidArgument);
        assert!(
            err.message.contains("goals do not transfer"),
            "refusal must state the ruling: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn access_admin_transfer_refuses_a_personal_destination() {
        let engine = crate::Engine::new(
            SpiiceRegistry::new().freeze_or_panic_for_tests(),
            crate::StoragePorts::rejecting_for_tests(),
        );
        let caller = UserId::new(Uuid::now_v7());
        let authz = AuthzContext::for_subject(caller, AuthPath::HostBearer);
        let entity = crate::EntityId::Memory(crate::MemoryId::new(Uuid::now_v7()));

        let err = engine
            .transfer_to_owner(&authz, entity, OwnerId::from(caller))
            .await
            .expect_err("a personal owner cannot grant receiving-side consent");
        assert_eq!(err.code, ErrorCode::InvalidArgument);
        assert!(
            err.message.contains("must be a group owner"),
            "refusal must name the destination rule: {}",
            err.message
        );
    }

    /// Stored membership that would consent: an Admin row on `group`.
    #[derive(Debug)]
    struct AdminOn(GroupId);

    #[async_trait::async_trait]
    impl crate::OwnerAccessReadPort for AdminOn {
        async fn resolve_membership(
            &self,
            _owner_scope: Option<&crate::OwnerScope>,
            _member: &OwnerId,
        ) -> Result<Vec<crate::MembershipRow>, crate::StorageError> {
            Ok(vec![crate::MembershipRow {
                group: self.0,
                role: crate::Role::admin(),
            }])
        }

        async fn visible_home_owner(
            &self,
            _owner_scope: Option<&crate::OwnerScope>,
            _entity: crate::EntityId,
            _read_owners: &[OwnerId],
        ) -> Result<
            Option<(OwnerId, crate::EntityKind, Option<crate::SchemaId>)>,
            crate::StorageError,
        > {
            Ok(None)
        }

        async fn home_owner(
            &self,
            _owner_scope: Option<&crate::OwnerScope>,
            _entity: crate::EntityId,
        ) -> Result<Option<OwnerId>, crate::StorageError> {
            Ok(None)
        }
    }

    /// Against a stored Admin row that admits an ordinary context, only the
    /// forwarded-owner exclusion refuses a forwarded one. (A real forwarded
    /// context is also refused earlier, at the source gate, so a transfer
    /// cannot show this rule alone.)
    #[tokio::test]
    async fn a_forwarded_context_is_refused_as_a_destination_out_of_band() {
        let caller = UserId::new(Uuid::now_v7());
        let destination = GroupId::new(Uuid::now_v7());
        let engine = crate::Engine::new(
            SpiiceRegistry::new().freeze_or_panic_for_tests(),
            crate::StoragePorts::rejecting_with_owner_access_read(std::sync::Arc::new(AdminOn(
                destination,
            ))),
        );
        let ordinary = AuthzContext::for_subject(caller, AuthPath::HostBearer);
        engine
            .authorize_transfer_destination_out_of_band(&ordinary, &destination.into())
            .await
            .expect("the stored Admin row consents for an ordinary context");
        let forwarded = AuthzContext::from_verified_forwarded_owner(
            caller,
            OwnerId::from(caller),
            crate::WriterAttribution {
                by: Uuid::now_v7(),
                via: None,
            },
        )
        .seal_verified_scope();

        let refusal = engine
            .authorize_transfer_destination_out_of_band(&forwarded, &destination.into())
            .await
            .expect_err("a forwarded context cannot borrow ordinary membership");

        assert_eq!(refusal.code, ErrorCode::Forbidden);
    }
}
