use crate::MemoryKind;
use crate::SearchProjectionColumnKind;
use crate::authz::AuthzContext;
use crate::edge::Edge;
use crate::error::ProtocolError;
use crate::read_models::{MemoryCounts, MemorySchemaSpec, MemorySnapshot};
use crate::storage::{MemoryGraphIdentity, MemoryGraphPayloadRow, StorageError};
use crate::storage_ports::ReadVerbStoragePorts;
use crate::storage_ports::other_stores::ReadAt;
use crate::verbs::query::{
    FactCitationReadback, FactCitations, MAX_RELEVANCE_SEARCH_DEPTH, MemorySearchRequest,
    MemorySearchResult, SearchCursor, SearchMode,
};
use crate::verbs::schema::{MemorySearchProjection, payload_entity_kind};
use crate::{MemoryId, OwnerId, SchemaId, SchemaVersion};

use super::Engine;
use super::pipeline::ENTRY_NOT_FOUND_MESSAGE;

const NEIGHBOR_EDGE_LIMIT: usize = 200;

#[derive(Debug, Clone)]
pub struct SearchReadRequest {
    pub search: MemorySearchRequest,
    pub include_body: bool,
    pub include_neighbor_edges: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchReadResponse {
    pub memories: Vec<MemorySearchResult>,
    /// At least one further match exists past the last returned row;
    /// see [`crate::verbs::query::MemorySearchPage`].
    pub has_more: bool,
    pub candidate_bound: crate::storage_ports::CandidateBound,
    pub payloads: Vec<MemoryGraphPayloadRow>,
    pub neighbor_edges: Vec<Edge>,
    /// The citation of each returned Fact that cites an object.
    pub citations: FactCitations,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GetMemoryReadRequest {
    pub memory_id: MemoryId,
    pub include_neighbor_edges: bool,
    /// The version `memory_id` names, or the head of its series.
    pub at: crate::storage_ports::other_stores::ReadAt,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GetMemoryReadResponse {
    pub memory: Option<MemorySnapshot>,
    pub neighbor_edges: Vec<Edge>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GetMemoriesReadRequest {
    pub memory_ids: Vec<MemoryId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GetMemoriesReadResponse {
    /// Snapshots for the visible subset of the requested ids; unknown and
    /// invisible ids are simply absent (deliberately indistinguishable).
    pub memories: Vec<MemorySnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GetGraphReadRequest {
    pub owner: OwnerId,
}

#[derive(Debug, Clone)]
pub struct GetGraphReadResponse {
    pub pending_embedding_jobs: u64,
    pub failed_embedding_jobs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactCitationReadRequest {
    pub fact_memory_id: MemoryId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactsCitingObjectReadRequest {
    pub cited_object_id: uuid::Uuid,
    /// Max citing Facts per page.
    pub limit: u32,
    /// Keyset resume point from a previous page's `next_cursor`; `None`
    /// starts from the newest citing Fact.
    pub after: Option<crate::verbs::query::FactCitationCursor>,
}

impl Engine {
    /// Owner-scoped memory search plus domain hydration for MCP projections.
    ///
    /// # Errors
    ///
    /// Returns `InvalidArgument` when `min_score`/`semantic_weight` fall
    /// outside `0.0..=1.0`, when `semantic_weight` is set on a mode other
    /// than [`crate::verbs::query::SearchMode::Hybrid`], or when `after`
    /// disagrees with `order` or exceeds the relevance pagination depth
    /// bound; `Forbidden` when the context cannot access
    /// `req.search.owner` or reads nothing on it; `Internal` when
    /// storage reads fail.
    pub async fn search(
        &self,
        authz: &AuthzContext,
        req: &SearchReadRequest,
    ) -> Result<SearchReadResponse, ProtocolError> {
        validate_search_request(&req.search)?;
        let owners = match req.search.scope {
            crate::verbs::query::MemorySearchScope::Readable => {
                self.authorize_row_read(authz).await?
            }
            crate::verbs::query::MemorySearchScope::Owner(owner) => {
                let owner = crate::stage_timing::timed(
                    "search_authorization",
                    self.authorize_owner_read(authz, &owner, MemoryKind::Fact),
                )
                .await?;
                vec![owner]
            }
        };
        search_authorized(
            &self.storage.read_verb,
            self.registry.search_projections(),
            &self.memory_schema_specs(),
            authz.owner_scope(),
            &owners,
            req,
        )
        .await
    }

    /// Single-memory read plus optional neighbor-edge domain rows.
    ///
    /// # Errors
    ///
    /// Returns `NotFound` when `req.memory_id` does not exist or is not
    /// visible to the caller (deliberately indistinguishable), `Forbidden`
    /// when the context itself authorizes nothing, and `Internal` when
    /// storage reads fail.
    pub async fn get_memory(
        &self,
        authz: &AuthzContext,
        req: &GetMemoryReadRequest,
    ) -> Result<GetMemoryReadResponse, ProtocolError> {
        // Home, visibility, and the row are one owner-scoped inspect.
        // Absent and invisible are both "not returned" — do not probe
        // `home_owner` / `visible_to_any` first.
        let read_owners = self.authorize_row_read(authz).await?;
        let schemas = self.memory_schema_specs();
        get_memory_authorized(
            &self.storage.read_verb,
            authz.owner_scope(),
            &read_owners,
            &schemas,
            req,
        )
        .await
    }

    /// Batch single-memory read: snapshots for the subset of
    /// `req.memory_ids` visible to the caller's read-owner set. Unknown
    /// and invisible ids are omitted rather than erroring, so a caller
    /// can only observe "not returned" (deliberately indistinguishable).
    ///
    /// # Errors
    ///
    /// Returns `Forbidden` when the context authorizes no read owners, and
    /// `Internal` when storage reads fail.
    pub async fn get_memories(
        &self,
        authz: &AuthzContext,
        req: &GetMemoriesReadRequest,
    ) -> Result<GetMemoriesReadResponse, ProtocolError> {
        let read_owners = self.authorize_row_read(authz).await?;
        if read_owners.is_empty() {
            return Ok(GetMemoriesReadResponse {
                memories: Vec::new(),
            });
        }
        let schemas = self.memory_schema_specs();
        let memories = self
            .storage
            .read_verb
            .memory_inspect
            .load_memories_by_ids(authz.owner_scope(), &read_owners, &req.memory_ids, &schemas)
            .await
            .map_err(|err| storage_error("load_memories_by_ids", &err))?;
        Ok(GetMemoriesReadResponse { memories })
    }

    /// Owner-scoped persisted one-liners. Missing/unreadable ids are absent.
    ///
    /// # Errors
    ///
    /// `Forbidden` when the context authorizes no reads; `Internal` on storage.
    pub async fn load_sketches(
        &self,
        authz: &AuthzContext,
        memory_ids: &[MemoryId],
    ) -> Result<Vec<crate::read_models::MemorySketch>, ProtocolError> {
        let read_owners = self.authorize_row_read(authz).await?;
        if read_owners.is_empty() {
            return Ok(Vec::new());
        }
        self.storage
            .read_verb
            .memory_read
            .load_sketches(authz.owner_scope(), &read_owners, memory_ids)
            .await
            .map_err(|err| storage_error("load_sketches", &err))
    }

    /// How many current memories the caller can read, by kind: one per
    /// series head and one per Goal. Scoped to the same read set as
    /// [`Engine::query`]; storage narrows it per kind.
    ///
    /// # Errors
    ///
    /// `Forbidden` when the context reads no owner; `Internal` on storage.
    pub async fn memory_counts(&self, authz: &AuthzContext) -> Result<MemoryCounts, ProtocolError> {
        let read_owners = self.authorize_row_read(authz).await?;
        if read_owners.is_empty() {
            return Ok(MemoryCounts::default());
        }
        self.storage
            .read_verb
            .memory_read
            .count_memories(authz.owner_scope(), &read_owners)
            .await
            .map_err(|err| storage_error("count_memories", &err))
    }

    /// Owner-scoped pin carriers for the given `t`s. Missing/unreadable
    /// ids are absent.
    ///
    /// # Errors
    ///
    /// `Forbidden` when the context authorizes no reads; `Internal` on storage.
    pub async fn pin_nodes(
        &self,
        authz: &AuthzContext,
        memory_ids: &[MemoryId],
    ) -> Result<Vec<crate::PinNode>, ProtocolError> {
        let read_owners = self.authorize_row_read(authz).await?;
        if read_owners.is_empty() {
            return Ok(Vec::new());
        }
        self.storage
            .read_verb
            .memory_read
            .load_pin_nodes(authz.owner_scope(), &read_owners, memory_ids)
            .await
            .map_err(|err| storage_error("load_pin_nodes", &err))
    }

    /// Newest-first inbound pin page.
    ///
    /// # Errors
    ///
    /// `Forbidden` when the context authorizes no reads; `Internal` on storage.
    pub async fn inbound_pin_nodes(
        &self,
        authz: &AuthzContext,
        query: crate::storage_ports::InboundPinQuery<'_>,
    ) -> Result<Vec<crate::PinNode>, ProtocolError> {
        let read_owners = self.authorize_row_read(authz).await?;
        if read_owners.is_empty() {
            return Ok(Vec::new());
        }
        self.storage
            .read_verb
            .memory_read
            .load_inbound_pin_nodes(authz.owner_scope(), &read_owners, query)
            .await
            .map_err(|err| storage_error("load_inbound_pin_nodes", &err))
    }

    /// Owner-scoped graph overview domain read.
    ///
    /// # Errors
    ///
    /// Returns `Forbidden` when the context cannot access `req.owner` or
    /// is not its admin ([`crate::Role::administers`]), and `Internal` when
    /// storage reads fail.
    pub async fn get_graph(
        &self,
        authz: &AuthzContext,
        req: &GetGraphReadRequest,
    ) -> Result<GetGraphReadResponse, ProtocolError> {
        let permit = self.authorize_owner_admin(authz, &req.owner).await?;
        get_graph_authorized(&self.storage.read_verb, authz.owner_scope(), permit.owner()).await
    }

    /// One row per embedding job of `entities` for `owner`.
    ///
    /// Each row is one job whose memory this caller can read. A processing
    /// job's state carries how long its claim has been held, from the
    /// database clock. Pending and failed jobs carry no claim age. This is
    /// not [`Self::embedding_coverage`], which returns counters for an
    /// operator.
    ///
    /// An empty `entities` reads nothing. More than
    /// [`crate::MAX_EMBEDDING_DIAGNOSTIC_UNITS`] memories is refused.
    ///
    /// # Errors
    ///
    /// `InvalidArgument` when `entities` is over the bound, `Forbidden`
    /// when `authz` cannot read `owner`, and `Internal` on storage faults.
    pub async fn embedding_job_diagnostics(
        &self,
        authz: &AuthzContext,
        owner: &OwnerId,
        entities: &[MemoryId],
    ) -> Result<Vec<crate::EmbeddingJobDiagnostic>, ProtocolError> {
        if let Err(crate::StorageError::ConstraintViolation(message)) =
            crate::check_diagnostic_unit_count(entities.len())
        {
            return Err(ProtocolError::invalid_argument("entities", message));
        }
        let owner = self
            .authorize_owner_read(authz, owner, MemoryKind::Fact)
            .await?;
        if entities.is_empty() {
            return Ok(Vec::new());
        }
        self.storage
            .read_verb
            .embedding_job
            .embedding_job_diagnostics(authz.owner_scope(), &owner, entities)
            .await
            .map_err(|err| storage_error("embedding_job_diagnostics", &err))
    }

    /// Confirmed-id inverse citation read for one Fact memory.
    ///
    /// # Errors
    ///
    /// Returns `Forbidden` when the context cannot access `req.fact_memory_id`,
    /// and `Internal` when storage reads fail.
    pub async fn read_fact_citation(
        &self,
        authz: &AuthzContext,
        req: &FactCitationReadRequest,
    ) -> Result<Option<FactCitationReadback>, ProtocolError> {
        let read_owners = self.authorize_row_read(authz).await?;
        if read_owners.is_empty() {
            return Err(ProtocolError::forbidden(ENTRY_NOT_FOUND_MESSAGE));
        }
        let found = self
            .storage
            .read_verb
            .memory_inspect
            .load_memories_by_ids(
                authz.owner_scope(),
                &read_owners,
                &[req.fact_memory_id],
                &self.memory_schema_specs(),
            )
            .await
            .map_err(|err| storage_error("load_memories_by_ids", &err))?;
        if found.is_empty() {
            return Err(ProtocolError::forbidden(ENTRY_NOT_FOUND_MESSAGE));
        }
        read_fact_citation_authorized(
            &self.storage.read_verb,
            authz.owner_scope(),
            &read_owners,
            req.fact_memory_id,
        )
        .await
    }

    /// Read-set-scoped inverse citation read for a stateful Fact entity head.
    ///
    /// # Errors
    ///
    /// Returns `Forbidden` when the context authorizes no read owners, and
    /// `Internal` when storage reads fail.
    /// Read-set-scoped citation-to-Fact read-back.
    ///
    /// # Errors
    ///
    /// Returns `Forbidden` when the context authorizes no read owners, and
    /// `Internal` when storage reads fail.
    pub async fn facts_citing_object(
        &self,
        authz: &AuthzContext,
        req: &FactsCitingObjectReadRequest,
    ) -> Result<crate::verbs::query::FactCitationPage, ProtocolError> {
        let read_owners = self.authorize_row_read(authz).await?;
        if read_owners.is_empty() {
            return Ok(crate::verbs::query::FactCitationPage {
                facts: Vec::new(),
                next_cursor: None,
                has_more: false,
            });
        }
        let schemas = self.memory_schema_specs();
        facts_citing_object_authorized(
            &self.storage.read_verb,
            authz.owner_scope(),
            &read_owners,
            req,
            &schemas,
        )
        .await
    }

    /// The registry's memory-bearing schemas, as the read verbs want them.
    ///
    /// `payload_entity_kind` is both the filter and the projection: a kind
    /// with no `EntityKind` is not a memory, so the `None` it returns is the
    /// same answer a separate `filter` would have given. Selecting and
    /// converting in one step is what keeps an "impossible" arm off the
    /// `match` — there is no second place for the two lists to disagree.
    ///
    /// Each spec carries display fields from the schema contract: the body is
    /// derived from a projected `Text` field, while tags use the declaration's
    /// single `tag_column` accessor even when search is disabled. Search hits
    /// and by-id reads therefore share one tag source.
    pub(in crate::engine) fn memory_schema_specs(&self) -> Vec<MemorySchemaSpec> {
        let projections = self.registry.search_projections();
        let contracts = self.registry.contracts();
        self.registry
            .list()
            .into_iter()
            .filter_map(|schema| {
                let kind = payload_entity_kind(schema.kind)?;
                let projection = projections.iter().find(|projection| {
                    projection.kind == schema.kind
                        && projection.schema_id == schema.schema_id
                        && projection.schema_version == schema.schema_version
                });
                let body_field = projection.and_then(|projection| {
                    projection
                        .fields
                        .iter()
                        .find(|field| {
                            field.kind == SearchProjectionColumnKind::Text
                                && field.column != "title"
                        })
                        .map(|field| field.column.clone())
                });
                let tag_field = contracts
                    .iter()
                    .flat_map(|contract| contract.schemas)
                    .find(|declared| {
                        declared.kind == schema.kind
                            && declared.id.schema_id() == schema.schema_id
                            && declared.id.schema_version() == schema.schema_version
                    })
                    .and_then(|declared| declared.search.tag_column())
                    .map(str::to_owned);
                Some(
                    MemorySchemaSpec::new(
                        kind,
                        SchemaId::new(schema.schema_id.as_str().to_string()),
                        SchemaVersion::new(schema.schema_version.into_inner()),
                        schema.sidecar_table,
                    )
                    .with_display_fields(body_field, tag_field),
                )
            })
            .collect()
    }
}

/// Admission check for caller-supplied search knobs, shared by every
/// entry into the search verb: bounds, the cursor/order pairing, and
/// the modes each knob is meaningful in. Storage re-validates the
/// cursor/order pairing as defense in depth for direct port callers.
fn validate_search_request(search: &MemorySearchRequest) -> Result<(), ProtocolError> {
    if let Some(floor) = search.min_score
        && !(floor.is_finite() && (0.0..=1.0).contains(&floor))
    {
        return Err(ProtocolError::invalid_argument(
            "min_score",
            "min_score must be within 0.0..=1.0",
        ));
    }
    if let Some(weight) = search.semantic_weight {
        if !(weight.is_finite() && (0.0..=1.0).contains(&weight)) {
            return Err(ProtocolError::invalid_argument(
                "semantic_weight",
                "semantic_weight must be within 0.0..=1.0",
            ));
        }
        // Only hybrid fusion reads the weight; lexical and semantic ranking
        // discard it. Saying so beats accepting a knob that does nothing,
        // which reads as a ranking that ignored the caller.
        if !matches!(search.mode, SearchMode::Hybrid) {
            return Err(ProtocolError::invalid_argument(
                "semantic_weight",
                "semantic_weight applies only to mode=hybrid",
            ));
        }
    }
    match search.after {
        Some(after) if after.order() != search.order => Err(ProtocolError::invalid_argument(
            "cursor",
            "search cursor order does not match request order",
        )),
        Some(after @ SearchCursor::Relevance { .. })
            if after.seen() > MAX_RELEVANCE_SEARCH_DEPTH =>
        {
            Err(ProtocolError::invalid_argument(
                "cursor",
                "relevance pagination depth exceeded; narrow the query or switch to order=recency",
            ))
        }
        _ => Ok(()),
    }
}

pub(in crate::engine) async fn search_authorized(
    ports: &ReadVerbStoragePorts,
    search_projections: &[MemorySearchProjection],
    schemas: &[MemorySchemaSpec],
    owner_scope: Option<&crate::OwnerScope>,
    read_owners: &[OwnerId],
    req: &SearchReadRequest,
) -> Result<SearchReadResponse, ProtocolError> {
    let mut effective = req.search.clone();
    effective.read_owners = read_owners.to_vec();
    let page = crate::stage_timing::timed(
        "storage_search",
        ports
            .memory_read
            .search_memories(owner_scope, &effective, search_projections),
    )
    .await
    .map_err(|err| storage_error("search_memories", &err))?;
    let memories = page.results;

    let identities = memories
        .iter()
        .map(|row| MemoryGraphIdentity {
            memory_id: row.memory_id,
            kind: row.kind,
            schema_id: row.schema_id.clone(),
        })
        .collect::<Vec<_>>();
    let memory_ids = identities
        .iter()
        .map(|identity| identity.memory_id)
        .collect::<Vec<_>>();
    let payloads = if identities.is_empty() {
        Vec::new()
    } else {
        crate::stage_timing::timed(
            "storage_payloads",
            ports.memory_read.load_memory_graph_payloads(
                owner_scope,
                &identities,
                schemas,
                req.include_body,
            ),
        )
        .await
        .map_err(|err| storage_error("load_memory_graph_payloads", &err))?
    };
    let facts = memories
        .iter()
        .filter(|row| row.kind == crate::EntityKind::Fact)
        .map(|row| row.memory_id)
        .collect::<Vec<_>>();
    let citations = if facts.is_empty() {
        FactCitations::new()
    } else {
        crate::stage_timing::timed(
            "storage_citations",
            ports
                .citation
                .citations_of_facts(owner_scope, read_owners, &facts),
        )
        .await
        .map_err(|err| storage_error("citations_of_facts", &err))?
    };
    let neighbor_edges = if req.include_neighbor_edges {
        crate::stage_timing::timed(
            "storage_neighbors",
            super::pin_read::neighbor_edges_from_nodes(
                &ports.memory_read,
                owner_scope,
                read_owners,
                &memory_ids,
                NEIGHBOR_EDGE_LIMIT,
            ),
        )
        .await?
    } else {
        Vec::new()
    };

    Ok(SearchReadResponse {
        memories,
        has_more: page.has_more,
        candidate_bound: page.candidate_bound,
        payloads,
        neighbor_edges,
        citations,
    })
}

pub(in crate::engine) async fn get_memory_authorized(
    ports: &ReadVerbStoragePorts,
    owner_scope: Option<&crate::OwnerScope>,
    read_owners: &[OwnerId],
    schemas: &[MemorySchemaSpec],
    req: &GetMemoryReadRequest,
) -> Result<GetMemoryReadResponse, ProtocolError> {
    if read_owners.is_empty() {
        return Err(ProtocolError::not_found("memory not found"));
    }
    let memory_id = match req.at {
        ReadAt::Version => req.memory_id,
        ReadAt::SeriesHead => ports
            .memory_inspect
            .load_series_head(owner_scope, read_owners, req.memory_id)
            .await
            .map_err(|err| storage_error("load_series_head", &err))?
            .ok_or_else(|| ProtocolError::not_found("memory not found"))?,
    };
    let mut found = ports
        .memory_inspect
        .load_memories_by_ids(owner_scope, read_owners, &[memory_id], schemas)
        .await
        .map_err(|err| storage_error("load_memories_by_ids", &err))?;
    let Some(memory) = found.pop() else {
        return Err(ProtocolError::not_found("memory not found"));
    };
    let neighbor_edges = if req.include_neighbor_edges {
        super::pin_read::neighbor_edges_from_nodes(
            &ports.memory_read,
            owner_scope,
            read_owners,
            &[memory_id],
            NEIGHBOR_EDGE_LIMIT,
        )
        .await?
    } else {
        Vec::new()
    };
    Ok(GetMemoryReadResponse {
        memory: Some(memory),
        neighbor_edges,
    })
}

pub(in crate::engine) async fn get_graph_authorized(
    ports: &ReadVerbStoragePorts,
    owner_scope: Option<&crate::OwnerScope>,
    owner: &OwnerId,
) -> Result<GetGraphReadResponse, ProtocolError> {
    let job_status = ports
        .embedding_job
        .count_embedding_job_status(owner_scope, owner)
        .await
        .map_err(|err| storage_error("count_embedding_job_status", &err))?;
    Ok(GetGraphReadResponse {
        pending_embedding_jobs: job_status.pending,
        failed_embedding_jobs: job_status.failed,
    })
}

pub(in crate::engine) async fn read_fact_citation_authorized(
    ports: &ReadVerbStoragePorts,
    owner_scope: Option<&crate::OwnerScope>,
    read_owners: &[OwnerId],
    fact_memory_id: MemoryId,
) -> Result<Option<FactCitationReadback>, ProtocolError> {
    let mut citations = ports
        .citation
        .citations_of_facts(owner_scope, read_owners, &[fact_memory_id])
        .await
        .map_err(|err| storage_error("citations_of_facts", &err))?;
    Ok(citations.remove(&fact_memory_id))
}

pub(in crate::engine) async fn facts_citing_object_authorized(
    ports: &ReadVerbStoragePorts,
    owner_scope: Option<&crate::OwnerScope>,
    read_owners: &[OwnerId],
    req: &FactsCitingObjectReadRequest,
    schemas: &[MemorySchemaSpec],
) -> Result<crate::verbs::query::FactCitationPage, ProtocolError> {
    ports
        .citation
        .facts_citing_object(
            owner_scope,
            read_owners,
            req.cited_object_id,
            schemas,
            req.after,
            req.limit,
        )
        .await
        .map_err(|err| storage_error("facts_citing_object", &err))
}

fn storage_error(context: &str, err: &StorageError) -> ProtocolError {
    super::errors::internal_storage_error(context, err)
}

#[cfg(test)]
#[path = "read_verbs_tests.rs"]
mod tests;
