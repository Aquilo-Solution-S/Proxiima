//! The `retention` rows through the Engine: set by whoever may erase that
//! owner's memory by hand, read back for exactly the owners the caller may
//! read, and read whole under the host's system authority.

#[path = "fixtures/example_engine.rs"]
#[allow(dead_code, reason = "each suite uses part of the shared fixture")]
mod example_engine;

use example_engine::ExampleEngine;
use proxiima_core::{
    AuthPath, AuthzContext, ErrorCode, KeepDays, OwnerId, Retention, Role, SchemaId, UserId,
};
use uuid::Uuid;

const MAIL: &str = "proxiima-example/note-v1";
const PAGE: &str = "proxiima-example/digest-v1";

fn days(days: i64) -> KeepDays {
    KeepDays::new(days).expect("in range")
}

fn schema(id: &str) -> SchemaId {
    SchemaId::new(id.to_owned())
}

/// An authenticated person not yet admitted to storage, holding `roles`
/// on other owners.
fn person(roles: &[(OwnerId, Role)]) -> (OwnerId, AuthzContext) {
    let user = UserId::new(Uuid::now_v7());
    (OwnerId::from(user), context(user, roles))
}

fn context(user: UserId, roles: &[(OwnerId, Role)]) -> AuthzContext {
    proxiima_core::test_fixtures::authenticated_context(AuthzContext::for_subject_with_role(
        user,
        roles.iter().copied(),
        AuthPath::HostBearer,
    ))
}

async fn register(h: &ExampleEngine, owner: OwnerId, kind: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO proxiima_core.owners (owner_id, kind)
         VALUES ($1, $2::proxiima_core.owner_kind) ON CONFLICT DO NOTHING",
    )
    .bind(owner.into_inner())
    .bind(kind)
    .execute(&h.pool)
    .await?;
    Ok(())
}

fn row(owner: OwnerId, schema_id: &str, keep: i64) -> Retention {
    Retention {
        owner,
        schema: schema(schema_id),
        keep_days: days(keep),
    }
}

/// Rows in the order the read answers: by owner, then schema.
fn sorted(mut rows: Vec<Retention>) -> Vec<Retention> {
    rows.sort_by(|a, b| (a.owner, a.schema.as_str()).cmp(&(b.owner, b.schema.as_str())));
    rows
}

async fn owner_kind(h: &ExampleEngine, owner: OwnerId) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT kind::text FROM proxiima_core.owners WHERE owner_id = $1")
        .bind(owner.into_inner())
        .fetch_optional(&h.pool)
        .await
}

#[tokio::test]
async fn personal_retention_is_a_first_write_and_unset_keeps_for_good()
-> Result<(), Box<dyn std::error::Error>> {
    let h = ExampleEngine::new("proxiima_retention_first", |_| {}).await;
    let (owner, authz) = person(&[]);
    let (other, _) = person(&[]);
    assert_eq!(owner_kind(&h, owner).await?, None);
    assert!(h.engine.read_retention(&authz).await?.is_empty());
    h.engine
        .set_retention(&authz, &owner, &schema(MAIL), None)
        .await?;
    assert_eq!(owner_kind(&h, owner).await?, None, "unset admits no owner");

    for keep in [Some(days(30)), None] {
        let refused = h
            .engine
            .set_retention(&authz, &other, &schema(MAIL), keep)
            .await
            .expect_err("one person never sets another's retention");
        assert_eq!(refused.code, ErrorCode::Forbidden);
    }
    assert_eq!(owner_kind(&h, other).await?, None);

    h.engine
        .set_retention(&authz, &owner, &schema(MAIL), Some(days(30)))
        .await?;
    assert_eq!(owner_kind(&h, owner).await?.as_deref(), Some("personal"));
    assert_eq!(
        h.engine.read_retention(&authz).await?,
        vec![row(owner, MAIL, 30)]
    );
    h.engine
        .set_retention(&authz, &owner, &schema(MAIL), None)
        .await?;
    assert!(h.engine.read_retention(&authz).await?.is_empty());
    assert_eq!(owner_kind(&h, owner).await?.as_deref(), Some("personal"));
    Ok(())
}

