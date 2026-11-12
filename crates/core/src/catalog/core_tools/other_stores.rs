//! Wire-side numbered store routing, search fan-out, and remote reads.

use std::sync::Arc;

use crate::catalog::MemoryHandleClass;
use crate::storage_ports::other_stores::{
    OtherStoresError, OtherStoresService, ReadAt, ReadOrigin, RemoteAncestors, RemoteAncestorsPage,
    RemoteEdge, RemoteGraphRead, RemoteMemory, RemoteOriginRead, RemoteRead, RemoteSearch,
    RemoteSearchResult, StoreCaller, StoreInventory, StoreRoute, route_stores,
};
use crate::{ToolCtx, ToolError};

pub(crate) fn service(ctx: &ToolCtx) -> Option<Arc<OtherStoresService>> {
    ctx.service::<OtherStoresService>()
}

pub(crate) fn caller(ctx: &ToolCtx) -> StoreCaller<'_> {
    StoreCaller {
        owner: ctx.owner(),
        authz: ctx.authz(),
    }
}

fn not_found(id: uuid::Uuid) -> ToolError {
    ToolError::NotFound(format!("memory {id} not found"))
}

/// THE mapping of a failed remote read onto the wire (#480).
///
/// An unknown store is `NotFound` (no existence leak, and the same answer as
/// "no other store wired"). Every other failure (unavailable, past the
/// deadline, an answer for a different memory, or anything else) says
/// nothing about whether the memory exists, so it is a retryable
/// [`ToolError::StoreUnavailable`] naming the store, never "not found".
/// An answer of "not found" or "erased" is not an error and never gets here.
pub(crate) fn remote_read_error(id: uuid::Uuid, error: &OtherStoresError) -> ToolError {
    match error {
        OtherStoresError::UnknownStore(_) => not_found(id),
        OtherStoresError::Unavailable(_)
        | OtherStoresError::DeadlineExceeded
        | OtherStoresError::MismatchedMemory { .. }
        | OtherStoresError::LocalStoreInInventory(_)
        | OtherStoresError::DuplicateStoreInInventory(_)
        | OtherStoresError::UnsupportedSearch(_) => ToolError::StoreUnavailable {
            store: crate::memory_store_number(id),
        },
    }
}

pub(crate) async fn read_origin(
    ctx: &ToolCtx,
    id: uuid::Uuid,
) -> Result<(ReadOrigin, RemoteMemory), ToolError> {
    let service = service(ctx).ok_or_else(|| not_found(id))?;
    match service.read_origin(caller(ctx), id).await {
        Ok(Some(read)) => Ok(read),
        Ok(None) => Err(not_found(id)),
        Err(error) => Err(remote_read_error(id, &error)),
    }
}

/// The newest version of the series `id` belongs to, read through its issuing
/// store as the caller. A memory the caller cannot read, or an erased one, is
/// not found; an unavailable store is refused as such.
pub(crate) async fn read_head(ctx: &ToolCtx, id: uuid::Uuid) -> Result<RemoteMemory, ToolError> {
    let service = service(ctx).ok_or_else(|| not_found(id))?;
    match service
        .read_bounded(caller(ctx), id, ReadAt::SeriesHead)
        .await
    {
        Ok(RemoteRead::Found(memory)) => Ok(*memory),
        Ok(RemoteRead::NotFound | RemoteRead::Erased) => Err(not_found(id)),
        Err(error) => Err(remote_read_error(id, &error)),
    }
}

/// The one-hop edges of a remote memory, read through its store as the
/// caller. Unreadable or erased is not found, as for the memory itself.
pub(crate) async fn read_neighbor_edges(
    ctx: &ToolCtx,
    id: uuid::Uuid,
) -> Result<Vec<RemoteEdge>, ToolError> {
    let service = service(ctx).ok_or_else(|| not_found(id))?;
    match service.neighbor_edges_bounded(caller(ctx), id).await {
        Ok(RemoteGraphRead::Found(edges)) => Ok(edges),
        Ok(RemoteGraphRead::NotFound | RemoteGraphRead::Erased) => Err(not_found(id)),
        Err(error) => Err(remote_read_error(id, &error)),
    }
}

