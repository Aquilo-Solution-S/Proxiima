//! The row insert `pg_sidecar!` runs and a hand-written sidecar calls
//! ([`insert_memory_sidecar_row`]): the optional column kinds round-trip, an
//! owner-stamping row takes its memory's owner, and a name that is not a
//! plain identifier never reaches the database.

use proxiima_core::verbs::schema::PayloadKind;
use proxiima_core::{
    DurablePayload, DurablePayloadBuilder, FactPayload, InterpretationV1, KeyField, MemoryId,
    MemoryKind, OwnerId, PayloadDisplay, PayloadKeyBuilder, PayloadProjection, PerspectivePayload,
    StorageError, StoredInterpretationError, UserId,
};
use proxiima_pg_testkit::{db_url, drop_db, unique_db_name};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use super::{
    PgMemoryPayload, PgMemorySidecar, PgSidecarFuture, PgSidecarKey, PgSidecarReadCtx,
    SidecarInsertPermit, SidecarOwnerMode, insert_memory_sidecar_row,
};
use crate::PgStorage;
use crate::test_fixtures::{TestMemory, create_core_db, create_interpretation};

const TABLES: &str = "
CREATE TABLE public.optional_kinds_v1 (
    t uuid PRIMARY KEY REFERENCES proxiima_core.memory (t),
    seen_at timestamptz,
    attempts integer
);
CREATE TABLE public.envelope_v1 (
    t uuid PRIMARY KEY REFERENCES proxiima_core.memory (t),
    owner_id uuid NOT NULL,
    envelope jsonb NOT NULL
)";

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct OptionalKinds {
    seen_at: Option<OffsetDateTime>,
    attempts: Option<i32>,
}

impl FactPayload for OptionalKinds {
    const SCHEMA_ID: &'static str = "test/optional-kinds-v1";
    const SCHEMA_VERSION: u32 = 1;

    fn receipt_key(&self) -> Vec<u8> {
        PayloadKeyBuilder::new(Self::SCHEMA_ID, Self::SCHEMA_VERSION).finish()
    }

    fn render(&self) -> String {
        String::new()
    }
}

impl PayloadProjection for OptionalKinds {
    const KEY_FIELDS: &'static [KeyField<Self>] = &[];

    fn display(&self) -> PayloadDisplay {
        PayloadDisplay::default()
    }

    fn durable(&self) -> Result<DurablePayload, String> {
        Ok(DurablePayloadBuilder::new().finish())
    }
}

crate::pg_sidecar! {
    payload: OptionalKinds,
    row: OptionalKindsRow,
    kinds: [Fact],
    table: "public.optional_kinds_v1",
    key: t,
    fields: {
        seen_at => seen_at: (opt_timestamptz),
        attempts => attempts: (opt_i32),
    },
}

/// A sidecar whose insert is written by hand, and stamps the owner like
/// `owner: follows`.
struct Envelope {
    body: serde_json::Value,
    table: &'static str,
}

impl PgMemorySidecar for Envelope {
    fn insert_memory_sidecar<'t>(
        &'t self,
        tx: &'t mut sqlx::Transaction<'_, sqlx::Postgres>,
        memory_id: MemoryId,
        permit: SidecarInsertPermit,
    ) -> PgSidecarFuture<'t> {
        Box::pin(insert_memory_sidecar_row::<Self>(
            tx,
            memory_id,
            permit,
            self.table,
            &[("envelope", None)],
            |query| Ok(query.bind(&self.body)),
        ))
    }
}

impl PgMemoryPayload for Envelope {
    const OWNER: SidecarOwnerMode = SidecarOwnerMode::Follows;
    const MEMORY_KEY_COLUMN: &'static str = "t";
}

fn envelope(table: &'static str) -> Envelope {
    Envelope {
        body: serde_json::json!({"kind": "reading", "value": 3}),
        table,
    }
}

