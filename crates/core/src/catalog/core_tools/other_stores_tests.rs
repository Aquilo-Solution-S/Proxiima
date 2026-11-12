use super::*;
use crate::UserId;
use crate::catalog::core_tools::memory::derive::MAX_SOURCE_HANDLES;
use crate::catalog::core_tools::memory_spaces::test_ctx::ctx_for;
use crate::storage_ports::other_stores::{
    OtherStores, RemoteEdgeTarget, RemoteHit, RemoteLineageEdge, RemoteLineageNode, RemoteRead,
    StoreInfo,
};
use futures::future::BoxFuture;

#[test]
fn a_search_response_must_carry_the_requested_store_number() {
    let hit = RemoteHit {
        handle: crate::new_memory_uuid(2),
        owner: crate::OwnerId::new(uuid::Uuid::from_u128(2)),
        writer: crate::WriterAttribution {
            by: uuid::Uuid::from_u128(3),
            via: None,
        },
        kind: crate::EntityKind::Fact,
        schema_id: "remote/note".into(),
        snippet: "remote result".into(),
        created_at: time::OffsetDateTime::UNIX_EPOCH,
        tags: Vec::new(),
        citation: None,
    };
    let reply = RemoteSearchResult {
        hits: vec![hit],
        degraded_to_lexical: true,
    };
    assert_eq!(
        checked_remote_hits(2, reply.clone()).expect("same store"),
        reply
    );
    assert!(matches!(
        checked_remote_hits(3, reply),
        Err(OtherStoresError::Unavailable(_))
    ));
}
enum InventoryReply {
    Stores(Vec<u16>),
    Unavailable,
    Pending,
}

struct InventoryPort {
    reply: InventoryReply,
    listings: std::sync::atomic::AtomicUsize,
    searches: std::sync::Mutex<Vec<u16>>,
}

impl OtherStores for InventoryPort {
    fn stores<'a>(
        &'a self,
        _caller: StoreCaller<'a>,
    ) -> BoxFuture<'a, Result<Vec<StoreInfo>, OtherStoresError>> {
        Box::pin(async move {
            self.listings
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            match &self.reply {
                InventoryReply::Stores(numbers) => Ok(numbers
                    .iter()
                    .enumerate()
                    .map(|(index, number)| StoreInfo {
                        number: *number,
                        name: format!("Store {index}"),
                        description: String::new(),
                        schema_ids: vec![format!("store/{number}")],
                    })
                    .collect()),
                InventoryReply::Unavailable => Err(OtherStoresError::Unavailable("offline".into())),
                InventoryReply::Pending => futures::future::pending().await,
            }
        })
    }

    fn search<'a>(
        &'a self,
        _caller: StoreCaller<'a>,
        store: u16,
        _args: &'a RemoteSearch,
    ) -> BoxFuture<'a, Result<RemoteSearchResult, OtherStoresError>> {
        Box::pin(async move {
            self.searches.lock().expect("search calls").push(store);
            Ok(RemoteSearchResult {
                hits: Vec::new(),
                degraded_to_lexical: false,
            })
        })
    }

    fn read<'a>(
        &'a self,
        _caller: StoreCaller<'a>,
        _id: uuid::Uuid,
        _at: crate::storage_ports::other_stores::ReadAt,
    ) -> BoxFuture<'a, Result<RemoteRead, OtherStoresError>> {
        Box::pin(async { Ok(RemoteRead::NotFound) })
    }
}

fn inventory_ctx(reply: InventoryReply) -> (ToolCtx, Arc<InventoryPort>) {
    let port = Arc::new(InventoryPort {
        reply,
        listings: std::sync::atomic::AtomicUsize::new(0),
        searches: std::sync::Mutex::new(Vec::new()),
    });
    let mut ctx = ctx_for(UserId::new(uuid::Uuid::now_v7()), Vec::new());
    let engine = Some(Arc::new(crate::Engine::new(
        ctx.registry().as_ref().clone(),
        crate::StoragePorts::rejecting_for_tests(),
    )));
    ctx = ctx.with_engine(engine);
    ctx.try_insert_service(OtherStoresService::new(port.clone()))
        .expect("the fixture context holds no service");
    (ctx, port)
}

