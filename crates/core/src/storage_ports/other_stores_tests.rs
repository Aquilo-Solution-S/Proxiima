use super::*;

fn info(number: u16, name: &str, schemas: &[&str]) -> StoreInfo {
    StoreInfo {
        number,
        name: name.into(),
        description: String::new(),
        schema_ids: schemas.iter().map(ToString::to_string).collect(),
    }
}

#[test]
fn a_schema_declared_by_one_store_routes_there_only() {
    let stores = StoreInventory::new(
        3,
        vec![
            info(5, "wiki", &["wiki/page"]),
            info(7, "crm", &["crm/contact", "shared"]),
        ],
    )
    .expect("valid inventory");
    let local = |id: &str| matches!(id, "core/note" | "shared");
    let only = route_stores(None, Some("wiki/page"), local, &stores).expect("route");
    assert_eq!(
        only,
        StoreRoute {
            local: false,
            remotes: vec![5]
        }
    );

    let local_only = route_stores(None, Some("core/note"), local, &stores).expect("route");
    assert_eq!(
        local_only,
        StoreRoute {
            local: true,
            remotes: Vec::new()
        }
    );

    for schema in ["shared", "nobody/has"] {
        let all = route_stores(None, Some(schema), local, &stores).expect("route");
        assert!(all.local);
        assert_eq!(all.remotes, [5, 7]);
    }
    let shared = StoreInventory::new(
        3,
        vec![info(5, "wiki", &["shared"]), info(7, "crm", &["shared"])],
    )
    .expect("valid inventory");
    let all = route_stores(None, Some("shared"), |_| false, &shared).expect("route");
    assert!(all.local);
    assert_eq!(all.remotes, [5, 7]);

    let explicit =
        route_stores(Some(&[3, 7, 7]), Some("wiki/page"), local, &stores).expect("route");
    assert_eq!(
        explicit,
        StoreRoute {
            local: true,
            remotes: vec![7]
        }
    );

    let error = route_stores(Some(&[9]), None, local, &stores).expect_err("unknown store");
    assert!(error.contains('9'));
    assert!(route_stores(Some(&[]), None, local, &stores).is_err());
}

#[test]
fn inventories_refuse_local_and_duplicate_store_numbers() {
    assert_eq!(
        StoreInventory::new(3, vec![info(5, "remote", &[]), info(3, "local", &[])]),
        Err(OtherStoresError::LocalStoreInInventory(3))
    );
    assert_eq!(
        StoreInventory::new(3, vec![info(5, "first", &[]), info(5, "second", &[])]),
        Err(OtherStoresError::DuplicateStoreInInventory(5))
    );
}

#[test]
fn changing_display_names_does_not_change_routes() {
    let before =
        StoreInventory::new(3, vec![info(5, "wiki", &["wiki/page"])]).expect("valid inventory");
    let after = StoreInventory::new(3, vec![info(5, "knowledge", &["wiki/page"])])
        .expect("valid inventory");
    for requested in [None, Some([5].as_slice())] {
        assert_eq!(
            route_stores(requested, Some("wiki/page"), |_| false, &before),
            route_stores(requested, Some("wiki/page"), |_| false, &after),
        );
    }
}

#[derive(Debug)]
struct WrongMemoryStore;

impl OtherStores for WrongMemoryStore {
    fn stores<'a>(
        &'a self,
        _caller: StoreCaller<'a>,
    ) -> BoxFuture<'a, Result<Vec<StoreInfo>, OtherStoresError>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn search<'a>(
        &'a self,
        _caller: StoreCaller<'a>,
        _store: u16,
        _args: &'a RemoteSearch,
    ) -> BoxFuture<'a, Result<RemoteSearchResult, OtherStoresError>> {
        Box::pin(async {
            Ok(RemoteSearchResult {
                hits: Vec::new(),
                degraded_to_lexical: false,
            })
        })
    }

    fn read<'a>(
        &'a self,
        _caller: StoreCaller<'a>,
        _id: Uuid,
        _at: ReadAt,
    ) -> BoxFuture<'a, Result<RemoteRead, OtherStoresError>> {
        Box::pin(async {
            Ok(RemoteRead::Found(Box::new(RemoteMemory {
                body: "different memory".into(),
                ..RemoteMemory::fixture(Uuid::from_u128(2))
            })))
        })
    }

    fn neighbor_edges<'a>(
        &'a self,
        _caller: StoreCaller<'a>,
        _id: Uuid,
    ) -> BoxFuture<'a, Result<RemoteGraphRead<Vec<RemoteEdge>>, OtherStoresError>> {
        Box::pin(async {
            Ok(RemoteGraphRead::Found(vec![RemoteEdge {
                source: Uuid::from_u128(2),
                target: RemoteEdgeTarget::Visible(Uuid::from_u128(3)),
                kind: EdgeKind::Reference,
            }]))
        })
    }

    fn ancestors<'a>(
        &'a self,
        _caller: StoreCaller<'a>,
        _id: Uuid,
        _page: &'a RemoteAncestorsPage,
    ) -> BoxFuture<'a, Result<RemoteGraphRead<RemoteAncestors>, OtherStoresError>> {
        Box::pin(async {
            Ok(RemoteGraphRead::Found(RemoteAncestors {
                nodes: vec![RemoteLineageNode {
                    handle: Uuid::from_u128(2),
                    owner: None,
                    kind: "fact".into(),
                    schema_id: "test/fact-v1".into(),
                    snippet: String::new(),
                    distance: 0,
                    status: None,
                    unavailable_reason: None,
                }],
                edges: Vec::new(),
                next_cursor: None,
            }))
        })
    }
}