#[tokio::test]
async fn interpretation_sql_check_agrees_with_ledger_confidence_bounds() {
    const MAX: i16 = proxiima_core::ledger!(Proxiima.Memory.confidenceMax);
    with_store("proxiima_interpretation_confidence", async |_, admin, _| {
        let mut connection = admin.acquire().await?;
        // Copy the installed CHECK without the parent-row triggers or foreign key.
        sqlx::query(
            "CREATE TEMP TABLE confidence_check
             (LIKE proxiima_core.interpretation_v1 INCLUDING DEFAULTS INCLUDING CONSTRAINTS)",
        )
        .execute(&mut *connection)
        .await?;
        for score in [-1, 0, MAX, MAX + 1] {
            let result = sqlx::query(
                "INSERT INTO confidence_check (t, claim, confidence, model_id, client_name, client_version)
                 VALUES ($1, 'claim', $2, 'model', 'client', '1')",
            )
            .bind(Uuid::now_v7())
            .bind(score)
            .execute(&mut *connection)
            .await;
            if (0..=MAX).contains(&score) {
                result?;
            } else {
                let error = result.expect_err("out-of-range confidence must refuse");
                assert_eq!(
                    error.as_database_error().and_then(|error| error.constraint()),
                    Some("interpretation_v1_confidence_chk")
                );
            }
        }
        Ok(())
    })
    .await;
}

#[tokio::test]
async fn legacy_interpretations_refuse_batch_decode_with_the_row_and_reason() {
    with_store("proxiima_legacy_interpretation", async |pg, admin, owner| {
        let subject = TestMemory::fact().create(pg, owner).await?.memory_id;
        let score = create_interpretation(
            pg,
            owner,
            InterpretationV1::fixture("legacy score", 80, [(subject, MemoryKind::Fact)]),
        )
        .await?;
        let subjects = create_interpretation(
            pg,
            owner,
            InterpretationV1::fixture("legacy subjects", 80, [(subject, MemoryKind::Fact)]),
        )
        .await?;

        let mut tx = admin.begin().await?;
        // Seed data the previous release could have admitted before this decoder and CHECK.
        sqlx::raw_sql(
            "ALTER TABLE proxiima_core.interpretation_v1
                DROP CONSTRAINT interpretation_v1_confidence_chk,
                DISABLE TRIGGER interpretation_v1_append_only",
        )
        .execute(tx.as_mut())
        .await?;
        sqlx::query("UPDATE proxiima_core.interpretation_v1 SET confidence = 101 WHERE t = $1")
            .bind(score.into_inner())
            .execute(tx.as_mut())
            .await?;
        sqlx::query("UPDATE proxiima_core.interpretation_v1 SET subject_kinds = '{}' WHERE t = $1")
            .bind(subjects.into_inner())
            .execute(tx.as_mut())
            .await?;
        sqlx::raw_sql(include_str!("../../migrations/0072_interpretation_confidence.sql"))
            .execute(tx.as_mut())
            .await?;
        sqlx::query(
            "ALTER TABLE proxiima_core.interpretation_v1 ENABLE TRIGGER interpretation_v1_append_only",
        )
        .execute(tx.as_mut())
        .await?;
        tx.commit().await?;

        let key = PgSidecarKey::new(
            PayloadKind::Perspective,
            InterpretationV1::schema_id(),
            proxiima_core::SchemaVersion::new(InterpretationV1::SCHEMA_VERSION),
        );
        let invalid_score = pg
            .sidecars()
            .load_memory_payloads_batch(admin.into(), &key, &[score])
            .await
            .expect_err("legacy score must refuse");
        assert!(
            matches!(invalid_score, StorageError::InvalidStoredInterpretation {
                memory,
                reason: StoredInterpretationError::Confidence { value: 101 },
            } if memory == score),
            "{invalid_score:?}"
        );
        let invalid_subjects = pg
            .sidecars()
            .load_memory_payloads_batch(admin.into(), &key, &[subjects])
            .await
            .expect_err("legacy subject columns must refuse");
        assert!(
            matches!(invalid_subjects, StorageError::InvalidStoredInterpretation {
                memory,
                reason: StoredInterpretationError::Subjects(_),
            } if memory == subjects),
            "{invalid_subjects:?}"
        );
        Ok(())
    })
    .await;
}