#[tokio::test]
async fn invalid_inventories_refuse_search_and_metadata_without_fanout() {
    use crate::Tool;
    use crate::catalog::core_tools::search_memories::{
        SearchMemoriesArgs, SearchMemoriesTool, caller_input_schema,
    };

    for (numbers, expected) in [
        (vec![1, 0], OtherStoresError::LocalStoreInInventory(0)),
        (
            vec![1, 2, 1],
            OtherStoresError::DuplicateStoreInInventory(1),
        ),
    ] {
        let (ctx, port) = inventory_ctx(InventoryReply::Stores(numbers));
        assert_eq!(
            service(&ctx)
                .expect("wired port")
                .stores_bounded(caller(&ctx), 0)
                .await,
            Err(expected.clone()),
        );
        assert_eq!(
            caller_input_schema(&ctx, serde_json::json!({"type": "object"})).await,
            Err(expected.clone()),
        );
        for (stores, schema_id) in [
            (None, None),
            (Some(vec![0]), None),
            (Some(vec![1]), None),
            (None, Some("store/1")),
        ] {
            let args: SearchMemoriesArgs = serde_json::from_value(serde_json::json!({
                "query": "needle", "mode": "lexical", "stores": stores, "schema_id": schema_id,
            }))
            .expect("valid search arguments");
            let error = SearchMemoriesTool::call(ctx.clone(), args)
                .await
                .expect_err("an invalid inventory must refuse before searching");
            assert!(
                matches!(error, ToolError::Other(ref message) if message == &expected.to_string()),
                "{error:?}"
            );
        }
        assert_eq!(port.listings.load(std::sync::atomic::Ordering::Relaxed), 6);
        assert!(port.searches.lock().expect("search calls").is_empty());
    }
}

#[tokio::test]
async fn valid_inventories_route_each_selected_remote_once_in_request_order() {
    for (requested, schema_id, expected) in [
        (
            None,
            None,
            StoreRoute {
                local: true,
                remotes: vec![1, 2],
            },
        ),
        (
            None,
            Some("store/2"),
            StoreRoute {
                local: false,
                remotes: vec![2],
            },
        ),
        (
            Some(vec![2, 1, 2, 0]),
            None,
            StoreRoute {
                local: true,
                remotes: vec![2, 1],
            },
        ),
        (
            Some(vec![0]),
            None,
            StoreRoute {
                local: true,
                remotes: Vec::new(),
            },
        ),
    ] {
        let (ctx, port) = inventory_ctx(InventoryReply::Stores(vec![1, 2]));
        let routed = route_search(&ctx, requested.as_deref(), schema_id)
            .await
            .expect("valid route");
        assert_eq!(routed.route, expected);
        let args = RemoteSearch {
            query: "needle".into(),
            limit: 8,
            kind: None,
            schema_id: schema_id.map(str::to_owned),
            tags: Vec::new(),
            tag_match: crate::verbs::query::TagMatch::Any,
            mode: crate::verbs::query::SearchMode::Lexical,
            since: None,
            until: None,
            order: crate::verbs::query::SearchOrder::Relevance,
            supersession: crate::verbs::query::SupersessionStatus::HeadsOnly,
            min_score: None,
            semantic_weight: None,
        };
        let (answered, unanswered) = search_remotes(
            &ctx,
            routed.service.as_deref().expect("wired port"),
            &routed.route.remotes,
            &args,
        )
        .await;
        assert_eq!(
            answered
                .iter()
                .map(|(number, _)| *number)
                .collect::<Vec<_>>(),
            expected.remotes
        );
        assert!(unanswered.is_empty());
        assert_eq!(
            *port.searches.lock().expect("search calls"),
            expected.remotes
        );
        assert_eq!(port.listings.load(std::sync::atomic::Ordering::Relaxed), 1);
    }
}

