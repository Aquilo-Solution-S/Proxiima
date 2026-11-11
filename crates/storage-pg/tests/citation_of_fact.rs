//! citations_of_facts is memory ⋈ blob.
#![allow(clippy::doc_markdown)]

use proxiima_core::OwnerId;
use proxiima_core::UserId;
use proxiima_core::storage_ports::CitationPort;
use proxiima_pg_testkit::{db_url, drop_db};
use proxiima_storage_pg::PgStorage;
use proxiima_storage_pg::test_fixtures::{TestMemory, create_core_db};
use uuid::Uuid;

#[tokio::test]
async fn citations_of_facts_key_each_citing_fact_by_blob_id_and_schema() {
    let db_name = format!("proxiima_test_{}", Uuid::now_v7().simple());
    if let Err(e) = create_core_db(&db_name).await {
        panic!("PG required for tests but admin connect failed: {e}");
    }
    let url = db_url(&db_name);
    let result: Result<(), Box<dyn std::error::Error>> = async {
        let pg = PgStorage::connect_for_tests(url.clone()).await?;
        let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
        let uncited = TestMemory::fact().create(&pg, owner).await?;
        // The citing Fact's admission records the object, and names its id.
        let written = TestMemory::fact()
            .cites("core/bytes-v1", [9u8; 32])
            .create(&pg, owner)
            .await?;
        let blob_id = written.cited_object_id.ok_or("cited object missing")?;

        let owner_scope = proxiima_core::test_fixtures::owner_scope_for_tests(&[owner]);
        let citations = pg
            .citations_of_facts(
                Some(&owner_scope),
                &[owner],
                &[uncited.memory_id, written.memory_id],
            )
            .await?;
        assert_eq!(citations.len(), 1, "only the citing Fact has an entry");
        let readback = &citations[&written.memory_id];
        assert_eq!(readback.cited_object_id, blob_id);
        assert_eq!(readback.cited_object_schema_id.as_str(), "core/bytes-v1");
        Ok(())
    }
    .await;
    let _ = drop_db(&db_name).await;
    result.expect("citations_of_facts blob projection failed");
}
