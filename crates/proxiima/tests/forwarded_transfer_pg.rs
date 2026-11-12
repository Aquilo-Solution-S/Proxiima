//! With the same stored Admin row on the destination group, the forwarder's
//! ordinary context transfers an entity into it, while its forwarded context,
//! resolved through the host's forwarder policy, is refused and leaves the
//! entity with its owner. The refusal comes at the source gate (a forwarded
//! role never administers); the destination rule has its own unit test.

#[path = "fixtures/example_engine.rs"]
#[allow(dead_code, reason = "each suite uses part of the shared fixture")]
mod example_engine;

use std::time::{Duration, SystemTime};

use example_engine::{ExampleEngine, storage_over};
use proxiima::ForwarderPolicy;
use proxiima::SpiiceBundle as _;
use proxiima::spiice::SpiiceRegistry;
use proxiima_core::error::ErrorCode;
use proxiima_core::test_fixtures::authenticated_context;
use proxiima_core::{
    AuthPath, AuthzContext, EntityId, ForwarderRole, GroupId, OwnerId, Relation, UserId,
    WriterAttribution,
};
use proxiima_example::ExampleSpiice;
use proxiima_storage_pg::PgOwnerAccessResolver;
use proxiima_storage_pg::test_fixtures::{
    TestMemory, add_member, create_group, create_user, memory_owner,
};
use std::sync::Arc;
use uuid::Uuid;

#[tokio::test]
async fn a_forwarded_context_cannot_borrow_the_destination_membership() {
    let h = ExampleEngine::new("proxiima_forwarded_transfer", |_| {}).await;
    let mut registry = SpiiceRegistry::new();
    ExampleSpiice::register(&mut registry).expect("the example spiice registers");
    let registry = registry.freeze_or_panic_for_tests();
    let pg = storage_over(h.db.runtime_url(), h.db.platform_url(), &registry).await;

    let forwarder = UserId::new(Uuid::now_v7());
    let source = OwnerId::from(forwarder);
    let destination = GroupId::new(Uuid::now_v7());
    create_user(&h.pool, forwarder).await.expect("user");
    create_group(&h.pool, destination).await.expect("group");
    add_member(&h.pool, destination, forwarder, Relation::Admin)
        .await
        .expect("stored Admin row");

    let resolver = Arc::new(PgOwnerAccessResolver::new(
        h.pool.clone(),
        pg.platform_scope_for_host(),
    ));
    assert!(resolver.is_registered_owner(source).await.expect("probe"));
    let port = ForwarderPolicy::new([forwarder], ForwarderRole::Editor)
        .expect("policy")
        .wrap(resolver.clone(), resolver);
    let forwarded_ctx = authenticated_context(
        AuthzContext::from_verified_forwarded_owner(
            forwarder,
            source,
            WriterAttribution {
                by: Uuid::now_v7(),
                via: None,
            },
        )
        .with_expires_at(Some(SystemTime::now() + Duration::from_mins(1))),
    )
    .with_authenticated_forwarded_owner(port.as_ref(), source)
    .await
    .expect("the forwarder policy resolves the verified owner");
    let ordinary =
        authenticated_context(AuthzContext::for_subject(forwarder, AuthPath::HostBearer));

    let admitted = TestMemory::fact()
        .create(&pg, source)
        .await
        .expect("memory")
        .memory_id;
    h.engine
        .transfer_to_owner(&ordinary, EntityId::Memory(admitted), destination.into())
        .await
        .expect("ordinary membership supplies receiving-side consent");
    assert_eq!(
        memory_owner(&h.pool, admitted).await.expect("owner"),
        destination.into()
    );

    let kept = TestMemory::fact()
        .create(&pg, source)
        .await
        .expect("memory")
        .memory_id;
    let refusal = h
        .engine
        .transfer_to_owner(&forwarded_ctx, EntityId::Memory(kept), destination.into())
        .await
        .expect_err("a forwarded context cannot borrow ordinary membership");
    assert_eq!(refusal.code, ErrorCode::Forbidden, "{}", refusal.message);
    assert_eq!(memory_owner(&h.pool, kept).await.expect("owner"), source);
}