/// Acceptance 1 of #480: a store list that failed or timed out is not
/// "there are no other stores". A search that names no store still runs
/// locally but says the inventory went unanswered; one that names a
/// remote store is a retryable `StoreUnavailable` (not "unknown store");
/// one that names only the local store needed no inventory. The tool
/// listing keeps its static schema.
#[tokio::test(start_paused = true)]
async fn unavailable_inventory_is_reported_and_refuses_named_stores() {
    for reply in [InventoryReply::Unavailable, InventoryReply::Pending] {
        let (ctx, port) = inventory_ctx(reply);
        let schema = serde_json::json!({"type": "object", "properties": {}});
        assert_eq!(
            super::super::search_memories::caller_input_schema(&ctx, schema.clone())
                .await
                .expect("unavailable inventory"),
            schema,
        );
        let routed = route_search(&ctx, None, None).await.expect("local route");
        assert_eq!(
            routed.route,
            StoreRoute {
                local: true,
                remotes: Vec::new()
            }
        );
        assert!(routed.inventory_unanswered);
        let error = route_search(&ctx, Some(&[0, 2, 1]), None)
            .await
            .err()
            .expect("a named remote store");
        assert!(
            matches!(error, ToolError::StoreUnavailable { store: 2 }),
            "{error:?}"
        );
        let local_only = route_search(&ctx, Some(&[0]), None)
            .await
            .expect("local only");
        assert_eq!(
            local_only.route,
            StoreRoute {
                local: true,
                remotes: Vec::new()
            }
        );
        assert!(!local_only.inventory_unanswered);
        assert_eq!(port.listings.load(std::sync::atomic::Ordering::Relaxed), 4);
        assert!(port.searches.lock().expect("search calls").is_empty());
    }
}

struct SlowOriginStore;

impl OtherStores for SlowOriginStore {
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
        id: uuid::Uuid,
        _at: crate::storage_ports::other_stores::ReadAt,
    ) -> BoxFuture<'a, Result<RemoteRead, OtherStoresError>> {
        Box::pin(async move {
            tokio::time::sleep(std::time::Duration::from_millis(1_900)).await;
            Ok(RemoteRead::Found(Box::new(RemoteMemory::fixture(id))))
        })
    }
}

#[tokio::test(start_paused = true)]
async fn derive_origin_admission_refuses_255_slow_origins_before_writing() {
    let mut ctx = ctx_for(UserId::new(uuid::Uuid::now_v7()), Vec::new());
    ctx.try_insert_service(OtherStoresService::new(Arc::new(SlowOriginStore)))
        .expect("the fixture context holds no service");
    let engine = Some(Arc::new(crate::Engine::new(
        ctx.registry().as_ref().clone(),
        crate::StoragePorts::rejecting_for_tests(),
    )));
    ctx = ctx.with_engine(engine);
    let origins = (0..MAX_SOURCE_HANDLES - 1)
        .map(|_| crate::new_memory_uuid(1).to_string())
        .collect::<Vec<_>>();
    let started = tokio::time::Instant::now();
    let deadline = crate::storage_ports::other_stores::DEFAULT_REMOTE_BATCH_DEADLINE;
    let error = tokio::time::timeout(
        deadline + std::time::Duration::from_millis(1),
        resolve_references(&ctx, &origins),
    )
    .await
    .expect("derive admission must finish within the call deadline")
    .expect_err("deadline must refuse derivation");
    assert!(started.elapsed() <= deadline);
    assert!(
        matches!(error, ToolError::StoreUnavailable { store: 1 }),
        "{error:?}"
    );
}

#[derive(Clone, Copy)]
enum ReadReply {
    Pending,
    Unavailable,
    Mismatched,
    NotFound,
    Erased,
}

struct ReadReplyStore(ReadReply);

