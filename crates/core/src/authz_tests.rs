use super::*;
use crate::MAX_OPERATOR_LABEL_CHARS;
use crate::access::Role;
use crate::protocol::tool as protocol_tool;
use crate::protocol::{action as protocol_action, resource as protocol_resource};
use crate::{GroupId, UserId};
use futures_util::StreamExt;
use std::sync::atomic::Ordering;
use tokio::sync::mpsc;
use tokio::time;

fn forwarded_writer() -> WriterAttribution {
    WriterAttribution {
        by: uuid::Uuid::from_u128(101),
        via: Some(uuid::Uuid::from_u128(102)),
    }
}

fn owner() -> OwnerId {
    OwnerId::from(UserId::new(uuid::Uuid::now_v7()))
}

fn claims() -> serde_json::Map<String, serde_json::Value> {
    serde_json::json!({
        "iss": "https://issuer.example",
        "sub": "subject",
        "application": {"request": "claim-value", "enabled": true}
    })
    .as_object()
    .expect("claim fixture is an object")
    .clone()
}

#[test]
fn writer_attribution_requires_by_and_explicit_nullable_via() {
    let by = uuid::Uuid::now_v7();
    let via = uuid::Uuid::now_v7();
    for via in [None, Some(via)] {
        let writer = WriterAttribution { by, via };
        let json = serde_json::to_value(writer).unwrap();
        assert_eq!(json, serde_json::json!({ "by": by, "via": via }));
        assert_eq!(
            serde_json::from_value::<WriterAttribution>(json).unwrap(),
            writer
        );
    }
    for invalid in [
        serde_json::json!({ "by": by }),
        serde_json::json!({ "via": null }),
        serde_json::json!({ "by": null, "via": null }),
        serde_json::json!({ "by": "not-a-uuid", "via": null }),
        serde_json::json!({ "by": by, "via": "not-a-uuid" }),
        serde_json::json!({ "by": by, "via": 17 }),
    ] {
        assert!(
            serde_json::from_value::<WriterAttribution>(invalid.clone()).is_err(),
            "{invalid}"
        );
    }
}

#[tokio::test]
async fn writer_is_sealed_once_and_preserved_by_narrowing_and_revalidation() {
    let subject = UserId::new(uuid::Uuid::now_v7());
    let group = OwnerId::from(GroupId::new(uuid::Uuid::now_v7()));
    let context = AuthzContext::for_subject_with_role(
        subject,
        [(group, Role::editor())],
        AuthPath::HostBearer,
    );
    assert_eq!(
        context.writer(),
        Some(WriterAttribution {
            by: subject.into_inner(),
            via: None
        })
    );
    let writer = forwarded_writer();
    let context = context.with_attested_writer(writer).unwrap();
    let sealed = authenticate(
        &VerifiedAuthenticator { context },
        &Credentials::Bearer("verified".into()),
    )
    .await
    .unwrap();
    assert_eq!(sealed.writer(), Some(writer));
    assert_eq!(sealed.owner_scope().unwrap().writer(), writer);
    let narrowed = sealed.clone().narrowed_to_owner(group).unwrap();
    assert_eq!(narrowed.writer(), Some(writer));
    assert_eq!(narrowed.owner_scope().unwrap().writer(), writer);
    assert_eq!(narrowed.identity.writer, Some(writer));
    assert_eq!(narrowed.subject(), Some(subject));
    assert_eq!(narrowed.principal(), OwnerId::from(subject));
    assert!(
        sealed
            .with_attested_writer(WriterAttribution {
                by: uuid::Uuid::now_v7(),
                via: None,
            })
            .is_none()
    );
}

#[tokio::test]
async fn writer_missing_at_authentication_refuses_to_seal() {
    let subject = UserId::new(uuid::Uuid::now_v7());
    let mut context = AuthzContext::for_subject(subject, AuthPath::HostBearer);
    context.identity.writer = None;
    assert_eq!(
        authenticate(
            &VerifiedAuthenticator { context },
            &Credentials::Bearer("verified".into()),
        )
        .await,
        Err(AuthError::InvalidCredentials)
    );
}

/// No bearer, no authenticator: the runtime's witness alone mints a
/// System context whose owner scope is sealed like an authenticated one.
#[test]
fn a_system_context_is_sealed_from_the_witness_without_a_bearer() {
    let authority = SystemAuthority::new(SystemAuthorityBinding::fresh());
    let subject = UserId::new(uuid::Uuid::now_v7());
    let group = OwnerId::from(GroupId::new(uuid::Uuid::now_v7()));
    let roles = OwnerRoles::for_subject(subject, [(group, Role::admin())]).expect("roles");

    let context = AuthzContext::for_system(&authority, roles);

    assert_eq!(context.auth_path(), AuthPath::System);
    assert_eq!(context.subject(), Some(subject));
    let writer = WriterAttribution {
        by: subject.into_inner(),
        via: None,
    };
    assert_eq!(context.writer(), Some(writer));
    assert_eq!(context.owner_scope().unwrap().writer(), writer);
    assert!(context.owner_scope().is_some(), "the owner scope is sealed");
    assert!(context.may_write(&group, MemoryKind::Fact));
    assert!(
        !context.can_access_owner(&OwnerId::from(GroupId::new(uuid::Uuid::now_v7()))),
        "no owner beyond the roles it was given"
    );
}

#[test]
fn insecure_single_owner_scope_cannot_expand_beyond_its_owner() {
    // With bare owner ids the synthetic subject's own id *is* the owner,
    // so the old "same uuid, other kind" owner no longer exists; the
    // property kept is that the scope reaches no owner but its own.
    let authority = SystemAuthority::new(SystemAuthorityBinding::fresh());
    let group_id = GroupId::new(uuid::Uuid::now_v7());
    let group = OwnerId::from(group_id);
    let personal = OwnerId::from(UserId::new(uuid::Uuid::now_v7()));
    let context = AuthzContext::for_insecure_single_owner(&authority, group);

    assert_eq!(context.auth_path(), AuthPath::HostBearer);
    assert_eq!(context.readable_owners(MemoryKind::Fact), vec![group]);
    assert_eq!(context.writable_owners(MemoryKind::Fact), vec![group]);
    let scope = context.owner_scope().expect("sealed single-owner scope");
    assert_eq!(scope.readable_owners(MemoryKind::Fact), vec![group]);
    assert_eq!(scope.writable_owners(MemoryKind::Fact), vec![group]);
    assert_eq!(
        context.writer(),
        Some(WriterAttribution {
            by: group.into_inner(),
            via: None
        })
    );
    assert_eq!(scope.writer(), context.writer().unwrap());
    assert!(!context.can_access_owner(&personal));
    assert!(!scope.may_write(&personal, MemoryKind::Fact));
    assert!(
        scope
            .add_group_role(GroupId::new(uuid::Uuid::now_v7()), Role::admin())
            .is_none()
    );
    assert!(context.clone().narrowed_to_owner(personal).is_none());
    assert!(context.narrowed_to_owner(group).is_some());
}

fn receiver_stream<T: Send + 'static>(
    receiver: mpsc::UnboundedReceiver<T>,
) -> impl Stream<Item = T> + Send {
    futures_util::stream::unfold(receiver, |mut receiver| async {
        receiver.recv().await.map(|item| (item, receiver))
    })
}