#[tokio::test]
async fn a_port_cannot_answer_a_graph_read_with_another_memorys_graph() {
    let owner = crate::UserId::new(Uuid::nil());
    let authz = AuthzContext::for_subject(owner, crate::AuthPath::System);
    let caller = || StoreCaller {
        owner: owner.into(),
        authz: &authz,
    };
    let service = OtherStoresService::new(Arc::new(WrongMemoryStore));
    let requested = Uuid::from_u128(1);
    assert!(matches!(
        service.neighbor_edges_bounded(caller(), requested).await,
        Err(OtherStoresError::MismatchedMemory { .. })
    ));
    let page = RemoteAncestorsPage {
        depth: 3,
        limit: 50,
        cursor: None,
    };
    assert!(matches!(
        service.ancestors_bounded(caller(), requested, &page).await,
        Err(OtherStoresError::MismatchedMemory { .. })
    ));
}

#[tokio::test]
async fn a_port_cannot_bind_another_memorys_kind_to_the_requested_origin() {
    let owner = crate::UserId::new(Uuid::nil());
    let authz = AuthzContext::for_subject(owner, crate::AuthPath::System);
    let service = OtherStoresService::new(Arc::new(WrongMemoryStore));
    let requested = Uuid::from_u128(1);
    let error = service
        .read_origin(
            StoreCaller {
                owner: owner.into(),
                authz: &authz,
            },
            requested,
        )
        .await
        .expect_err("a mismatched memory cannot become an origin");
    assert_eq!(
        error,
        OtherStoresError::MismatchedMemory {
            requested,
            returned: Uuid::from_u128(2),
        }
    );
    let reads = service
        .read_origins_bounded(
            StoreCaller {
                owner: owner.into(),
                authz: &authz,
            },
            &[requested],
        )
        .await;
    assert!(matches!(
        &reads[0],
        Err(OtherStoresError::MismatchedMemory { requested: actual, returned })
            if *actual == requested && *returned == Uuid::from_u128(2)
    ));
}

#[tokio::test]
async fn a_head_read_may_answer_with_another_version_of_the_requested_store_only() {
    let owner = crate::UserId::new(Uuid::nil());
    let authz = AuthzContext::for_subject(owner, crate::AuthPath::System);
    let caller = || StoreCaller {
        owner: owner.into(),
        authz: &authz,
    };
    let service = OtherStoresService::new(Arc::new(WrongMemoryStore));
    // The store answers with `from_u128(2)`, a version it issued.
    let same_store = Uuid::from_u128(1);
    let head = service
        .read_bounded(caller(), same_store, ReadAt::SeriesHead)
        .await
        .expect("a head read names the version it returns");
    assert!(matches!(head, RemoteRead::Found(memory) if memory.handle == Uuid::from_u128(2)));
    let exact = service
        .read_bounded(caller(), same_store, ReadAt::Version)
        .await;
    assert!(matches!(
        exact,
        Err(OtherStoresError::MismatchedMemory { .. })
    ));
    // An answer issued by a different store is not the head of this series.
    let other_store = crate::new_memory_uuid(1);
    let foreign = service
        .read_bounded(caller(), other_store, ReadAt::SeriesHead)
        .await;
    assert!(matches!(
        foreign,
        Err(OtherStoresError::MismatchedMemory { .. })
    ));
}

#[test]
fn rrf_interleaves_lists_by_rank() {
    let merged = rrf_merge(vec![vec!["l1", "l2", "l3"], vec!["r1"], vec!["s1", "s2"]]);
    let order: Vec<_> = merged.iter().map(|(item, _)| *item).collect();
    assert_eq!(order, ["l1", "r1", "s1", "l2", "s2", "l3"]);
    assert!((merged[0].1 - 1.0 / 61.0).abs() < f32::EPSILON);
    assert!(merged.windows(2).all(|w| w[0].1 >= w[1].1));
}
struct SlowOrigins {
    delay: Duration,
    active: std::sync::atomic::AtomicUsize,
    peak: std::sync::atomic::AtomicUsize,
}