impl OtherStores for ReadReplyStore {
    fn stores<'a>(
        &'a self,
        _caller: StoreCaller<'a>,
    ) -> BoxFuture<'a, Result<Vec<StoreInfo>, OtherStoresError>> {
        Box::pin(async { Ok(vec![]) })
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
        _id: uuid::Uuid,
        _at: crate::storage_ports::other_stores::ReadAt,
    ) -> BoxFuture<'a, Result<RemoteRead, OtherStoresError>> {
        let reply = self.0;
        Box::pin(async move {
            match reply {
                ReadReply::Pending => futures::future::pending().await,
                ReadReply::Unavailable => {
                    Err(OtherStoresError::Unavailable("remote internals".into()))
                }
                ReadReply::Mismatched => Ok(RemoteRead::Found(Box::new(RemoteMemory::fixture(
                    crate::new_memory_uuid(1),
                )))),
                ReadReply::NotFound => Ok(RemoteRead::NotFound),
                ReadReply::Erased => Ok(RemoteRead::Erased),
            }
        })
    }
}

fn read_reply_ctx(reply: ReadReply) -> ToolCtx {
    let mut ctx = ctx_for(UserId::new(uuid::Uuid::now_v7()), Vec::new());
    ctx.try_insert_service(OtherStoresService::new(Arc::new(ReadReplyStore(reply))))
        .expect("the fixture context holds no service");
    let engine = Some(Arc::new(crate::Engine::new(
        ctx.registry().as_ref().clone(),
        crate::StoragePorts::rejecting_for_tests(),
    )));
    ctx = ctx.with_engine(engine);
    ctx
}

/// Acceptance 2 of #480: a remote source that times out, fails, or answers
/// for another memory is a retryable `StoreUnavailable` naming its store,
/// on both the batch path (derive sources, interpret subjects) and the
/// single read (`get_memory`). A store that answers "no such memory" (or
/// erased) stays `NotFound`, so nothing about existence leaks.
#[tokio::test(start_paused = true)]
async fn an_unanswered_remote_read_is_store_unavailable_and_an_answer_is_not_found() {
    for (reply, unavailable) in [
        (ReadReply::Pending, true),
        (ReadReply::Unavailable, true),
        (ReadReply::Mismatched, true),
        (ReadReply::NotFound, false),
        (ReadReply::Erased, false),
    ] {
        let ctx = read_reply_ctx(reply);
        let id = crate::new_memory_uuid(1);
        let batch = resolve_references(&ctx, &[id.to_string()])
            .await
            .expect_err("no remote origin is readable");
        let single = read_origin(&ctx, id)
            .await
            .expect_err("no remote memory is readable");
        for error in [batch, single] {
            if unavailable {
                assert!(
                    matches!(error, ToolError::StoreUnavailable { store: 1 }),
                    "{error:?}"
                );
                assert_eq!(
                    error.kind(),
                    crate::catalog::ToolErrorKind::StoreUnavailable
                );
                assert!(!error.client_message().contains("internals"));
            } else {
                assert!(matches!(error, ToolError::NotFound(_)), "{error:?}");
            }
        }
    }
}

/// How the owning store answers the graph reads of [`GraphStore`].
#[derive(Clone, Copy)]
enum GraphAnswer {
    Serves,
    RefusesViewer,
    Down,
}

/// Store 1 holds `memory`, a Fact `citing` that refers to it, and the
/// `ancestor` it was made from.
struct GraphStore {
    answer: GraphAnswer,
    memory: uuid::Uuid,
    citing: uuid::Uuid,
    ancestor: uuid::Uuid,
    cursors: std::sync::Mutex<Vec<Option<String>>>,
}