#[test]
fn single_owner_context_is_owner_accessible() {
    let o = owner();
    let ctx = AuthzContext::single_owner(&o, AuthPath::System);

    assert_eq!(ctx.auth_path, AuthPath::System);
    assert!(ctx.may_write(&o, MemoryKind::Fact));
    assert!(ctx.can_access_owner(&o));
    assert!(ctx.expires_at().is_none());
    assert!(ctx.owner_scope().is_none());
}

struct VerifiedAuthenticator {
    context: AuthzContext,
}

#[async_trait]
impl Authenticator for VerifiedAuthenticator {
    async fn authenticate(&self, _creds: &Credentials) -> Result<AuthzContext, AuthError> {
        Ok(self.context.clone())
    }
}

struct ForwardedOwnerAccess {
    subject: UserId,
    owner: OwnerId,
    role: crate::ForwarderRole,
    registered: bool,
    probes: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl OwnerAccessPort for ForwardedOwnerAccess {
    async fn resolve_roles_for_subject(
        &self,
        _subject: UserId,
    ) -> Result<OwnerRoles, crate::AccessError> {
        panic!("forwarding must not enumerate memberships")
    }

    fn is_forwarder(&self, subject: UserId) -> Result<bool, crate::AccessError> {
        Ok(subject == self.subject)
    }

    async fn resolve_forwarded_owner_role(
        &self,
        subject: UserId,
        owner: OwnerId,
    ) -> Result<Option<crate::ForwarderRole>, crate::AccessError> {
        self.probes.fetch_add(1, Ordering::SeqCst);
        Ok(
            (subject == self.subject && owner == self.owner && self.registered)
                .then_some(self.role),
        )
    }
}

#[test]
fn group_goal_work_follows_membership_not_the_memory_write_ceiling() {
    let group = OwnerId::from(GroupId::new(uuid::Uuid::now_v7()));
    let member = |role| {
        AuthzContext::for_subject_with_role(
            UserId::new(uuid::Uuid::now_v7()),
            [(group, role)],
            AuthPath::HostBearer,
        )
    };
    let viewer = member(Role::viewer());
    assert!(viewer.may_run_tool(&group, OwnerAccess::GoalWork));
    assert!(!viewer.may_run_tool(&group, OwnerAccess::Write));
    assert!(!viewer.may_write(&group, MemoryKind::Fact));
    assert!(!viewer.may_write(&group, MemoryKind::Perspective));
    assert!(member(Role::admin()).may_work_goals(&group));
    assert!(!member(Role::ingest()).may_work_goals(&group));
    let delegated = AuthzContext::for_subject_with_role(
        UserId::new(uuid::Uuid::now_v7()),
        [(group, Role::viewer())],
        AuthPath::Delegated,
    );
    assert!(
        !delegated.may_work_goals(&group),
        "a delegated request works Goals only where it administers"
    );
    let outsider = OwnerId::from(GroupId::new(uuid::Uuid::now_v7()));
    assert!(!viewer.may_work_goals(&outsider));
    assert!(!member(Role::admin()).may_work_goals(&outsider));
}

#[tokio::test]
async fn forwarded_owner_scope_is_exact_and_keeps_the_forwarding_identity() {
    let subject = UserId::new(uuid::Uuid::now_v7());
    let stranger = OwnerId::from(UserId::new(uuid::Uuid::now_v7()));
    let unrelated = OwnerId::from(GroupId::new(uuid::Uuid::now_v7()));
    let expires = SystemTime::now() + Duration::from_mins(1);
    for owner in [OwnerId::from(subject), stranger] {
        for forwarding_role in [
            crate::ForwarderRole::Viewer,
            crate::ForwarderRole::Ingest,
            crate::ForwarderRole::Editor,
        ] {
            let role = forwarding_role.role();
            let port = ForwardedOwnerAccess {
                subject,
                owner,
                role: forwarding_role,
                registered: true,
                probes: std::sync::atomic::AtomicUsize::new(0),
            };
            let pending =
                AuthzContext::from_verified_forwarded_owner(subject, owner, forwarded_writer())
                    .with_expires_at(Some(expires))
                    .with_tool_scope(ToolScope::Palette(Vec::new()))
                    .with_trusted_model_id("trusted-forwarder-model")
                    .unwrap();
            assert!(pending.readable_owners(MemoryKind::Fact).is_empty());
            assert!(!pending.can_access_owner(&OwnerId::from(subject)));
            assert!(
                pending
                    .clone()
                    .with_host_resolved_role(unrelated.as_group(), Role::admin())
                    .is_none()
            );
            assert!(
                pending
                    .with_authenticated_forwarded_owner(&port, owner)
                    .await
                    .is_none(),
                "unsealed claims grant nothing"
            );
            let sealed = authenticate(
                &VerifiedAuthenticator { context: pending },
                &Credentials::Bearer("verified".into()),
            )
            .await
            .unwrap();
            assert!(
                sealed
                    .with_authenticated_forwarded_owner(&port, unrelated)
                    .await
                    .is_none()
            );
            assert_eq!(
                port.probes.load(Ordering::SeqCst),
                0,
                "selection mismatch must precede target lookup"
            );
            let bound = sealed
                .with_authenticated_forwarded_owner(&port, owner)
                .await
                .unwrap();
            assert_eq!(port.probes.load(Ordering::SeqCst), 1);
            assert_eq!(bound.subject(), Some(subject));
            assert_eq!(bound.writer(), Some(forwarded_writer()));
            assert_eq!(bound.identity.writer, Some(forwarded_writer()));
            assert_eq!(bound.principal(), OwnerId::from(subject));
            assert_eq!(bound.verified_forwarded_owner(), Some(owner));
            assert_eq!(bound.role_for_owner(&owner), Some(role));
            assert!(!bound.may_work_goals(&owner), "forwarding works no Goals");
            assert_eq!(bound.expires_at(), Some(expires));
            assert_eq!(bound.trusted_model_id(), Some("trusted-forwarder-model"));
            assert_eq!(bound.tool_scope(), &ToolScope::Palette(Vec::new()));
            assert_eq!(bound.readable_owners(MemoryKind::Fact), [owner]);
            assert!(!bound.may_manage(&owner));
            for kind in MemoryKind::ALL {
                assert_eq!(bound.may_read(&owner, kind), role.may_read(kind));
                assert_eq!(bound.may_write(&owner, kind), role.may_write(kind));
                assert!(!bound.may_read(&unrelated, kind));
                assert!(!bound.may_write(&unrelated, kind));
            }
            if owner != OwnerId::from(subject) {
                assert!(!bound.may_read(&OwnerId::from(subject), MemoryKind::Fact));
            }
            assert!(bound.clone().narrowed_to_owner(unrelated).is_none());
            assert!(
                bound
                    .with_authenticated_group_role(&port, unrelated.as_group())
                    .await
                    .is_none()
            );
            let scope = bound.owner_scope().unwrap();
            assert_eq!(scope.subject(), subject);
            assert_eq!(scope.writer(), forwarded_writer());
            assert_eq!(scope.readable_owners(MemoryKind::Fact), [owner]);
            assert!(scope.managed_owners().is_empty());
        }
    }
}

#[tokio::test]
async fn forwarded_owner_refuses_expiry_unknown_owners_and_unconfigured_subjects() {
    let subject = UserId::new(uuid::Uuid::now_v7());
    let owner = OwnerId::from(UserId::new(uuid::Uuid::now_v7()));
    let port = ForwardedOwnerAccess {
        subject,
        owner,
        role: crate::ForwarderRole::Editor,
        registered: false,
        probes: std::sync::atomic::AtomicUsize::new(0),
    };
    let expired = AuthzContext::from_verified_forwarded_owner(subject, owner, forwarded_writer())
        .with_expires_at(Some(SystemTime::UNIX_EPOCH))
        .seal_verified_scope()
        .unwrap();
    assert!(
        expired
            .with_authenticated_forwarded_owner(&port, owner)
            .await
            .is_none()
    );
    let unconfigured = AuthzContext::from_verified_forwarded_owner(
        UserId::new(uuid::Uuid::now_v7()),
        owner,
        forwarded_writer(),
    )
    .seal_verified_scope()
    .unwrap();
    assert!(
        unconfigured
            .with_authenticated_forwarded_owner(&port, owner)
            .await
            .is_none()
    );
    assert_eq!(port.probes.load(Ordering::SeqCst), 0);
    let unknown = AuthzContext::from_verified_forwarded_owner(subject, owner, forwarded_writer())
        .seal_verified_scope()
        .unwrap();
    assert!(
        unknown
            .with_authenticated_forwarded_owner(&port, owner)
            .await
            .is_none()
    );
    assert_eq!(port.probes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn forwarded_owner_binding_cannot_extend_the_sealed_expiry() {
    let subject = UserId::new(uuid::Uuid::now_v7());
    let owner = OwnerId::from(UserId::new(uuid::Uuid::now_v7()));
    let port = ForwardedOwnerAccess {
        subject,
        owner,
        role: crate::ForwarderRole::Editor,
        registered: true,
        probes: std::sync::atomic::AtomicUsize::new(0),
    };
    let sealed_expiry = SystemTime::now() + Duration::from_mins(2);
    let pending = AuthzContext::from_verified_forwarded_owner(subject, owner, forwarded_writer())
        .with_expires_at(Some(sealed_expiry))
        .seal_verified_scope()
        .unwrap();
    let earlier = sealed_expiry - Duration::from_mins(1);
    for (changed, expected) in [
        (None, sealed_expiry),
        (Some(sealed_expiry + Duration::from_mins(1)), sealed_expiry),
        (Some(earlier), earlier),
    ] {
        let bound = pending
            .clone()
            .with_expires_at(changed)
            .with_authenticated_forwarded_owner(&port, owner)
            .await
            .unwrap();
        assert_eq!(bound.expires_at(), Some(expected));
        assert_eq!(bound.owner_scope().unwrap().expires_at(), Some(expected));
        assert_eq!(
            bound.clone().narrowed_to_owner(owner).unwrap().expires_at(),
            Some(expected)
        );
    }
    let expired = pending.with_expires_at(Some(SystemTime::UNIX_EPOCH));
    assert!(
        expired
            .with_authenticated_forwarded_owner(&port, owner)
            .await
            .is_none()
    );
    assert_eq!(port.probes.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn only_core_authentication_seals_an_owner_scope() {
    let subject = UserId::new(uuid::Uuid::now_v7());
    let group = OwnerId::from(GroupId::new(uuid::Uuid::now_v7()));
    let context = AuthzContext::for_subject_with_role(
        subject,
        [(group, Role::editor())],
        AuthPath::HostBearer,
    );
    assert!(context.owner_scope().is_none());

    let verified = authenticate(
        &VerifiedAuthenticator { context },
        &Credentials::Bearer("opaque".to_string()),
    )
    .await
    .expect("the trusted authenticator succeeds");
    let scope = verified.owner_scope().expect("authentication seals scope");
    assert!(scope.may_write(&group, MemoryKind::Fact));
    assert!(
        verified
            .clone()
            .with_host_resolved_role(GroupId::new(uuid::Uuid::now_v7()), Role::admin())
            .is_none()
    );
    let narrowed = verified
        .narrowed_to_owner(group)
        .expect("authorized group remains selectable");
    assert_eq!(
        narrowed
            .owner_scope()
            .expect("scope survives")
            .readable_owners(MemoryKind::Fact),
        vec![group]
    );
}

#[test]
fn resolved_subject_context_gets_role_ceilings() {
    let subject = UserId::new(uuid::Uuid::now_v7());
    let group = OwnerId::from(GroupId::new(uuid::Uuid::now_v7()));
    let roles = OwnerRoles::for_subject(subject, [(group, Role::editor())]).unwrap();

    let ctx = AuthzContext::server_resolved(roles, AuthPath::HostBearer);

    assert_eq!(ctx.subject(), Some(subject));
    assert!(ctx.may_administer(&OwnerId::from(subject)));
    assert!(ctx.may_write(&group, MemoryKind::Perspective));
    assert!(ctx.may_work_goals(&group));
    assert!(!ctx.may_administer(&group));
    assert!(!ctx.may_manage(&group));

    let readable = ctx.readable_owners(MemoryKind::Perspective);
    assert_eq!(readable.len(), 2, "own personal owner plus the one group");
    assert!(readable.contains(&OwnerId::from(subject)));
    assert!(readable.contains(&group));
    assert!(ctx.may_read_goals(&OwnerId::from(subject)) && ctx.may_read_goals(&group));
}

/// A bound read set keeps the wider roles it was bound with, and never
/// outlives the context's deadline.
#[test]
fn the_bound_reader_follows_the_contexts_deadline() {
    let subject = UserId::new(uuid::Uuid::now_v7());
    let group = OwnerId::from(GroupId::new(uuid::Uuid::now_v7()));
    let bound = AuthzContext::for_subject_with_role(
        subject,
        [(group, Role::editor())],
        AuthPath::HostBearer,
    )
    .with_expires_at(Some(SystemTime::now() + Duration::from_mins(5)))
    .seal_verified_scope()
    .unwrap()
    .with_read_set(crate::ReadSet::default())
    .unwrap();
    let narrowed = bound.narrowed_to_owner(group).unwrap();

    let reads = narrowed.bound_reads();
    let reader = reads.reader().expect("bound on a sealed context");
    assert!(reader.may_read(&OwnerId::from(subject), MemoryKind::Fact));
    assert!(reader.may_read(&group, MemoryKind::Fact));

    let expired = narrowed.with_expires_at(Some(SystemTime::UNIX_EPOCH));
    let reads = expired.bound_reads();
    let reader = reads.reader().expect("still bound");
    assert!(reader.is_expired());
    assert!(!reader.may_read(&group, MemoryKind::Fact));
}

/// Roles, accessible owners and expiry answer from one state: a
/// narrowed context agrees with its sealed scope on all three, and an
/// expired one answers nothing anywhere.
#[test]
fn roles_accessible_owners_and_expiry_answer_from_one_state() {
    let subject = UserId::new(uuid::Uuid::now_v7());
    let group = OwnerId::from(GroupId::new(uuid::Uuid::now_v7()));
    let personal = OwnerId::from(subject);
    let deadline = SystemTime::now() + Duration::from_mins(5);
    let sealed = AuthzContext::for_subject_with_role(
        subject,
        [(group, Role::editor())],
        AuthPath::HostBearer,
    )
    .with_expires_at(Some(deadline))
    .seal_verified_scope()
    .unwrap();

    let narrowed = sealed.clone().narrowed_to_owner(group).unwrap();
    let scope = narrowed.owner_scope().unwrap();
    assert_eq!(narrowed.expires_at(), Some(deadline));
    assert_eq!(scope.expires_at(), Some(deadline));
    assert_eq!(narrowed.accessible_owners().collect::<Vec<_>>(), [group]);
    assert!(narrowed.can_access_owner(&group) && !narrowed.can_access_owner(&personal));
    assert_eq!(narrowed.role_for_owner(&personal), None);
    assert_eq!(scope.role_for_owner(&personal), None);

    let expired = sealed.with_expires_at(Some(SystemTime::UNIX_EPOCH));
    let scope = expired.owner_scope().unwrap();
    for owner in [&group, &personal] {
        assert!(!expired.may_read(owner, MemoryKind::Fact), "{owner:?}");
        assert!(!expired.may_write(owner, MemoryKind::Fact), "{owner:?}");
        assert!(!expired.can_access_owner(owner), "{owner:?}");
        assert_eq!(expired.role_for_owner(owner), None, "{owner:?}");
        assert!(!scope.may_read(owner, MemoryKind::Fact), "{owner:?}");
    }
    assert!(expired.accessible_owners().next().is_none());
    assert!(expired.clone().narrowed_to_owner(group).is_none());
}

/// The forwarder shape: a subject whose resolved map carries no Group
/// at all is still narrowed to one party, on a role the host resolved
/// for that party alone. The role is folded into the map and the one
/// narrowing path reads it — the same path that refused the owner a
/// moment earlier, when the map did not carry it.
#[test]
fn a_host_resolved_role_narrows_to_exactly_that_owner() {
    let subject = UserId::new(uuid::Uuid::now_v7());
    let group_id = GroupId::new(uuid::Uuid::now_v7());
    let group = OwnerId::from(group_id);
    let roles = OwnerRoles::for_subject(subject, []).unwrap();
    let ctx = AuthzContext::server_resolved(roles, AuthPath::HostBearer);

    assert!(
        ctx.clone().narrowed_to_owner(group).is_none(),
        "the eager map has no role for this group"
    );

    let with_role = ctx
        .with_host_resolved_role(group_id, Role::editor())
        .expect("a host-resolved Group role folds into the map");
    assert!(
        with_role.can_access_owner(&group),
        "the accessible set is recomputed from the extended map"
    );
    let narrowed = with_role
        .narrowed_to_owner(group)
        .expect("the one narrowing path now finds the role");

    assert_eq!(narrowed.subject(), Some(subject));
    assert_eq!(narrowed.auth_path(), AuthPath::HostBearer);
    assert_eq!(narrowed.role_for_owner(&group), Some(Role::editor()));
    assert!(narrowed.may_write(&group, MemoryKind::Perspective));
    assert!(!narrowed.may_administer(&group));
    assert_eq!(
        narrowed.readable_owners(MemoryKind::Perspective),
        vec![group],
        "the narrowed set is exactly the one party"
    );
    assert!(narrowed.can_access_owner(&group));
    assert!(
        !narrowed.can_access_owner(&OwnerId::from(subject)),
        "accessible principals are recomputed from the narrowed roles"
    );
}

/// One request, one party: narrowing again for the next party starts
/// from the authenticated context, and neither narrowing can see the
/// other's owner.
#[test]
fn each_host_resolved_narrowing_sees_only_its_own_party() {
    let subject = UserId::new(uuid::Uuid::now_v7());
    let first_id = GroupId::new(uuid::Uuid::now_v7());
    let second_id = GroupId::new(uuid::Uuid::now_v7());
    let first = OwnerId::from(first_id);
    let second = OwnerId::from(second_id);
    let ctx = AuthzContext::server_resolved(
        OwnerRoles::for_subject(subject, []).unwrap(),
        AuthPath::HostBearer,
    );

    let to_first = ctx
        .clone()
        .with_host_resolved_role(first_id, Role::editor())
        .and_then(|ctx| ctx.narrowed_to_owner(first))
        .expect("first party resolves");
    let to_second = ctx
        .with_host_resolved_role(second_id, Role::viewer())
        .and_then(|ctx| ctx.narrowed_to_owner(second))
        .expect("second party resolves");

    assert!(to_first.can_access_owner(&first) && !to_first.can_access_owner(&second));
    assert!(to_second.can_access_owner(&second) && !to_second.can_access_owner(&first));
    assert!(!to_second.may_write(&second, MemoryKind::Fact));
}

/// Personal owners are a kernel rule, not a membership row: no resolved
/// role can reach the map for one (the fold is typed on `GroupId`), so
/// a stranger's personal owner stays refused and the subject's own
/// still narrows through the one path, to `personal`, whatever Group
/// roles were folded in beside it.
#[test]
fn a_host_resolved_role_never_authorizes_a_foreign_personal_owner() {
    let subject = UserId::new(uuid::Uuid::now_v7());
    let stranger = OwnerId::from(UserId::new(uuid::Uuid::now_v7()));
    let ctx = AuthzContext::server_resolved(
        OwnerRoles::for_subject(subject, []).unwrap(),
        AuthPath::HostBearer,
    )
    .with_host_resolved_role(GroupId::new(uuid::Uuid::now_v7()), Role::admin())
    .expect("a Group role folds");

    assert!(ctx.clone().narrowed_to_owner(stranger).is_none());

    let own = ctx
        .narrowed_to_owner(OwnerId::from(subject))
        .expect("the subject's own personal owner narrows");
    assert_eq!(
        own.role_for_owner(&OwnerId::from(subject)),
        Some(Role::personal())
    );
    assert!(!own.may_manage(&OwnerId::from(subject)));
}

/// A role is folded in before authentication seals the context; a sealed
/// context takes none.
#[test]
fn a_sealed_context_takes_no_host_resolved_role() {
    let group = GroupId::new(uuid::Uuid::now_v7());
    let sealed = crate::test_fixtures::authenticated_context(AuthzContext::for_subject(
        UserId::new(uuid::Uuid::now_v7()),
        AuthPath::HostBearer,
    ));

    assert!(
        sealed
            .with_host_resolved_role(group, Role::admin())
            .is_none()
    );
}

/// A default selects among the owners the context already carries; it
/// can never name one the map lacks, so it cannot grant anything.
#[test]
fn a_default_owner_must_be_one_the_context_carries() {
    let subject = UserId::new(uuid::Uuid::now_v7());
    let tenant = OwnerId::from(GroupId::new(uuid::Uuid::now_v7()));
    let elsewhere = OwnerId::from(GroupId::new(uuid::Uuid::now_v7()));
    let roles = OwnerRoles::for_subject(subject, [(tenant, Role::editor())]).unwrap();
    let ctx = AuthzContext::server_resolved(roles, AuthPath::HostBearer);
    assert_eq!(ctx.default_owner(), None);

    for carried in [tenant, OwnerId::from(subject)] {
        let named = ctx.clone().with_default_owner(carried).expect("carried");
        assert_eq!(named.default_owner(), Some(carried));
    }
    assert!(ctx.clone().with_default_owner(elsewhere).is_none());
    assert!(
        ctx.with_default_owner(OwnerId::from(UserId::new(uuid::Uuid::now_v7())))
            .is_none(),
        "another subject's personal owner is never carried"
    );
}

/// A narrowed context is bound to one owner; a default left on it would
/// name an owner the context can no longer act for.
#[test]
fn narrowing_drops_the_default() {
    let subject = UserId::new(uuid::Uuid::now_v7());
    let tenant = OwnerId::from(GroupId::new(uuid::Uuid::now_v7()));
    let roles = OwnerRoles::for_subject(subject, [(tenant, Role::editor())]).unwrap();
    let ctx = AuthzContext::server_resolved(roles, AuthPath::HostBearer)
        .with_default_owner(tenant)
        .expect("carried");

    let narrowed = ctx
        .narrowed_to_owner(OwnerId::from(subject))
        .expect("own personal owner");
    assert_eq!(narrowed.default_owner(), None);
}

#[test]
fn contexts_carry_no_verified_claims_by_default() {
    let context = AuthzContext::single_owner(&owner(), AuthPath::HostBearer);
    assert_eq!(context.verified_claims(), None);
}

#[test]
fn verified_claims_survive_cloning_and_owner_narrowing() {
    let subject = UserId::new(uuid::Uuid::now_v7());
    let group = OwnerId::from(GroupId::new(uuid::Uuid::now_v7()));
    let claims = claims();
    let context = AuthzContext::for_subject_with_role(
        subject,
        [(group, Role::editor())],
        AuthPath::HostBearer,
    )
    .with_verified_claims(claims.clone());
    assert_eq!(context.verified_claims(), Some(&claims));
    assert_eq!(context.clone().verified_claims(), Some(&claims));

    for owner in [group, OwnerId::from(subject)] {
        let narrowed = context
            .clone()
            .narrowed_to_owner(owner)
            .expect("owner role");
        assert_eq!(narrowed.verified_claims(), Some(&claims));
    }
}

#[test]
fn verified_claim_values_are_redacted_from_context_debug() {
    let context =
        AuthzContext::single_owner(&owner(), AuthPath::HostBearer).with_verified_claims(claims());
    let debug = format!("{context:?}");
    assert!(!debug.contains("claim-value"));
    assert!(!debug.contains("https://issuer.example"));
}

/// `trusted_model_id` is identity, not a capability: it must survive
/// every narrowing the edge performs between authentication and the
/// tool call, exactly like `subject`.
#[test]
fn a_trusted_model_id_survives_owner_narrowing_and_revalidation() {
    let subject = UserId::new(uuid::Uuid::now_v7());
    let group = OwnerId::from(GroupId::new(uuid::Uuid::now_v7()));
    let roles = OwnerRoles::for_subject(subject, [(group, Role::editor())]).unwrap();

    let ctx = AuthzContext::server_resolved(roles, AuthPath::HostBearer)
        .with_trusted_model_id("runner/pinned")
        .expect("a well-formed runner id binds");

    assert_eq!(ctx.trusted_model_id(), Some("runner/pinned"));
    assert_eq!(
        ctx.identity.trusted_model_id.as_deref(),
        Some("runner/pinned"),
        "a revalidated stream keeps the bound model identity"
    );

    let narrowed = ctx
        .clone()
        .narrowed_to_owner(group)
        .expect("editor narrows to the group owner");
    assert_eq!(narrowed.trusted_model_id(), Some("runner/pinned"));
    assert_eq!(narrowed.subject(), Some(subject));

    let personal = ctx
        .narrowed_to_owner(OwnerId::from(subject))
        .expect("own personal owner narrows");
    assert_eq!(personal.trusted_model_id(), Some("runner/pinned"));
}

/// Nothing binds a model identity unless an authenticator says so: the
/// test constructors and the fail-closed context all leave it absent.
#[test]
fn contexts_carry_no_trusted_model_id_by_default() {
    let owner = owner();
    assert_eq!(
        AuthzContext::single_owner(&owner, AuthPath::HostBearer).trusted_model_id(),
        None
    );
    assert_eq!(
        AuthzContext::for_subject(UserId::new(uuid::Uuid::now_v7()), AuthPath::HostBearer)
            .trusted_model_id(),
        None
    );
}

/// The bound is applied where the value is bound, not where it is
/// later used: an authenticator that binds an unusable id learns at the
/// boundary that produced it, not on some later write.
#[test]
fn a_bound_trusted_model_id_is_trimmed_and_bounded() {
    let owner = owner();
    let base = || AuthzContext::single_owner(&owner, AuthPath::HostBearer);

    assert_eq!(
        base()
            .with_trusted_model_id("  runner/pinned  ")
            .expect("trims")
            .trusted_model_id(),
        Some("runner/pinned"),
        "one trim, at the boundary, so every later comparison is on the same string"
    );

    for blank in ["", "   "] {
        assert_eq!(
            base().with_trusted_model_id(blank).unwrap_err(),
            TrustedModelIdError::Blank
        );
    }

    assert!(
        base()
            .with_trusted_model_id("m".repeat(MAX_OPERATOR_LABEL_CHARS))
            .is_ok(),
        "the bound itself fits"
    );
    assert_eq!(
        base()
            .with_trusted_model_id("m".repeat(MAX_OPERATOR_LABEL_CHARS + 1))
            .unwrap_err(),
        TrustedModelIdError::TooLong {
            chars: MAX_OPERATOR_LABEL_CHARS + 1,
            max: MAX_OPERATOR_LABEL_CHARS,
        }
    );
}

/// There is no way to spell "clear it": the builder takes a `String`,
/// so no later step in a chain can drop provenance an authenticator
/// established.
#[test]
fn a_bound_trusted_model_id_cannot_be_cleared_by_a_later_builder_step() {
    let owner = owner();
    let ctx = AuthzContext::single_owner(&owner, AuthPath::HostBearer)
        .with_trusted_model_id("runner/pinned")
        .expect("binds")
        .with_tool_scope(ToolScope::Palette(Vec::new()))
        .with_expires_at(Some(SystemTime::UNIX_EPOCH));

    assert_eq!(ctx.trusted_model_id(), Some("runner/pinned"));
}

fn key(text: &str) -> ScopeKey {
    ScopeKey::parse(text).expect("a scope key")
}

fn tool(name: &str) -> ToolName {
    ToolName::parse(name).expect("a tool name")
}

fn action(name: &str) -> ActionName {
    ActionName::parse(name).expect("an action name")
}

fn palette(ids: &[&str]) -> ToolScope {
    ToolScope::Palette(ids.iter().map(|id| key(id)).collect())
}

#[test]
fn palette_scope_allows_and_denies() {
    let scope = palette(&[protocol_resource::MEMORY]);

    assert!(scope.allows(&key(protocol_resource::MEMORY)));
    assert!(!scope.allows(&key(protocol_action::CORE_GOAL_SET)));
}

#[test]
fn deployment_admits_declared_reads_but_callers_require_exact_keys() {
    let empty = ServedSurface::Palette(Vec::new());
    let search_key = key(protocol_tool::CORE_SEARCH_MEMORIES);
    let read = KeyDeclaration::shared(Some(ToolEffect::ReadOnly));
    let owner_read = KeyDeclaration {
        audience: ToolAudience::Owner,
        ..read
    };
    assert!(empty.admits(&search_key, read));
    assert!(empty.admits(&search_key, owner_read));
    assert!(!empty.admits(&search_key, KeyDeclaration::UNDECLARED));
    assert!(!empty.admits(
        &search_key,
        KeyDeclaration::shared(Some(ToolEffect::Additive(Replay::Idempotent)))
    ));
    let holding = ServedSurface::Palette(vec![search_key.clone()]);
    assert!(holding.admits(&search_key, KeyDeclaration::UNDECLARED));
    assert!(holding.admits(&search_key, owner_read));
    let mut capabilities = CapabilitySet {
        tool_scope: palette(&[]),
        served_surface: empty,
    };
    for declaration in [read, owner_read, KeyDeclaration::UNDECLARED] {
        assert!(!capabilities.admits(&search_key, declaration));
    }
    capabilities.tool_scope = palette(&[
        protocol_tool::CORE_SEARCH_MEMORIES,
        protocol_resource::MEMORY,
    ]);
    assert!(capabilities.admits(&search_key, read));
    assert!(capabilities.admits(&search_key, owner_read));
    assert!(!capabilities.admits(&search_key, KeyDeclaration::UNDECLARED));
    assert!(capabilities.admits(&key(protocol_resource::MEMORY), read));
    assert!(!capabilities.admits(&key(protocol_resource::GRAPH), read));
}

#[test]
fn deployment_restrictions_survive_narrowing_and_cannot_be_reopened() {
    let owner = OwnerId::from(UserId::new(uuid::Uuid::now_v7()));
    let surface = ServedSurface::Palette(vec![key(protocol_tool::CORE_REMEMBER)]);
    let context = AuthzContext::single_owner(&owner, AuthPath::HostBearer)
        .with_tool_scope(palette(&[protocol_tool::CORE_SEARCH_MEMORIES]))
        .with_served_surface(surface.clone())
        .with_served_surface(ServedSurface::All);
    assert_eq!(context.served_surface(), &surface);
    assert_eq!(
        context.capabilities().tool_scope,
        palette(&[protocol_tool::CORE_SEARCH_MEMORIES])
    );
    let narrowed = context
        .clone()
        .narrowed_to_owner(owner)
        .expect("the authenticated owner");
    assert_eq!(narrowed.capabilities(), context.capabilities());
    let disjoint = narrowed.with_served_surface(ServedSurface::Palette(vec![key(
        protocol_tool::CORE_FORGET,
    )]));
    assert_eq!(
        disjoint.served_surface(),
        &ServedSurface::Palette(Vec::new())
    );
    assert!(disjoint.capabilities().admits(
        &key(protocol_tool::CORE_SEARCH_MEMORIES),
        KeyDeclaration::shared(Some(ToolEffect::ReadOnly))
    ));
}

#[test]
fn allows_action_requires_leaf_scope_key() {
    let scope = palette(&[protocol_action::CORE_GOAL_SET]);

    assert!(scope.allows_action(&tool(protocol_tool::CORE_GOAL), &action("set")));
    assert!(!scope.allows_action(&tool(protocol_tool::CORE_GOAL), &action("transition")));
    assert!(!scope.allows(&key(protocol_tool::CORE_GOAL)));
}

#[test]
fn allows_group_advertisement_requires_a_leaf_key() {
    let leaf = palette(&[protocol_action::CORE_GOAL_SET]);
    let flat = palette(&[protocol_tool::CORE_GOAL]);
    let unrelated = palette(&[protocol_resource::MEMORY]);
    let goal = tool(protocol_tool::CORE_GOAL);

    assert!(ToolScope::All.allows_group_advertisement(&goal));
    assert!(leaf.allows_group_advertisement(&goal));
    assert!(!flat.allows_group_advertisement(&goal));
    assert!(!unrelated.allows_group_advertisement(&goal));
}

#[test]
fn flat_tool_advertisement_requires_the_exact_tool_id() {
    let exact = palette(&[protocol_tool::CORE_SEARCH_MEMORIES]);
    let bogus_leaf = palette(&[&format!("{}:unknown", protocol_tool::CORE_SEARCH_MEMORIES)]);
    let dispatcher_leaf = palette(&[protocol_action::CORE_GOAL_SET]);
    let search = tool(protocol_tool::CORE_SEARCH_MEMORIES);

    assert!(exact.allows_tool_advertisement(&search, false));
    assert!(!bogus_leaf.allows_tool_advertisement(&search, false));
    assert!(dispatcher_leaf.allows_tool_advertisement(&tool(protocol_tool::CORE_GOAL), true));
}

#[test]
fn tool_scope_intersect_only_narrows_never_widens() {
    let mem = palette(&[
        protocol_resource::MEMORY,
        protocol_tool::CORE_SEARCH_MEMORIES,
    ]);

    // `All` is the identity element in both positions.
    assert_eq!(ToolScope::All.intersect(&mem), mem);
    assert_eq!(mem.intersect(&ToolScope::All), mem);
    assert_eq!(ToolScope::All.intersect(&ToolScope::All), ToolScope::All);

    // One caller restriction cannot re-add keys another omitted.
    let caller = palette(&[protocol_resource::MEMORY, protocol_action::CORE_GOAL_SET]);
    let result = mem.intersect(&caller);
    assert!(result.allows(&key(protocol_resource::MEMORY)));
    assert!(!result.allows(&key(protocol_tool::CORE_SEARCH_MEMORIES))); // caller lacked it
    assert!(!result.allows(&key(protocol_action::CORE_GOAL_SET))); // the other restriction lacked it

    // Disjoint palettes intersect to empty (deny-all), never widening.
    assert_eq!(
        palette(&["a"]).intersect(&palette(&["b"])),
        ToolScope::Palette(Vec::new())
    );
}

/// Intersection is key equality: a bare dispatcher name holds none of its
/// leaves, so neither palette narrows the other through subsumption. The
/// order of the receiver is kept.
#[test]
fn intersect_is_key_equality_without_subsumption() {
    let bare = palette(&[protocol_tool::CORE_GOAL]);
    let leaves = palette(&[protocol_action::CORE_GOAL_SET]);

    assert_eq!(bare.intersect(&leaves), ToolScope::Palette(Vec::new()));
    assert_eq!(leaves.intersect(&bare), ToolScope::Palette(Vec::new()));

    let wide = palette(&["c", "a", "b"]);
    let narrow = palette(&["b", "c"]);
    assert_eq!(wide.intersect(&narrow), palette(&["c", "b"]));
}

/// Invocation, advertisement and delegation all require the same leaf.
#[test]
fn caller_invocation_advertisement_and_delegation_require_the_same_key() {
    // The questions, in the column order of the table below.
    const COLUMNS: [&str; 5] = [
        // `allows_action(core_goal, set)`: the call gate.
        "call",
        // `advertises_action(core_goal, set)`: `tools/list`.
        "advertise",
        // `covers_command(core_goal, Some(set))`: delegation of a leaf.
        "delegate a leaf",
        // `covers_command(core_search_memories, None)`: delegation of a
        // flat tool.
        "delegate a flat tool",
        // `allows_group_advertisement(core_goal)`: the dispatcher is
        // listed at all.
        "group",
    ];
    let goal = tool(protocol_tool::CORE_GOAL);
    let set = action("set");
    let search = tool(protocol_tool::CORE_SEARCH_MEMORIES);

    let table = [
        (
            "leaf",
            palette(&[protocol_action::CORE_GOAL_SET]),
            [true, true, true, false, true],
        ),
        (
            "bare dispatcher",
            palette(&[protocol_tool::CORE_GOAL]),
            [false; 5],
        ),
        (
            "flat tool",
            palette(&[protocol_tool::CORE_SEARCH_MEMORIES]),
            [false, false, false, true, false],
        ),
        (
            "resource key",
            palette(&[protocol_resource::MEMORY]),
            [false; 5],
        ),
        ("empty", palette(&[]), [false; 5]),
        ("all", ToolScope::All, [true; 5]),
    ];
    for (name, scope, expected) in &table {
        let actual = [
            scope.allows_action(&goal, &set),
            scope.advertises_action(&goal, &set),
            scope.covers_command(&goal, Some(&set)),
            scope.covers_command(&search, None),
            scope.allows_group_advertisement(&goal),
        ];
        for ((column, actual), expected) in COLUMNS.iter().zip(actual).zip(expected) {
            assert_eq!(actual, *expected, "{name}: {column}");
        }
    }
}

/// A resource named like a tool is still a resource: `resource:core_goal`
/// holds neither the tool `core_goal`, nor one of its leaves, nor a
/// dispatcher called `resource`.
#[test]
fn a_resource_key_is_never_a_tool_or_a_leaf() {
    let scope = palette(&["resource:core_goal"]);

    assert!(!scope.allows_group_advertisement(&tool(protocol_tool::CORE_GOAL)));
    assert!(!scope.allows(&key(protocol_tool::CORE_GOAL)));
    assert!(!scope.allows_group_advertisement(&tool("resource")));
    assert!(!scope.allows_action(&tool("resource"), &action("core_goal")));
    assert!(!scope.covers_command(&tool("resource"), Some(&action("core_goal"))));
}

/// An independent string model of exact caller keys, used by
/// [`typed_matching_equals_the_string_matchers`].
mod string_matchers {
    pub(super) fn allows(palette: &[&str], name: &str) -> bool {
        palette.contains(&name)
    }

    pub(super) fn allows_action(palette: &[&str], tool: &str, action: &str) -> bool {
        allows(palette, &format!("{tool}:{action}"))
    }

    pub(super) fn allows_group_advertisement(palette: &[&str], tool: &str) -> bool {
        let action_prefix = format!("{tool}:");
        palette
            .iter()
            .any(|entry| entry.starts_with(&action_prefix))
    }

    pub(super) fn allows_tool_advertisement(
        palette: &[&str],
        tool: &str,
        has_actions: bool,
    ) -> bool {
        if has_actions {
            allows_group_advertisement(palette, tool)
        } else {
            allows(palette, tool)
        }
    }

    /// `scope_permits_action`, the listing predicate.
    pub(super) fn advertises_action(palette: &[&str], tool: &str, action: &str) -> bool {
        allows_action(palette, tool, action)
    }

    /// `DelegatedAuthorityService::validate_scopes`' closure.
    pub(super) fn covers_command(palette: &[&str], tool: &str, action: Option<&str>) -> bool {
        match action {
            Some(action) => allows_action(palette, tool, action),
            None => allows(palette, tool),
        }
    }

    pub(super) fn intersect<'a>(a: &[&'a str], b: &[&str]) -> Vec<&'a str> {
        a.iter().copied().filter(|id| b.contains(id)).collect()
    }
}

/// Every palette over a vocabulary of canonical keys, asked every
/// question both ways: the typed answer equals the string answer. The
/// vocabulary mixes dispatcher, flat and resource spellings, including a
/// resource named like a dispatcher and a flat tool named `resource`.
/// Dispatcher queries leave out the name `resource`: it is the one name
/// whose leaf would spell a resource key, and the registry refuses a
/// dispatcher with it.
#[test]
fn typed_matching_equals_the_string_matchers() {
    const VOCABULARY: [&str; 11] = [
        "core_goal",
        "core_goal:set",
        "core_goal:transition",
        "other:set",
        "core_search_memories",
        "core_search_memories:unknown",
        "flat",
        "flat:unknown",
        "resource",
        "resource:memory",
        "resource:core_goal",
    ];
    const DISPATCHERS: [&str; 3] = ["core_goal", "other", "flat"];
    const ACTIONS: [&str; 3] = ["set", "transition", "unknown"];
    const FLAT: [&str; 4] = ["core_search_memories", "flat", "resource", "core_goal"];

    let from_mask = |mask: u32| -> Vec<&str> {
        VOCABULARY
            .iter()
            .enumerate()
            .filter(|(index, _)| mask & (1 << index) != 0)
            .map(|(_, id)| *id)
            .collect()
    };
    let typed = |ids: &[&str]| palette(ids);

    for mask in 0..(1u32 << VOCABULARY.len()) {
        let strings = from_mask(mask);
        let scope = typed(&strings);

        for name in VOCABULARY {
            assert_eq!(
                scope.allows(&key(name)),
                string_matchers::allows(&strings, name),
                "allows {name} in {strings:?}"
            );
        }
        for dispatcher in DISPATCHERS {
            let name = tool(dispatcher);
            assert_eq!(
                scope.allows_group_advertisement(&name),
                string_matchers::allows_group_advertisement(&strings, dispatcher),
                "group {dispatcher} in {strings:?}"
            );
            for has_actions in [true, false] {
                assert_eq!(
                    scope.allows_tool_advertisement(&name, has_actions),
                    string_matchers::allows_tool_advertisement(&strings, dispatcher, has_actions),
                    "tool advertisement {dispatcher} ({has_actions}) in {strings:?}"
                );
            }
            for leaf in ACTIONS {
                let act = action(leaf);
                assert_eq!(
                    scope.allows_action(&name, &act),
                    string_matchers::allows_action(&strings, dispatcher, leaf),
                    "call {dispatcher}:{leaf} in {strings:?}"
                );
                assert_eq!(
                    scope.advertises_action(&name, &act),
                    string_matchers::advertises_action(&strings, dispatcher, leaf),
                    "advertise {dispatcher}:{leaf} in {strings:?}"
                );
                assert_eq!(
                    scope.covers_command(&name, Some(&act)),
                    string_matchers::covers_command(&strings, dispatcher, Some(leaf)),
                    "delegate {dispatcher}:{leaf} in {strings:?}"
                );
            }
        }
        for flat in FLAT {
            assert_eq!(
                scope.covers_command(&tool(flat), None),
                string_matchers::covers_command(&strings, flat, None),
                "delegate {flat} in {strings:?}"
            );
        }
    }
}

/// `intersect` over every pair of palettes drawn from six keys keeps the
/// entries, and their order, that the string version kept.
#[test]
fn typed_intersect_equals_the_string_intersect() {
    const VOCABULARY: [&str; 6] = [
        "core_goal",
        "core_goal:set",
        "flat",
        "resource:memory",
        "other:set",
        "resource",
    ];
    let from_mask = |mask: u32| -> Vec<&str> {
        VOCABULARY
            .iter()
            .enumerate()
            .filter(|(index, _)| mask & (1 << index) != 0)
            .map(|(_, id)| *id)
            .collect()
    };
    for left in 0..(1u32 << VOCABULARY.len()) {
        for right in 0..(1u32 << VOCABULARY.len()) {
            let (left, right) = (from_mask(left), from_mask(right));
            assert_eq!(
                palette(&left).intersect(&palette(&right)),
                palette(&string_matchers::intersect(&left, &right)),
                "{left:?} and {right:?}"
            );
        }
    }
}

/// A request name that is no key is held by no palette, as no string in
/// a canonical palette equals it; `All` still allows it, at the gate.
#[test]
fn a_name_that_is_no_key_is_in_no_palette() {
    for text in [
        "",
        ":",
        "a:b:c",
        "a b",
        "core_goal:",
        ":set",
        "resource:",
        "a/b",
    ] {
        assert!(ScopeKey::parse(text).is_err(), "{text:?}");
        assert!(
            !string_matchers::allows(&["core_goal", "core_goal:set", "resource:memory"], text),
            "{text:?}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn expired_identity_terminates_at_deadline_but_non_expired_keeps_flowing() {
    let config = RevalidationConfig {
        max_stream_lifetime: Duration::from_mins(1),
    };
    let now = SystemTime::now();
    let (expired_sender, expired_receiver) = mpsc::unbounded_channel();
    let (live_sender, live_receiver) = mpsc::unbounded_channel();
    let mut expired_stream = revalidate_stream(
        receiver_stream(expired_receiver),
        Some(now + Duration::from_secs(10)),
        config,
    );
    let mut live_stream = revalidate_stream(
        receiver_stream(live_receiver),
        Some(now + Duration::from_mins(1)),
        config,
    );

    expired_sender.send(1).expect("receiver is alive");
    live_sender.send(10).expect("receiver is alive");
    assert_eq!(expired_stream.next().await, Some(1));
    assert_eq!(live_stream.next().await, Some(10));

    time::advance(Duration::from_secs(10)).await;
    live_sender.send(11).expect("receiver is alive");

    assert_eq!(expired_stream.next().await, None);
    assert_eq!(live_stream.next().await, Some(11));
}

#[tokio::test(start_paused = true)]
async fn max_lifetime_terminates_otherwise_valid_stream() {
    let config = RevalidationConfig {
        max_stream_lifetime: Duration::from_secs(10),
    };
    let (_sender, receiver) = mpsc::unbounded_channel::<u8>();
    let mut stream = revalidate_stream(receiver_stream(receiver), None, config);

    time::advance(Duration::from_secs(10)).await;

    assert_eq!(stream.next().await, None);
}

#[tokio::test(start_paused = true)]
async fn none_expiry_runs_until_max_lifetime() {
    let config = RevalidationConfig {
        max_stream_lifetime: Duration::from_secs(30),
    };
    let (sender, receiver) = mpsc::unbounded_channel();
    let mut stream = revalidate_stream(receiver_stream(receiver), None, config);

    time::advance(Duration::from_secs(20)).await;
    sender.send(1).expect("receiver is alive");
    assert_eq!(stream.next().await, Some(1));

    time::advance(Duration::from_secs(10)).await;
    assert_eq!(stream.next().await, None);
}

#[tokio::test(start_paused = true)]
async fn queued_items_before_termination_are_delivered() {
    let config = RevalidationConfig {
        max_stream_lifetime: Duration::from_secs(10),
    };
    let (sender, receiver) = mpsc::unbounded_channel();
    let mut stream = revalidate_stream(receiver_stream(receiver), None, config);

    sender.send(1).expect("receiver is alive");
    sender.send(2).expect("receiver is alive");
    sender.send(3).expect("receiver is alive");
    time::advance(Duration::from_secs(10)).await;

    assert_eq!(stream.next().await, Some(1));
    assert_eq!(stream.next().await, Some(2));
    assert_eq!(stream.next().await, Some(3));
    assert_eq!(stream.next().await, None);
}

#[test]
fn bound_publication_extensions_merge_and_never_overwrite() {
    let ctx = AuthzContext::for_subject(UserId::new(uuid::Uuid::now_v7()), AuthPath::HostBearer);
    assert!(ctx.publication_extensions().is_empty());

    let ctx = ctx
        .with_publication_extensions(
            PublicationExtensions::new()
                .with("workflowid", "wf-1")
                .expect("a workflow id is a legal attribute"),
        )
        .expect("the first binding takes");
    let ctx = ctx
        .with_publication_extensions(
            PublicationExtensions::new()
                .with("runid", "run-3")
                .expect("a run id is a legal attribute"),
        )
        .expect("a second binding merges");
    assert_eq!(
        ctx.publication_extensions()
            .iter()
            .map(|(name, _)| name)
            .collect::<Vec<_>>(),
        vec!["runid", "workflowid"]
    );

    let err = ctx
        .clone()
        .with_publication_extensions(
            PublicationExtensions::new()
                .with("runid", "run-4")
                .expect("bind"),
        )
        .expect_err("nothing may overwrite a bound attribute");
    assert!(matches!(
        err,
        PublicationExtensionsError::DuplicateName { .. }
    ));

    // Narrowing carries them; the identity handed to a revalidating
    // stream does not — they are run context, not identity material.
    let narrowed = ctx
        .clone()
        .narrowed_to_owner(OwnerId::from(ctx.identity.subject.expect("a subject")))
        .expect("a subject may narrow to itself");
    assert_eq!(narrowed.publication_extensions().len(), 2);
}

#[tokio::test]
async fn goal_principal_is_only_direct_unconfined_bearer() {
    let subject = UserId::new(uuid::Uuid::now_v7());
    let direct = authenticate(
        &VerifiedAuthenticator {
            context: AuthzContext::for_subject(subject, AuthPath::HostBearer),
        },
        &Credentials::Bearer("verified".into()),
    )
    .await
    .expect("direct bearer");
    assert!(
        direct
            .owner_scope()
            .expect("sealed")
            .goal_principal()
            .is_some()
    );
    assert!(
        direct
            .clone()
            .narrowed_to_owner(OwnerId::from(subject))
            .expect("narrow")
            .owner_scope()
            .expect("sealed")
            .goal_principal()
            .is_none()
    );
    let authority = SystemAuthority::new(SystemAuthorityBinding::fresh());
    let system = AuthzContext::for_system(
        &authority,
        OwnerRoles::for_subject(subject, []).expect("roles"),
    );
    assert!(
        system
            .owner_scope()
            .expect("sealed")
            .goal_principal()
            .is_none()
    );
    let insecure = AuthzContext::for_insecure_single_owner(&authority, OwnerId::from(subject));
    assert!(
        insecure
            .owner_scope()
            .expect("sealed")
            .goal_principal()
            .is_none()
    );
}
