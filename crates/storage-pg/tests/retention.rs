//! The `retention` rows follow their owner: owner export carries them and
//! owner erase takes them, leaving every other owner's rows.

use proxiima_core::owner_inverse::{
    EraseAuthorization, ExportAuthorization, OwnerEraseOutcome, OwnerEraseTarget,
    OwnerExportTarget, OwnerSurfaces,
};
use proxiima_core::storage_ports::{OwnerInversePort, OwnerWritePermit, RetentionPort};
use proxiima_core::{KeepDays, MemoryKind, OwnerId, SchemaId, SpiiceRegistry, UserId};
use proxiima_pg_testkit::{db_url, drop_db};
use proxiima_storage_pg::PgStorage;
use proxiima_storage_pg::test_fixtures::create_core_db;
use uuid::Uuid;

fn surfaces() -> OwnerSurfaces {
    OwnerSurfaces::for_registry(&SpiiceRegistry::new().freeze_or_panic_for_tests())
}

/// A person with two retention rows, written through the retention port (which
/// registers the owner with its first row).
async fn person_with_rows(pg: &PgStorage) -> Result<UserId, Box<dyn std::error::Error>> {
    let person = UserId::new(Uuid::now_v7());
    let permit = OwnerWritePermit::new_for_tests(OwnerId::from(person), MemoryKind::Fact);
    for (schema, days) in [("s.mail-v1", 30), ("s.page-v1", 365)] {
        pg.set_retention(
            &permit,
            &SchemaId::new(schema.into()),
            Some(KeepDays::new(days)?),
        )
        .await?;
    }
    Ok(person)
}

async fn rows_of(pool: &sqlx::PgPool, owner: UserId) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT count(*) FROM proxiima_core.retention WHERE owner_id = $1")
        .bind(owner.into_inner())
        .fetch_one(pool)
        .await
}

#[tokio::test]
async fn retention_database_bounds_match_the_ledger() {
    let (pg, _database) = proxiima_storage_pg::test_fixtures::fresh_pg("retention_bounds").await;
    let owner = person_with_rows(&pg).await.expect("retention rows");
    let min = i32::from(KeepDays::MIN);
    let max = i32::from(KeepDays::MAX);
    for (days, valid) in [(min - 1, false), (min, true), (max, true), (max + 1, false)] {
        let result =
            sqlx::query("UPDATE proxiima_core.retention SET keep_days = $1 WHERE owner_id = $2")
                .bind(days)
                .bind(owner.into_inner())
                .execute(pg.pool_for_tests())
                .await;
        if valid {
            assert_eq!(result.expect("bound admitted").rows_affected(), 2);
        } else {
            assert_eq!(
                result
                    .expect_err("out-of-range day refused")
                    .as_database_error()
                    .and_then(|error| error.constraint()),
                Some("retention_keep_days_chk"),
            );
        }
    }
}

#[tokio::test]
async fn owner_export_carries_the_rows_and_owner_erase_takes_them()
-> Result<(), Box<dyn std::error::Error>> {
    let name = format!("proxiima_test_retention_{}", Uuid::now_v7().simple());
    create_core_db(&name).await.expect("create test database");
    let result: Result<(), Box<dyn std::error::Error>> = async {
        let pg = PgStorage::connect_for_tests(db_url(&name)).await?;
        let pool = pg.pool_for_tests();
        let person = person_with_rows(&pg).await?;
        let neighbour = person_with_rows(&pg).await?;

        let export = ExportAuthorization::new_for_tests(OwnerExportTarget::PersonalOwner {
            user_id: person,
        });
        let bundle = pg.export_owner_bundle(&export, &surfaces()).await?;
        let mut exported: Vec<(String, i64)> = bundle.tables["proxiima_core.retention"]
            .iter()
            .map(|row| {
                assert_eq!(row["owner_id"], person.into_inner().to_string());
                (
                    row["schema_id"].as_str().unwrap_or_default().to_owned(),
                    row["keep_days"].as_i64().unwrap_or_default(),
                )
            })
            .collect();
        exported.sort();
        assert_eq!(
            exported,
            [("s.mail-v1".to_owned(), 30), ("s.page-v1".to_owned(), 365)],
            "the bundle carries the owner's rows and no neighbour's"
        );

        let erase = EraseAuthorization::new_for_tests(OwnerEraseTarget::PersonalOwner {
            user_id: person,
            drop_event_id: "retention-test".into(),
        });
        let erased = pg.erase_personal_owner(&erase, person, &surfaces()).await?;
        assert!(
            matches!(erased, OwnerEraseOutcome::Completed { .. }),
            "{erased:?}"
        );
        assert_eq!(rows_of(pool, person).await?, 0, "the erase takes the rows");
        assert_eq!(
            rows_of(pool, neighbour).await?,
            2,
            "a neighbour keeps its rows"
        );
        Ok(())
    }
    .await;
    drop_db(&name).await.ok();
    result
}
