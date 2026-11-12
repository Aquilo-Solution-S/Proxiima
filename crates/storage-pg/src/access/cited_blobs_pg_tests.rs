//! `load_cited_blob_source` fences the source owner itself.

use super::{CitedBlobUploads, load_cited_blob_source};
use crate::PgStorage;
use crate::test_fixtures::{create_core_db, create_user};
use proxiima_core::{OwnerId, UserId};
use proxiima_pg_testkit::{db_url, drop_db};
use uuid::Uuid;

#[tokio::test]
async fn loading_a_cited_blob_source_holds_the_source_owners_shared_fence() {
    let db_name = format!("proxiima_test_{}", Uuid::now_v7().simple());
    if let Err(e) = create_core_db(&db_name).await {
        panic!("PG required for tests but admin connect failed: {e}");
    }
    let result: Result<(), Box<dyn std::error::Error>> = async {
        let pg = PgStorage::connect_for_tests(db_url(&db_name)).await?;
        let pool = pg.pool_for_tests();
        let user = UserId::new(Uuid::now_v7());
        let owner = OwnerId::from(user);
        create_user(pool, user).await?;
        // Raw INSERT: no write path for blob reachable from test fixtures.
        let blob_id: Uuid = sqlx::query_scalar(
            "INSERT INTO proxiima_core.blob (owner_id, schema_id, content_hash)
             VALUES ($1, 'core/uploaded-blob-v1', $2) RETURNING blob_id",
        )
        .bind(owner.into_inner())
        .bind([5_u8; 32].as_slice())
        .fetch_one(pool)
        .await?;

        // A transaction that took no fence before the call.
        let mut tx = pool.begin().await?;
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(tx.as_mut())
            .await?;
        // The upload set is not exact, so the load refuses; the fence is held
        // from before the refusal.
        let _ = load_cited_blob_source(&mut tx, blob_id, owner.into_inner(), CitedBlobUploads::Owner)
            .await;
        let modes: Vec<String> = sqlx::query_scalar(
            "SELECT l.mode
               FROM pg_locks l,
                    LATERAL (SELECT hashtextextended('proxiima-owner-fence:' || $2::text, 0) AS key) k
              WHERE l.locktype = 'advisory' AND l.granted AND l.pid = $1
                AND l.classid::bigint = ((k.key >> 32) & 4294967295)
                AND l.objid::bigint = (k.key & 4294967295)",
        )
        .bind(pid)
        .bind(owner.into_inner())
        .fetch_all(pool)
        .await?;
        assert_eq!(modes, ["ShareLock"]);
        tx.rollback().await?;
        Ok(())
    }
    .await;
    let _ = drop_db(&db_name).await;
    result.expect("source owner fence held");
}