/// A migrated store with the two test tables, an administrator pool to
/// write and read them, and an owner.
async fn with_store<F>(name: &str, body: F)
where
    F: AsyncFnOnce(&PgStorage, &PgPool, OwnerId) -> Result<(), Box<dyn std::error::Error>>,
{
    let db_name = unique_db_name(name);
    create_core_db(&db_name)
        .await
        .expect("PG required for tests");
    let result = async {
        let pg = PgStorage::connect_for_tests(db_url(&db_name)).await?;
        let admin = PgPool::connect(&db_url(&db_name)).await?;
        sqlx::raw_sql(TABLES).execute(&admin).await?;
        let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
        let result = body(&pg, &admin, owner).await;
        admin.close().await;
        result
    }
    .await;
    let _ = drop_db(&db_name).await;
    result.unwrap_or_else(|err| panic!("{name} failed: {err}"));
}

async fn insert(
    admin: &PgPool,
    sidecar: &impl PgMemorySidecar,
    memory: MemoryId,
) -> Result<(), StorageError> {
    let mut tx = admin.begin().await.map_err(crate::error::map_err)?;
    sidecar
        .insert_memory_sidecar(&mut tx, memory, SidecarInsertPermit::new())
        .await?;
    tx.commit().await.map_err(crate::error::map_err)
}

#[tokio::test]
async fn an_optional_timestamp_and_integer_round_trip_as_some_and_as_none() {
    with_store(
        "proxiima_sidecar_optional_kinds",
        async |pg, admin, owner| {
            for payload in [
                OptionalKinds {
                    seen_at: Some(OffsetDateTime::from_unix_timestamp(1_700_000_000)?),
                    attempts: Some(-7),
                },
                OptionalKinds {
                    seen_at: None,
                    attempts: None,
                },
            ] {
                let memory = TestMemory::fact().create(pg, owner).await?.memory_id;
                insert(admin, &payload, memory).await?;

                let loaded = <OptionalKinds as PgMemoryPayload>::load_batch(
                    PgSidecarReadCtx::from(admin),
                    PayloadKind::Fact,
                    &[memory],
                )
                .await?;

                assert_eq!(loaded.len(), 1, "{payload:?}");
                assert_eq!(
                    loaded[0].1.downcast_ref::<OptionalKinds>(),
                    Some(&payload),
                    "the row decodes to what was written"
                );
            }
            Ok(())
        },
    )
    .await;
}

#[tokio::test]
async fn a_hand_written_insert_stamps_the_owner_of_its_memory() {
    with_store("proxiima_sidecar_row_stamp", async |pg, admin, owner| {
        let memory = TestMemory::fact().create(pg, owner).await?.memory_id;
        insert(admin, &envelope("public.envelope_v1"), memory).await?;

        let (owner_id, body): (Uuid, serde_json::Value) =
            sqlx::query_as("SELECT owner_id, envelope FROM public.envelope_v1 WHERE t = $1")
                .bind(memory.into_inner())
                .fetch_one(admin)
                .await?;
        assert_eq!(owner_id, owner.into_inner());
        assert_eq!(body, envelope("public.envelope_v1").body);

        let missing = insert(
            admin,
            &envelope("public.envelope_v1"),
            MemoryId::new(Uuid::now_v7()),
        )
        .await
        .expect_err("no memory to take the owner from");
        assert!(
            matches!(&missing, StorageError::ConstraintViolation(message)
                if message.contains("has no Memory row")),
            "{missing:?}"
        );
        Ok(())
    })
    .await;
}

#[tokio::test]
async fn a_name_that_is_not_a_plain_identifier_never_reaches_the_database() {
    with_store("proxiima_sidecar_row_ident", async |pg, admin, owner| {
        let memory = TestMemory::fact().create(pg, owner).await?.memory_id;
        for table in [
            "public.envelope_v1; DROP TABLE public.envelope_v1",
            "public.envelope_v1 (t) VALUES ($1) --",
            "\"public\".envelope_v1",
        ] {
            let refused = insert(admin, &envelope(table), memory)
                .await
                .expect_err("not an identifier");
            assert!(
                matches!(refused, StorageError::Internal(_)),
                "{table}: {refused:?}"
            );
        }
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM public.envelope_v1")
            .fetch_one(admin)
            .await?;
        assert_eq!(rows, 0, "nothing was written");
        Ok(())
    })
    .await;
}