impl GraphStore {
    fn new(answer: GraphAnswer) -> Self {
        Self {
            answer,
            memory: crate::new_memory_uuid(1),
            citing: crate::new_memory_uuid(1),
            ancestor: crate::new_memory_uuid(1),
            cursors: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn admit<T>(&self, id: uuid::Uuid, found: T) -> Result<RemoteGraphRead<T>, OtherStoresError> {
        match self.answer {
            GraphAnswer::Serves if id == self.memory => Ok(RemoteGraphRead::Found(found)),
            GraphAnswer::Serves | GraphAnswer::RefusesViewer => Ok(RemoteGraphRead::NotFound),
            GraphAnswer::Down => Err(OtherStoresError::Unavailable("remote internals".into())),
        }
    }

    fn node(handle: uuid::Uuid, distance: u8) -> RemoteLineageNode {
        RemoteLineageNode {
            handle,
            owner: Some(crate::OwnerId::new(uuid::Uuid::from_u128(2))),
            kind: "fact".into(),
            schema_id: "test/fact-v1".into(),
            snippet: String::new(),
            distance,
            status: None,
            unavailable_reason: None,
        }
    }
}

impl OtherStores for GraphStore {
    fn stores<'a>(
        &'a self,
        _caller: StoreCaller<'a>,
    ) -> BoxFuture<'a, Result<Vec<StoreInfo>, OtherStoresError>> {
        Box::pin(async { Ok(vec![]) })
    }

    fn search<'a>(
        &'a self,
        _caller: StoreCaller<'a>,
        _store: u16,
        _args: &'a RemoteSearch,
    ) -> BoxFuture<'a, Result<RemoteSearchResult, OtherStoresError>> {
        Box::pin(async { Err(OtherStoresError::UnsupportedSearch("not searched".into())) })
    }

    fn read<'a>(
        &'a self,
        _caller: StoreCaller<'a>,
        id: uuid::Uuid,
        _at: ReadAt,
    ) -> BoxFuture<'a, Result<RemoteRead, OtherStoresError>> {
        let read = if id == self.memory {
            RemoteRead::Found(Box::new(RemoteMemory::fixture(id)))
        } else {
            RemoteRead::NotFound
        };
        Box::pin(async move { Ok(read) })
    }

    fn neighbor_edges<'a>(
        &'a self,
        _caller: StoreCaller<'a>,
        id: uuid::Uuid,
    ) -> BoxFuture<'a, Result<RemoteGraphRead<Vec<RemoteEdge>>, OtherStoresError>> {
        let edges = vec![RemoteEdge {
            source: self.citing,
            target: RemoteEdgeTarget::Visible(self.memory),
            kind: crate::EdgeKind::Reference,
        }];
        Box::pin(async move { self.admit(id, edges) })
    }

    fn ancestors<'a>(
        &'a self,
        _caller: StoreCaller<'a>,
        id: uuid::Uuid,
        page: &'a RemoteAncestorsPage,
    ) -> BoxFuture<'a, Result<RemoteGraphRead<RemoteAncestors>, OtherStoresError>> {
        self.cursors
            .lock()
            .expect("cursor log")
            .push(page.cursor.clone());
        let ancestors = RemoteAncestors {
            nodes: vec![Self::node(self.memory, 0), Self::node(self.ancestor, 1)],
            edges: vec![RemoteLineageEdge {
                source: self.memory,
                target: RemoteEdgeTarget::Visible(self.ancestor),
                kind: crate::EdgeKind::Origin,
                distance: 1,
            }],
            next_cursor: Some("owning store cursor".into()),
        };
        Box::pin(async move { self.admit(id, ancestors) })
    }
}

fn graph_ctx(store: &Arc<GraphStore>) -> ToolCtx {
    let mut ctx = ctx_for(UserId::new(uuid::Uuid::now_v7()), Vec::new());
    ctx.try_insert_service(OtherStoresService::new(store.clone()))
        .expect("the fixture context holds no service");
    let engine = Some(Arc::new(crate::Engine::new(
        ctx.registry().as_ref().clone(),
        crate::StoragePorts::rejecting_for_tests(),
    )));
    ctx = ctx.with_engine(engine);
    ctx
}