/// One ancestors page from a remote start, read through its store as the
/// caller. Unreadable or erased is not found, as for the memory itself.
pub(crate) async fn read_ancestors(
    ctx: &ToolCtx,
    id: uuid::Uuid,
    page: &RemoteAncestorsPage,
) -> Result<RemoteAncestors, ToolError> {
    let service = service(ctx).ok_or_else(|| not_found(id))?;
    match service.ancestors_bounded(caller(ctx), id, page).await {
        Ok(RemoteGraphRead::Found(ancestors)) => Ok(ancestors),
        Ok(RemoteGraphRead::NotFound | RemoteGraphRead::Erased) => Err(not_found(id)),
        Err(error) => Err(remote_read_error(id, &error)),
    }
}

/// The persistence carrier of one readable reference. Every wire spelling is a UUID.
#[derive(Debug)]
pub(crate) enum ResolvedMemory {
    Local(crate::MemoryId, MemoryHandleClass),
    Remote(ReadOrigin),
}

impl ResolvedMemory {
    pub(crate) fn classified(&self) -> Result<(crate::MemoryId, MemoryHandleClass), ToolError> {
        match self {
            Self::Local(id, kind) => Ok((*id, *kind)),
            Self::Remote(origin) => Ok((
                crate::MemoryId::new(origin.handle()),
                super::get_memory::memory_class(origin.kind())?,
            )),
        }
    }
}

/// Read each unique UUID through its issuing store as the caller.
#[cfg(test)]
pub(crate) async fn resolve_references(
    ctx: &ToolCtx,
    handles: &[String],
) -> Result<Vec<ResolvedMemory>, ToolError> {
    resolve_references_with_retained(ctx, handles, &[]).await
}

/// Resolve references while bypassing ordinary reads only for exact pins
/// sealed from a forked prior revision. All other handles use the caller's
/// normal local or remote read scope.
pub(crate) async fn resolve_references_with_retained(
    ctx: &ToolCtx,
    handles: &[String],
    retained: &[(crate::MemoryId, MemoryHandleClass)],
) -> Result<Vec<ResolvedMemory>, ToolError> {
    let local_number = ctx.require_engine()?.store_number();
    let retained = retained
        .iter()
        .map(|(id, kind)| (id.into_inner(), *kind))
        .collect::<std::collections::HashMap<_, _>>();
    let mut seen = std::collections::HashSet::with_capacity(handles.len());
    let mut ids = Vec::with_capacity(handles.len());
    for raw in handles {
        let id = crate::catalog::parse_wire_uuid(raw)?;
        if seen.insert(id) {
            ids.push(id);
        }
    }
    let readable_ids = ids
        .iter()
        .copied()
        .filter(|id| !retained.contains_key(id))
        .collect::<Vec<_>>();
    let (local_ids, remote_ids): (Vec<_>, Vec<_>) = readable_ids
        .iter()
        .copied()
        .partition(|id| crate::memory_store_number(*id) == local_number);
    let mut remote = resolve_remote_origins(ctx, &remote_ids).await?.into_iter();
    let local_ids = local_ids
        .into_iter()
        .map(crate::MemoryId::new)
        .collect::<Vec<_>>();
    let mut local = ctx
        .resolve_memory_ids_with_kind(&local_ids)
        .await?
        .into_iter();
    let mut resolved = readable_ids
        .into_iter()
        .map(|id| {
            if crate::memory_store_number(id) == local_number {
                let (id, kind) = local.next().expect("one classified memory per local UUID");
                (id.into_inner(), ResolvedMemory::Local(id, kind))
            } else {
                let origin = remote.next().expect("one bounded result per remote UUID");
                (origin.handle(), ResolvedMemory::Remote(origin))
            }
        })
        .collect::<std::collections::HashMap<_, _>>();
    ids.into_iter()
        .map(|id| {
            retained.get(&id).map_or_else(
                || resolved.remove(&id).ok_or_else(|| not_found(id)),
                |kind| Ok(ResolvedMemory::Local(crate::MemoryId::new(id), *kind)),
            )
        })
        .collect::<Result<Vec<_>, _>>()
}