#[tokio::test]
async fn failed_first_retention_rolls_back_owner_admission()
-> Result<(), Box<dyn std::error::Error>> {
    let h = ExampleEngine::new("proxiima_retention_rollback", |_| {}).await;
    let (owner, authz) = person(&[]);
    assert_eq!(owner_kind(&h, owner).await?, None);
    sqlx::query(
        "ALTER TABLE proxiima_core.retention ADD CONSTRAINT refuse_retention CHECK (false)",
    )
    .execute(&h.pool)
    .await?;
    let refused = h
        .engine
        .set_retention(&authz, &owner, &schema(MAIL), Some(days(30)))
        .await
        .expect_err("the retention insert is rejected");
    assert_eq!(refused.code, ErrorCode::Internal);
    assert_eq!(
        owner_kind(&h, owner).await?,
        None,
        "admission shares the rejected write's transaction"
    );
    assert!(h.engine.read_retention(&authz).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn a_row_is_set_by_whoever_may_erase_that_owners_memory() {
    let h = ExampleEngine::new("proxiima_retention", |_| {}).await;
    let result: Result<(), Box<dyn std::error::Error>> = async {
        let group = OwnerId::new(Uuid::now_v7());
        register(&h, group, "group").await?;
        let (admin, admin_authz) = person(&[(group, Role::admin())]);
        let (editor, editor_authz) = person(&[(group, Role::editor())]);
        let (viewer, viewer_authz) = person(&[(group, Role::viewer())]);
        let (ingest, ingest_authz) = person(&[(group, Role::ingest())]);
        let (outsider, outsider_authz) = person(&[]);

        for (authz, entry) in [
            (&admin_authz, row(group, MAIL, 30)),
            (&admin_authz, row(group, PAGE, 14)),
            (&admin_authz, row(admin, PAGE, 7)),
            (&editor_authz, row(editor, MAIL, 90)),
            (&viewer_authz, row(viewer, PAGE, 14)),
            (&ingest_authz, row(ingest, MAIL, 180)),
            (&outsider_authz, row(outsider, PAGE, 365)),
        ] {
            h.engine
                .set_retention(authz, &entry.owner, &entry.schema, Some(entry.keep_days))
                .await?;
        }
        assert_eq!(owner_kind(&h, group).await?.as_deref(), Some("group"));
        for (who, authz) in [
            ("editor", &editor_authz),
            ("viewer", &viewer_authz),
            ("ingest", &ingest_authz),
            ("outsider", &outsider_authz),
        ] {
            for keep in [Some(days(1)), None] {
                let refused = h
                    .engine
                    .set_retention(authz, &group, &schema(MAIL), keep)
                    .await
                    .expect_err("only the group's admin sets or removes its row");
                assert_eq!(refused.code, ErrorCode::Forbidden, "{who}");
            }
        }
        // #5114, "Who reads it": every relation uses the owner's read set.
        for (who, authz, own) in [
            ("admin", &admin_authz, row(admin, PAGE, 7)),
            ("editor", &editor_authz, row(editor, MAIL, 90)),
            ("viewer", &viewer_authz, row(viewer, PAGE, 14)),
            ("ingest", &ingest_authz, row(ingest, MAIL, 180)),
        ] {
            assert_eq!(
                h.engine.read_retention(authz).await?,
                sorted(vec![row(group, MAIL, 30), row(group, PAGE, 14), own]),
                "{who} reads its own rows and exactly its group's rows"
            );
        }
        assert_eq!(
            h.engine.read_retention(&outsider_authz).await?,
            vec![row(outsider, PAGE, 365)],
            "an outsider reads only its own rows"
        );
        let removed_authz = context(UserId::new(editor.into_inner()), &[]);
        assert_eq!(
            h.engine.read_retention(&removed_authz).await?,
            vec![row(editor, MAIL, 90)],
            "a removed member's refreshed context reads only its own rows"
        );

        h.engine
            .set_retention(&admin_authz, &group, &schema(MAIL), Some(days(60)))
            .await?;
        h.engine
            .set_retention(&admin_authz, &admin, &schema(PAGE), None)
            .await?;
        assert_eq!(
            h.engine.read_retention(&admin_authz).await?,
            sorted(vec![row(group, MAIL, 60), row(group, PAGE, 14)]),
            "a set replaces the row, and a set with no days removes it"
        );

        let (engine, system) = h.engine.into_system_authority();
        assert_eq!(
            engine.all_retention(&system).await?,
            sorted(vec![
                row(group, MAIL, 60),
                row(group, PAGE, 14),
                row(editor, MAIL, 90),
                row(viewer, PAGE, 14),
                row(ingest, MAIL, 180),
                row(outsider, PAGE, 365),
            ]),
            "the host's clock reads every owner's rows"
        );
        let (_, foreign) = proxiima_core::Engine::new(
            proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests(),
        )
        .into_system_authority();
        let refused = engine
            .all_retention(&foreign)
            .await
            .expect_err("another boot's authority reads nothing");
        assert_eq!(refused.code, ErrorCode::Forbidden);
        Ok(())
    }
    .await;
    result.expect("retention authority");
}
