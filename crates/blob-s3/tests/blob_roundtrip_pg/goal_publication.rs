use super::*;

use proxiima_core::test_fixtures::authenticated_context;
use proxiima_core::verbs::goal_write::{
    GoalAuthorship, GoalCreateRequest, GoalTopologyWrite, IdempotencyKey,
};
use proxiima_core::{
    GoalAudienceGrantInput, GoalPublicationInput, MemoryId, PublicationUse, Role, SimpleTextGoalV1,
};

#[tokio::test(flavor = "multi_thread")]
#[expect(
    clippy::too_many_lines,
    reason = "one publication lifecycle verifies exact capture across a later upload and revocation"
)]
async fn goal_publication_reads_only_the_exact_cited_upload_route() {
    if !S3RuntimeConfig::present_in_env() {
        eprintln!("skipped: PROXIIMA_S3_* unset (run the s3 service to enable)");
        return;
    }

    let (pg, db_name) = fresh_storage().await;
    let pool = pg.pool_for_tests().clone();
    let config = s3_config_for_dev();
    let store = CitedBlobStore::new(
        pg.clone_pool_for_backend(),
        config.clone(),
        pg.platform_scope_for_host(),
    )
    .expect("valid S3 config");

    let owner = owner_fixture();
    let source_user = UserId::new(owner.into_inner());
    let source =
        authenticated_context(AuthzContext::for_subject(source_user, AuthPath::HostBearer));
    let (cited_object_id, original_object_key) =
        complete_upload_fixture(&pg, &store, &config, &pool, &source, owner)
            .await
            .expect("completed upload");
    let cited_object_id = Uuid::parse_str(&cited_object_id).expect("cited object id");
    let fact_memory: MemoryId = MemoryId::new(
        sqlx::query_scalar("SELECT t FROM proxiima_core.memory WHERE blob_id = $1")
            .bind(cited_object_id)
            .fetch_one(&pool)
            .await
            .expect("upload fact"),
    );

    let engine = Engine::new(SpiiceRegistry::new().freeze_or_panic_for_tests())
        .with_storage_ports(Arc::new(pg.clone()).storage_ports());
    let bob = UserId::new(Uuid::now_v7());
    let goal = goal_audience_fixture(&engine, &source, owner, bob).await;
    let bob_auth = authenticated_context(AuthzContext::for_subject(bob, AuthPath::HostBearer));
    let read = NonZeroU64::new(4).expect("nonzero ceiling");
    let missing_route = store
        .collect_verified_for_goal(&bob_auth, owner, goal, fact_memory, cited_object_id, read)
        .await
        .expect_err("unpublished route");
    assert!(matches!(
        missing_route,
        CitedBlobReadError::NotFound | CitedBlobReadError::AccessDenied
    ));

    engine
        .publish_to_goal(
            &source,
            &GoalPublicationInput {
                goal,
                memory: fact_memory,
                permitted_use: PublicationUse::Read,
            },
        )
        .await
        .expect("publish exact fact");
    let verified = store
        .collect_verified_for_goal(&bob_auth, owner, goal, fact_memory, cited_object_id, read)
        .await
        .expect("published cited upload");
    assert_eq!(verified.bytes, b"real");
    assert_eq!(verified.filename, "genuine.pdf");

    let delivery = delivery_service(&store);
    let ordinary_denied = delivery
        .authorize_delivery(
            &bob_auth,
            CitedBlobDeliveryRequest::Owner {
                owner,
                cited_object_id,
            },
        )
        .await
        .expect_err("Goal audience cannot use ordinary owner delivery");
    assert!(matches!(ordinary_denied, CitedBlobReadError::AccessDenied));

    let admitted = delivery
        .authorize_delivery(
            &bob_auth,
            CitedBlobDeliveryRequest::GoalPublication {
                owner,
                goal,
                fact_memory,
                cited_object_id,
            },
        )
        .await
        .expect("published Goal admits delivery");
    let (_request, target) = admitted.into_parts();
    assert_eq!(target.object_key, original_object_key);
    let captured_upload_id = target.upload_id;
    assert_eq!(target.filename, "genuine.pdf");

    let later =
        upload_goal_fixture(&pg, &store, &config, &source, owner, "later.pdf", b"real").await;
    assert_eq!(later.blob.cited_object_id, cited_object_id.to_string());
    let snapshot = store
        .collect_verified_for_goal(&bob_auth, owner, goal, fact_memory, cited_object_id, read)
        .await
        .expect("published metadata snapshot");
    assert_eq!(snapshot.filename, "genuine.pdf");
    let after_later_upload = delivery
        .authorize_delivery(
            &bob_auth,
            CitedBlobDeliveryRequest::GoalPublication {
                owner,
                goal,
                fact_memory,
                cited_object_id,
            },
        )
        .await
        .expect("exact published upload remains admitted");
    let (_request, after_target) = after_later_upload.into_parts();
    assert_eq!(after_target.upload_id, captured_upload_id);
    assert_eq!(after_target.object_key, original_object_key);
    assert_eq!(after_target.filename, "genuine.pdf");

    let unrelated =
        upload_goal_fixture(&pg, &store, &config, &source, owner, "other.pdf", b"other").await;
    let unrelated_id =
        Uuid::parse_str(&unrelated.blob.cited_object_id).expect("unrelated object id");
    let unrelated_err = store
        .collect_verified_for_goal(&bob_auth, owner, goal, fact_memory, unrelated_id, read)
        .await
        .expect_err("unrelated blob route");
    assert!(matches!(
        unrelated_err,
        CitedBlobReadError::NotFound | CitedBlobReadError::AccessDenied
    ));

    let request = CitedBlobDeliveryRequest::GoalPublication {
        owner,
        goal,
        fact_memory,
        cited_object_id,
    };
    let already_admitted = delivery
        .authorize_delivery(&bob_auth, request.clone())
        .await
        .expect("admit before revocation");
    for wrong_route in [
        CitedBlobDeliveryRequest::GoalPublication {
            owner,
            goal,
            fact_memory,
            cited_object_id: unrelated_id,
        },
        CitedBlobDeliveryRequest::GoalPublication {
            owner,
            goal,
            fact_memory: MemoryId::new(Uuid::now_v7()),
            cited_object_id,
        },
        CitedBlobDeliveryRequest::GoalPublication {
            owner,
            goal: proxiima_core::GoalHandle::new(Uuid::now_v7()),
            fact_memory,
            cited_object_id,
        },
    ] {
        assert!(matches!(
            delivery.authorize_delivery(&bob_auth, wrong_route).await,
            Err(CitedBlobReadError::NotFound | CitedBlobReadError::AccessDenied)
        ));
    }

    engine
        .unpublish_from_goal(&source, goal, fact_memory)
        .await
        .expect("revoke publication");
    let revoked = store
        .collect_verified_for_goal(&bob_auth, owner, goal, fact_memory, cited_object_id, read)
        .await
        .expect_err("revoked route");
    assert!(matches!(
        revoked,
        CitedBlobReadError::NotFound | CitedBlobReadError::AccessDenied
    ));
    let revoked_delivery = delivery
        .authorize_delivery(
            &bob_auth,
            CitedBlobDeliveryRequest::GoalPublication {
                owner,
                goal,
                fact_memory,
                cited_object_id,
            },
        )
        .await
        .expect_err("revoked Goal route denies a new delivery request");
    assert!(matches!(
        revoked_delivery,
        CitedBlobReadError::NotFound | CitedBlobReadError::AccessDenied
    ));

    let (admitted_request, admitted_target) = already_admitted.into_parts();
    assert_eq!(admitted_request, request);
    assert_eq!(admitted_target.upload_id, captured_upload_id);

    drop_db(&db_name).await.expect("drop test database");
}