async fn read_graph(
    ctx: &ToolCtx,
    memory: uuid::Uuid,
    cursor: Option<String>,
) -> Result<
    (
        super::super::get_memory::GetMemoryOutput,
        super::super::walk_memory_lineage::WalkMemoryLineageOutput,
    ),
    ToolError,
> {
    use super::super::{get_memory, walk_memory_lineage};
    let read = get_memory::get_memory(
        ctx.clone(),
        get_memory::GetMemoryArgs {
            memory: memory.to_string(),
            expand_neighbors: true,
            space: None,
            version: get_memory::VersionSelector::Exact,
        },
    )
    .await?;
    let walk = walk_memory_lineage::walk_memory_lineage(
        ctx.clone(),
        walk_memory_lineage::WalkMemoryLineageArgs {
            memory: memory.to_string(),
            direction: walk_memory_lineage::WalkMemoryLineageDirectionArg::Ancestors,
            depth: 3,
            limit: 50,
            cursor,
        },
    )
    .await?;
    Ok((read, walk))
}

/// A memory of another store answers `expand_neighbors` and the
/// ancestors walk through its store, as that store admits the caller:
/// a refused viewer is not found, and a store that is down is refused
/// as unavailable, never an empty list or page.
#[tokio::test]
async fn a_remote_memory_serves_its_neighbor_edges_and_ancestors_through_its_store() {
    let store = Arc::new(GraphStore::new(GraphAnswer::Serves));
    let ctx = graph_ctx(&store);
    let (read, walk) = read_graph(&ctx, store.memory, None)
        .await
        .expect("graph reads");
    let edges = read.neighbor_edges.expect("neighbor edges");
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].source, store.citing.to_string());
    assert_eq!(edges[0].target, store.memory.to_string());
    assert_eq!(edges[0].kind, crate::EdgeKind::Reference);

    let ancestor = walk
        .nodes
        .iter()
        .find(|node| node.memory == store.ancestor.to_string())
        .expect("ancestor node");
    assert_eq!(ancestor.distance, 1);
    assert_eq!(ancestor.store, Some(1));
    assert_eq!(ancestor.space, Some(uuid::Uuid::from_u128(2).to_string()));
    assert_eq!(walk.edges.len(), 1);
    assert_eq!(walk.edges[0].source, store.memory.to_string());
    assert_eq!(walk.edges[0].target, store.ancestor.to_string());
    assert_eq!(walk.edges[0].kind, crate::EdgeKind::Origin);
    assert!(walk.has_more);
    let next = walk.next_cursor.expect("next page");
    read_graph(&ctx, store.memory, Some(next))
        .await
        .expect("next page");
    assert_eq!(
        *store.cursors.lock().expect("cursor log"),
        [None, Some("owning store cursor".to_string())]
    );

    for (answer, unavailable) in [
        (GraphAnswer::RefusesViewer, false),
        (GraphAnswer::Down, true),
    ] {
        let store = Arc::new(GraphStore::new(answer));
        let ctx = graph_ctx(&store);
        let error = read_graph(&ctx, store.memory, None)
            .await
            .expect_err("no graph crosses");
        if unavailable {
            assert!(
                matches!(error, ToolError::StoreUnavailable { store: 1 }),
                "{error:?}"
            );
        } else {
            assert!(matches!(error, ToolError::NotFound(_)), "{error:?}");
        }
        let walk = super::super::walk_memory_lineage::walk_memory_lineage(
            ctx.clone(),
            super::super::walk_memory_lineage::WalkMemoryLineageArgs {
                memory: store.memory.to_string(),
                direction:
                    super::super::walk_memory_lineage::WalkMemoryLineageDirectionArg::Ancestors,
                depth: 3,
                limit: 50,
                cursor: None,
            },
        )
        .await
        .expect_err("no ancestors cross");
        assert_eq!(
            walk.kind(),
            if unavailable {
                crate::catalog::ToolErrorKind::StoreUnavailable
            } else {
                crate::catalog::ToolErrorKind::NotFound
            }
        );
    }
}
