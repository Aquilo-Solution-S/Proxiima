//! A refused membership write reaches the client as the caller's mistake.

use proxiima::{CoreMcpErrorKind, Proxiima, ServedSurface};
use proxiima_core::{GroupId, OwnerId, Relation, Role, UserId};
use uuid::Uuid;

use super::{
    AgentMemoryApp, PgStorage, call_test_model_tool, clone_split_core_db, seed_group_membership,
    space_authz,
};

#[tokio::test]
async fn a_manager_adding_a_group_to_itself_is_refused_as_invalid_input() {
    let split_db = clone_split_core_db("proxiima_membership_refusal")
        .await
        .expect("PG required for tests");
    let manager = OwnerId::from(UserId::new(Uuid::now_v7()));
    let group = OwnerId::from(GroupId::new(Uuid::now_v7()));
    let built = Proxiima::<AgentMemoryApp>::app()
        .database_url(split_db.runtime_url().to_owned())
        .platform_database_url(split_db.platform_url().to_owned())
        .store_number(0)
        .owner(manager)
        .served_surface(ServedSurface::All)
        .build()
        .await
        .expect("host builds");
    let scope = proxiima_storage_pg::PgPlatformScope::new(
        sqlx::PgPool::connect(split_db.platform_url())
            .await
            .expect("platform pool"),
        &["proxiima_core"],
    )
    .await
    .expect("platform scope");
    let pg = PgStorage::connect_with_config(
        split_db.runtime_url(),
        proxiima_storage_pg::PgPoolConfig::from_env().expect("pool config"),
        proxiima_storage_pg::PgTuning::from_env().expect("tuning"),
        scope,
        0,
    )
    .await
    .expect("storage");
    seed_group_membership(&pg, &group, Relation::Admin, &manager).await;
    let authz = space_authz(manager, vec![manager, group], Role::admin());

    let refused = call_test_model_tool(
        &built.host().core_mcp_tools(),
        authz,
        manager,
        "core_membership",
        serde_json::json!({
            "action": "add_member",
            "group": group.into_inner().to_string(),
            "member": group.into_inner().to_string(),
        }),
    )
    .await
    .expect_err("a group cannot contain itself");
    assert_eq!(refused.kind(), CoreMcpErrorKind::InvalidInput, "{refused}");
    built.shutdown().await;
}