async fn goal_audience_fixture(
    engine: &Engine,
    source: &AuthzContext,
    owner: OwnerId,
    bob: UserId,
) -> proxiima_core::GoalHandle {
    let goal = engine
        .create_goal(
            source,
            GoalCreateRequest {
                owner,
                topology: GoalTopologyWrite::new(Vec::new(), Vec::new(), Vec::new())
                    .expect("empty topology"),
                title: "Blob publication".into(),
                text: "Read one exact uploaded fact".into(),
                payload: SimpleTextGoalV1 {},
                request_id: IdempotencyKey::generated("blob-publication"),
                authorship: GoalAuthorship::User,
                author_self_perspective_id: None,
            },
        )
        .await
        .expect("goal")
        .goal_handle;

    engine
        .grant_goal_access(
            source,
            &GoalAudienceGrantInput {
                goal,
                subject: bob,
                role: Role::viewer(),
                expires_at: None,
            },
        )
        .await
        .expect("viewer grant");
    goal
}

async fn upload_goal_fixture(
    pg: &PgStorage,
    store: &CitedBlobStore,
    config: &S3RuntimeConfig,
    source: &AuthzContext,
    owner: OwnerId,
    filename: &str,
    bytes: &'static [u8],
) -> UploadCompleted {
    let prepared = store
        .prepare_upload(
            source,
            CitedBlobUploadPrepareTs {
                owner,
                filename: filename.into(),
                mime: "application/pdf".into(),
                byte_len: u64::try_from(bytes.len()).expect("fixture byte length"),
            },
        )
        .await
        .expect("prepare upload");
    let key: String =
        sqlx::query_scalar("SELECT object_key FROM proxiima_core.blob_uploads WHERE upload_id=$1")
            .bind(Uuid::parse_str(&prepared.upload_id).expect("upload id"))
            .fetch_one(pg.pool_for_tests())
            .await
            .expect("upload row");
    put_object_via_sdk(config, &key, bytes).await;
    complete_via_engine(pg, store, source, owner, &prepared.upload_id)
        .await
        .expect("complete upload")
}
