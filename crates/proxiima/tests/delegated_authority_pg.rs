//! A delegated grant issued through the service the runtime composes is
//! stored by `PgDelegationStore` and redeemed from the stored row.

#[path = "fixtures/example_engine.rs"]
#[allow(dead_code, reason = "each suite uses part of the shared fixture")]
mod example_engine;

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use example_engine::{expect_described, fixture_pool, migrated_db};
use proxiima::SpiiceBundle as _;
use proxiima::spiice::SpiiceRegistry;
use proxiima_core::test_fixtures::authenticated_context;
use proxiima_core::{
    AccessCeiling, AuthPath, AuthzContext, DelegatedAuthorityService, DelegatedCommand, Engine,
    GoalAccess, GroupId, OwnerId, Relation, Role, ServedSurface, StoragePorts, UserId,
};
use proxiima_example::ExampleSpiice;
use proxiima_example::contract::LIST_NOTEBOOKS_TOOL;
use proxiima_storage_pg::test_fixtures::{add_member, create_group, create_user};
use proxiima_storage_pg::{PgDelegationStore, PgOwnerAccessResolver, PgPlatformScope};
use time::OffsetDateTime;
use uuid::Uuid;

#[tokio::test]
async fn an_issued_grant_is_stored_as_issued_and_redeems() {
    let db = migrated_db("proxiima_delegated_authority").await;
    let admin = expect_described("admin connection", fixture_pool(&db.admin_url(), 2).await).await;
    let platform = expect_described(
        "platform validation",
        PgPlatformScope::new(
            expect_described(
                "platform connection",
                fixture_pool(db.platform_url(), 2).await,
            )
            .await,
            &["proxiima_core", "proxiima_example"],
        )
        .await,
    )
    .await;

    let subject = UserId::new(Uuid::now_v7());
    let group = GroupId::new(Uuid::now_v7());
    let owner = OwnerId::from(group);
    create_user(&admin, subject).await.expect("user");
    create_group(&admin, group).await.expect("group");
    add_member(&admin, group, subject, Relation::Editor)
        .await
        .expect("editor membership");

    let mut registry = SpiiceRegistry::new();
    ExampleSpiice::register(&mut registry).expect("the example spiice registers");
    let (engine, _system, delegation_runtime) = Engine::new(
        registry.freeze_or_panic_for_tests(),
        StoragePorts::rejecting_for_tests(),
    )
    .into_runtime_authorities();
    let registry = Arc::new(engine.registry().clone());
    let service = DelegatedAuthorityService::new(
        Arc::new(PgDelegationStore::new(platform.clone())),
        Arc::new(PgOwnerAccessResolver::new(admin.clone(), platform)),
        registry.clone(),
        ServedSurface::All,
        &delegation_runtime,
    );

    // Whole seconds, so the stored timestamp compares exactly.
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("after the epoch");
    let bearer_expiry = SystemTime::UNIX_EPOCH + Duration::from_secs(now.as_secs() + 600);
    let source = authenticated_context(
        AuthzContext::for_subject_with_role(
            subject,
            [(owner, Role::editor())],
            AuthPath::HostBearer,
        )
        .with_expires_at(Some(bearer_expiry)),
    );
    let command = DelegatedCommand::parse(LIST_NOTEBOOKS_TOOL, &registry).expect("a flat tool");
    let ceiling = Role::new(AccessCeiling::Abstraction, AccessCeiling::Fact)
        .expect("write within read")
        .with_goals(GoalAccess::Reads);

    let issued = service
        .issue(&source, owner, command.clone(), ceiling)
        .await
        .expect("the grant is issued and stored");
    assert_eq!(issued.expires_at, bearer_expiry);

    let (stored_subject, stored_owner, tool, action, read, write, goals, administers, expires_at) =
        sqlx::query_as::<
            _,
            (
                Uuid,
                Uuid,
                String,
                Option<String>,
                AccessCeiling,
                AccessCeiling,
                GoalAccess,
                bool,
                OffsetDateTime,
            ),
        >(
            "SELECT subject_user_id, owner_id, tool_name, action_name, read_ceiling,
                    write_ceiling, goal_access, administers, expires_at
               FROM proxiima_core.delegated_authority_grants
              WHERE delegation_id = $1",
        )
        .bind(issued.id.into_uuid())
        .fetch_one(&admin)
        .await
        .expect("the issued grant is stored");
    assert_eq!(UserId::new(stored_subject), subject);
    assert_eq!(OwnerId::new(stored_owner), owner);
    assert_eq!(tool, LIST_NOTEBOOKS_TOOL);
    assert_eq!(action, None);
    let stored_role = Role::new(read, write)
        .expect("stored write within read")
        .with_goals(goals);
    assert!(!administers);
    assert_eq!(stored_role, ceiling);
    assert_eq!(SystemTime::from(expires_at), bearer_expiry);

    let _phase = service
        .redeem_phase(issued.id, owner, &command)
        .await
        .expect("the stored grant redeems for its owner and command");
}