impl SlowOrigins {
    fn new(delay: Duration) -> Self {
        Self {
            delay,
            active: std::sync::atomic::AtomicUsize::new(0),
            peak: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

struct ActiveRead<'a>(&'a std::sync::atomic::AtomicUsize);

impl Drop for ActiveRead<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

impl OtherStores for SlowOrigins {
    fn stores<'a>(
        &'a self,
        _caller: StoreCaller<'a>,
    ) -> BoxFuture<'a, Result<Vec<StoreInfo>, OtherStoresError>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn search<'a>(
        &'a self,
        _caller: StoreCaller<'a>,
        _store: u16,
        _args: &'a RemoteSearch,
    ) -> BoxFuture<'a, Result<RemoteSearchResult, OtherStoresError>> {
        Box::pin(async {
            Ok(RemoteSearchResult {
                hits: Vec::new(),
                degraded_to_lexical: false,
            })
        })
    }

    fn read<'a>(
        &'a self,
        _caller: StoreCaller<'a>,
        _id: Uuid,
        _at: ReadAt,
    ) -> BoxFuture<'a, Result<RemoteRead, OtherStoresError>> {
        Box::pin(async {
            use std::sync::atomic::Ordering::SeqCst;
            let active = self.active.fetch_add(1, SeqCst) + 1;
            self.peak.fetch_max(active, SeqCst);
            let _active_read = ActiveRead(&self.active);
            tokio::time::sleep(self.delay).await;
            Ok(RemoteRead::NotFound)
        })
    }
}

#[tokio::test(start_paused = true)]
async fn origin_batch_deadline_bounds_255_slow_foreign_origins() {
    let owner = OwnerId::new(uuid::Uuid::now_v7());
    let authz = crate::test_fixtures::no_access_context();
    let caller = StoreCaller {
        owner,
        authz: &authz,
    };
    let handles = (0..255)
        .map(|_| crate::new_memory_uuid(1))
        .collect::<Vec<_>>();
    let port = Arc::new(SlowOrigins::new(Duration::from_millis(1_900)));
    let service = OtherStoresService::new(port.clone());
    let started = tokio::time::Instant::now();
    let reads = tokio::time::timeout(
        DEFAULT_REMOTE_BATCH_DEADLINE + Duration::from_millis(1),
        service.read_many_bounded(caller, &handles),
    )
    .await
    .expect("call-wide deadline bounds the batch");
    assert_eq!(started.elapsed(), DEFAULT_REMOTE_BATCH_DEADLINE);
    assert_eq!(port.peak.load(std::sync::atomic::Ordering::SeqCst), 8);
    assert_eq!(port.active.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(reads.len(), 255);
    assert!(
        reads
            .iter()
            .any(|read| matches!(read, Err(OtherStoresError::DeadlineExceeded)))
    );
}
#[tokio::test(start_paused = true)]
async fn origin_batch_keeps_two_second_per_read_timeout() {
    let owner = OwnerId::new(uuid::Uuid::now_v7());
    let authz = crate::test_fixtures::no_access_context();
    let caller = StoreCaller {
        owner,
        authz: &authz,
    };
    let service = OtherStoresService::new(Arc::new(SlowOrigins::new(Duration::from_secs(3))));
    let handles = [crate::new_memory_uuid(1)];
    let started = tokio::time::Instant::now();
    let reads = service.read_many_bounded(caller, &handles).await;
    assert_eq!(started.elapsed(), Duration::from_secs(2));
    assert!(
        matches!(&reads[0], Err(OtherStoresError::Unavailable(reason)) if reason == "timed out")
    );
}

#[tokio::test(start_paused = true)]
async fn origin_batch_reads_different_stores_concurrently() {
    let owner = OwnerId::new(uuid::Uuid::now_v7());
    let authz = crate::test_fixtures::no_access_context();
    let caller = StoreCaller {
        owner,
        authz: &authz,
    };
    let handles = [1, 2]
        .into_iter()
        .flat_map(|store| (0..8).map(move |_| crate::new_memory_uuid(store)))
        .collect::<Vec<_>>();
    let port = Arc::new(SlowOrigins::new(Duration::from_millis(1_900)));
    let service = OtherStoresService::new(port.clone());
    let started = tokio::time::Instant::now();
    let reads = service.read_many_bounded(caller, &handles).await;
    assert_eq!(started.elapsed(), Duration::from_millis(1_900));
    assert_eq!(port.peak.load(std::sync::atomic::Ordering::SeqCst), 16);
    assert!(
        reads
            .iter()
            .all(|read| matches!(read, Ok(RemoteRead::NotFound)))
    );
}