pub(crate) async fn resolve_remote_origins(
    ctx: &ToolCtx,
    ids: &[uuid::Uuid],
) -> Result<Vec<ReadOrigin>, ToolError> {
    let Some(first) = ids.first() else {
        return Ok(Vec::new());
    };
    let service = service(ctx).ok_or_else(|| not_found(*first))?;
    let reads = service.read_origins_bounded(caller(ctx), ids).await;
    // The first failing id in request order decides the error: deterministic.
    ids.iter()
        .zip(reads)
        .map(|(id, read)| match read {
            Ok(RemoteOriginRead::Found(origin, _)) => Ok(origin),
            Ok(RemoteOriginRead::NotFound | RemoteOriginRead::Erased) => Err(not_found(*id)),
            Err(error) => Err(remote_read_error(*id, &error)),
        })
        .collect()
}

pub(crate) struct SearchStores {
    pub(crate) route: StoreRoute,
    pub(crate) service: Option<Arc<OtherStoresService>>,
    /// Other stores are wired but their list did not answer (failed or
    /// timed out), so the search ran locally only.
    pub(crate) inventory_unanswered: bool,
}

/// Route a search over the stores the caller may read.
///
/// When other stores are wired but their list does not answer, a search
/// that names a remote store fails retryable (`StoreUnavailable` for the
/// first one named), one that names only this store runs as asked, and
/// one that names none runs locally and reports `inventory_unanswered`.
pub(crate) async fn route_search(
    ctx: &ToolCtx,
    requested: Option<&[u16]>,
    schema_id: Option<&str>,
) -> Result<SearchStores, ToolError> {
    let local_number = ctx.require_engine()?.store_number();
    let service = service(ctx);
    let answered = match &service {
        Some(service) => Some(
            service
                .stores_bounded(caller(ctx), local_number)
                .await
                .map_err(|error| ToolError::Other(error.to_string()))?,
        ),
        None => None,
    };
    let mut inventory_unanswered = false;
    if matches!(answered, Some(None)) {
        if let Some(store) = requested
            .into_iter()
            .flatten()
            .copied()
            .find(|number| *number != local_number)
        {
            return Err(ToolError::StoreUnavailable { store });
        }
        inventory_unanswered = requested.is_none();
    }
    let readable = answered
        .flatten()
        .unwrap_or_else(|| StoreInventory::empty(local_number));
    let local_declares = |schema_id: &str| {
        ctx.engine().is_some_and(|engine| {
            engine
                .registry()
                .schemas()
                .iter()
                .any(|info| info.schema_id.as_str() == schema_id)
        })
    };
    let route = route_stores(requested, schema_id, local_declares, &readable)
        .map_err(ToolError::InvalidInput)?;
    Ok(SearchStores {
        route,
        service,
        inventory_unanswered,
    })
}

pub(crate) async fn search_remotes(
    ctx: &ToolCtx,
    service: &OtherStoresService,
    stores: &[u16],
    args: &RemoteSearch,
) -> (Vec<(u16, RemoteSearchResult)>, Vec<(u16, String)>) {
    let caller = caller(ctx);
    let calls = stores.iter().map(|store| async move {
        let result = tokio::time::timeout(
            service.timeout(),
            service.port().search(caller, *store, args),
        )
        .await
        .map(|result| result.and_then(|result| checked_remote_hits(*store, result)));
        (*store, result)
    });
    let mut answered = Vec::new();
    let mut unanswered = Vec::new();
    for (store, result) in futures::future::join_all(calls).await {
        match result {
            Ok(Ok(mut result)) => {
                result
                    .hits
                    .truncate(usize::try_from(args.limit).unwrap_or(usize::MAX));
                answered.push((store, result));
            }
            Ok(Err(error)) => unanswered.push((store, error.to_string())),
            Err(elapsed) => unanswered.push((store, elapsed.to_string())),
        }
    }
    (answered, unanswered)
}

/// A port response belongs to the requested numbered store, or the store did not answer safely.
fn checked_remote_hits(
    store: u16,
    result: RemoteSearchResult,
) -> Result<RemoteSearchResult, OtherStoresError> {
    if result
        .hits
        .iter()
        .any(|hit| crate::memory_store_number(hit.handle) != store)
    {
        return Err(OtherStoresError::Unavailable(
            "search returned a memory from another store".into(),
        ));
    }
    Ok(result)
}

#[cfg(test)]
#[path = "other_stores_tests.rs"]
mod tests;
