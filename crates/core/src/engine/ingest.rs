use super::Engine;
use crate::SchemaVersion;
use crate::access::AccessKind;
use crate::authz::EngineAuthority;
use crate::edge::EdgeEndpoint;
use crate::embedding::EmbeddingSpace;
use crate::error::ProtocolError;
use crate::storage_ports::OwnerWritePermit;
use crate::verbs::fact_ingest::{
    AuthorizedCitation, AuthorizedFactCore, AuthorizedFactWrite, AuthorizedInlineCitedObject,
    AuthorizedNodeLinks, FactIngestOutcome, FactWriteCommand, InlineCitedObjectDraft,
    check_occurred_at,
};
use crate::verbs::schema::{PayloadKind, ProtocolPayload, SchemaInfo};
use crate::{EntityKind, MemoryId, OwnerId, SidecarPayload};

/// Capture only the values which can select a natural series; body replay semantics are unchanged.
fn bind_fact_natural_key(
    registry: &crate::SpiiceRegistryFrozen,
    draft: &FactWriteCommand,
    sidecars: &[SidecarPayload],
) -> Result<Option<crate::NaturalKeySnapshot>, ProtocolError> {
    if draft.handle.is_some() {
        return Ok(None);
    }
    let declaration = registry
        .natural_key_declaration(PayloadKind::Fact, &draft.schema_id, draft.schema_version)
        .ok_or_else(|| {
            ProtocolError::internal("registered Fact is missing its typed key declaration")
        })?;
    if declaration.is_empty() {
        return Ok(None);
    }
    let mut matches = sidecars.iter().filter(|payload| {
        payload.kind == PayloadKind::Fact
            && payload.schema_id == draft.schema_id
            && payload.schema_version == draft.schema_version
    });
    let payload = matches.next().ok_or_else(|| {
        ProtocolError::invalid_argument(
            "sidecars",
            "automatic natural-key selection requires a matching typed Fact payload",
        )
    })?;
    if matches.next().is_some() {
        return Err(ProtocolError::invalid_argument(
            "sidecars",
            "natural-key binding requires exactly one matching typed Fact payload",
        ));
    }
    declaration.capture(payload).map(Some).ok_or_else(|| {
        ProtocolError::invalid_argument(
            "sidecars",
            "typed Fact payload does not match the registered natural-key declaration",
        )
    })
}

/// The one check every Fact authorization path runs on the draft before it
/// mints a witness: the draft is a Fact, and its writer-given occurrence
/// time is admitted against this host's clock and stored in the form
/// storage keeps ([`check_occurred_at`]).
fn normalize_fact_draft(draft: &mut FactWriteCommand) -> Result<(), ProtocolError> {
    if draft.kind != crate::MemoryKind::Fact {
        return Err(ProtocolError::invalid_argument(
            "kind",
            "Fact ingest requires a Fact source",
        ));
    }
    if let Some(occurred_at) = draft.occurred_at {
        draft.occurred_at = Some(check_occurred_at(
            occurred_at,
            time::OffsetDateTime::now_utc(),
        )?);
    }
    Ok(())
}

impl Engine {
    /// Authorize + schema-validate + owner-stamp a Fact write,
    /// returning a witness required by the sidecar-ingest primitive.
    /// Does NOT write.
    ///
    /// # Errors
    ///
    /// Returns `Forbidden` when the context cannot resolve exactly one owner it
    /// may write Facts on; `UnknownSchema` when the Fact schema or provided
    /// citation schemas are not registered; `InvalidArgument` when the
    /// draft's `occurred_at` lies more than [`crate::OCCURRED_AT_FUTURE_BOUND`]
    /// in the future.
    pub async fn authorize_fact_ingest<A>(
        &self,
        authority: &A,
        draft: FactWriteCommand,
        sidecars: &[SidecarPayload],
    ) -> Result<AuthorizedFactWrite, ProtocolError>
    where
        A: EngineAuthority + ?Sized,
    {
        self.authorize_fact_ingest_visible(authority, draft, sidecars, &[], &[])
            .await
    }

    /// [`Self::authorize_fact_ingest`] treating `session_visible` memory
    /// ids as already read-checked (written earlier in the same [`UnitOfWork`]).
    pub(in crate::engine) async fn authorize_fact_ingest_visible<A>(
        &self,
        authority: &A,
        draft: FactWriteCommand,
        sidecars: &[SidecarPayload],
        session_visible: &[MemoryId],
        session_visible_kinds: &[(MemoryId, EntityKind)],
    ) -> Result<AuthorizedFactWrite, ProtocolError>
    where
        A: EngineAuthority + ?Sized,
    {
        let owner = self.single_fact_write_owner(authority)?;
        let permit = self
            .authorize_write(authority, &owner, AccessKind::Fact)
            .await?;
        self.authorize_fact_ingest_permitted_visible(
            authority,
            permit,
            draft,
            sidecars,
            session_visible,
            session_visible_kinds,
        )
        .await
    }

    /// Shared normalization for the ownerless protocol and owner-explicit typed API.
    pub(in crate::engine) async fn authorize_fact_ingest_permitted_visible<
        A: EngineAuthority + ?Sized,
    >(
        &self,
        authority: &A,
        permit: super::pipeline::WritePermit,
        mut draft: FactWriteCommand,
        sidecars: &[SidecarPayload],
        session_visible: &[MemoryId],
        session_visible_kinds: &[(MemoryId, EntityKind)],
    ) -> Result<AuthorizedFactWrite, ProtocolError> {
        normalize_fact_draft(&mut draft)?;
        let fact_info = self.fact_schema_info(&draft.schema_id, draft.schema_version)?;
        let fact_sidecar_table = fact_info.sidecar_table.clone();
        let fact_natural_key_columns = fact_info.natural_key_columns.clone();
        if let Some(citation) = &draft.citation {
            self.ensure_fact_ingest_schema(
                &citation.object.schema_id,
                citation.object.schema_version,
            )?;
        }
        let links = self
            .authorize_fact_node_links(
                authority,
                &draft,
                sidecars,
                session_visible,
                session_visible_kinds,
            )
            .await?;
        let natural_key_values = bind_fact_natural_key(&self.registry, &draft, sidecars)?;
        let publication =
            self.resolve_publication(authority, fact_info, *permit.owner(), sidecars)?;
        Ok(AuthorizedFactWrite::new(
            AuthorizedFactCore::new(
                permit,
                draft,
                fact_sidecar_table,
                fact_natural_key_columns,
                sidecars.to_vec(),
                links,
            )
            .with_natural_key_values(natural_key_values)
            .with_publication(publication),
            AuthorizedCitation::None,
        ))
    }

    /// Authorize + schema-validate + owner-stamp a Fact with a typed
    /// inline cited object. Does NOT write.
    ///
    /// # Errors
    ///
    /// Returns `Forbidden` when the context cannot resolve exactly one owner it
    /// may write Facts on; `UnknownSchema` when any schema is absent for the
    /// required kind; `InvalidArgument` when JSON payload validation fails or
    /// the draft's `occurred_at` lies more than
    /// [`crate::OCCURRED_AT_FUTURE_BOUND`] in the future; or `Internal` when
    /// the cited object produces no content hash.
    pub async fn authorize_fact_with_citation<A>(
        &self,
        authority: &A,
        draft: FactWriteCommand,
        cited_object: InlineCitedObjectDraft,
        sidecars: &[SidecarPayload],
    ) -> Result<AuthorizedFactWrite, ProtocolError>
    where
        A: EngineAuthority + ?Sized,
    {
        let (core, object) = self
            .authorize_inline_cited_fact(authority, draft, cited_object, sidecars)
            .await?;
        Ok(AuthorizedFactWrite::new(
            core,
            AuthorizedCitation::Inline {
                object: Box::new(object),
                upload_completion: None,
            },
        ))
    }

    /// [`Self::authorize_fact_with_citation`] before its citation is settled:
    /// the upload verb completes its upload row with the same write.
    pub(in crate::engine) async fn authorize_inline_cited_fact<A>(
        &self,
        authority: &A,
        mut draft: FactWriteCommand,
        cited_object: InlineCitedObjectDraft,
        sidecars: &[SidecarPayload],
    ) -> Result<(AuthorizedFactCore, AuthorizedInlineCitedObject), ProtocolError>
    where
        A: EngineAuthority + ?Sized,
    {
        let owner = self.single_fact_write_owner(authority)?;
        let permit = self
            .authorize_write(authority, &owner, AccessKind::Fact)
            .await?;
        normalize_fact_draft(&mut draft)?;

        // Validate the Fact only by schema-existence, matching
        // `authorize_fact_ingest`. The Fact payload is built from a
        // trusted typed struct. The untrusted cited-object payload is
        // agent-supplied JSON, so it stays fully validated below.
        let fact_info = self.fact_schema_info(&draft.schema_id, draft.schema_version)?;
        let fact_sidecar_table = fact_info.sidecar_table.clone();
        let fact_natural_key_columns = fact_info.natural_key_columns.clone();
        let cited_object = self.authorize_inline_citation(cited_object)?;
        let links = self
            .authorize_fact_node_links(authority, &draft, sidecars, &[], &[])
            .await?;

        let natural_key_values = bind_fact_natural_key(&self.registry, &draft, sidecars)?;
        let publication =
            self.resolve_publication(authority, fact_info, *permit.owner(), sidecars)?;
        Ok((
            AuthorizedFactCore::new(
                permit,
                draft,
                fact_sidecar_table,
                fact_natural_key_columns,
                sidecars.to_vec(),
                links,
            )
            .with_natural_key_values(natural_key_values)
            .with_publication(publication),
            cited_object,
        ))
    }

    /// Authorize + schema-validate + owner-stamp a Fact that cites an
    /// ALREADY-STORED cited object by id. Does NOT write, and does not
    /// resolve the referenced object — existence and owner of the stored
    /// row are storage's check, inside the same transaction that writes
    /// the Fact (no TOCTOU window).
    ///
    /// # Errors
    ///
    /// Returns `Forbidden` when the context cannot resolve exactly one
    /// owner it may write Facts on; `UnknownSchema` when the Fact
    /// schema is absent for the required kind; or `InvalidArgument` when the
    /// draft's `occurred_at` lies more than [`crate::OCCURRED_AT_FUTURE_BOUND`]
    /// in the future.
    pub async fn authorize_fact_with_citation_by_ref<A>(
        &self,
        authority: &A,
        mut draft: FactWriteCommand,
        cited_object_id: uuid::Uuid,
        sidecars: &[SidecarPayload],
    ) -> Result<AuthorizedFactWrite, ProtocolError>
    where
        A: EngineAuthority + ?Sized,
    {
        let owner = self.single_fact_write_owner(authority)?;
        let permit = self
            .authorize_write(authority, &owner, AccessKind::Fact)
            .await?;
        normalize_fact_draft(&mut draft)?;
        let fact_info = self.fact_schema_info(&draft.schema_id, draft.schema_version)?;
        let fact_sidecar_table = fact_info.sidecar_table.clone();
        let fact_natural_key_columns = fact_info.natural_key_columns.clone();
        let links = self
            .authorize_fact_node_links(authority, &draft, sidecars, &[], &[])
            .await?;

        let natural_key_values = bind_fact_natural_key(&self.registry, &draft, sidecars)?;
        let publication =
            self.resolve_publication(authority, fact_info, *permit.owner(), sidecars)?;
        Ok(AuthorizedFactWrite::new(
            AuthorizedFactCore::new(
                permit,
                draft,
                fact_sidecar_table,
                fact_natural_key_columns,
                sidecars.to_vec(),
                links,
            )
            .with_natural_key_values(natural_key_values)
            .with_publication(publication),
            AuthorizedCitation::Reference(cited_object_id),
        ))
    }

    /// Resolve the index rows a Fact write is admitted to assert: its
    /// declared origins, and the references its typed payloads carry.
    ///
    /// A Fact sits at the bottom of the F/A/P order, so its pins may target
    /// Facts or Goals. Origins come from the derivation declaration and
    /// references from payload content; every declared endpoint kind is
    /// checked against the resolved target before the authorized carrier is
    /// minted.
    async fn authorize_fact_node_links<A>(
        &self,
        authority: &A,
        draft: &FactWriteCommand,
        sidecars: &[SidecarPayload],
        session_visible: &[MemoryId],
        session_visible_kinds: &[(MemoryId, EntityKind)],
    ) -> Result<AuthorizedNodeLinks, ProtocolError>
    where
        A: EngineAuthority + ?Sized,
    {
        let declared: Vec<_> = sidecars
            .iter()
            .flat_map(SidecarPayload::references)
            .collect();
        let typed_references: Vec<EdgeEndpoint> = declared
            .into_iter()
            .map(|reference| reference.target)
            .fold(Vec::new(), |mut references, target| {
                if !references.contains(&target) {
                    references.push(target);
                }
                references
            });
        let raw_references: Vec<EdgeEndpoint> = draft
            .refs
            .iter()
            .copied()
            .map(|id| EdgeEndpoint::memory(EntityKind::Fact, MemoryId::new(id)))
            .fold(Vec::new(), |mut references, target| {
                if !references.contains(&target) {
                    references.push(target);
                }
                references
            });
        if !draft.refs.is_empty() && !typed_references.is_empty() {
            let typed_ids: Vec<_> = typed_references
                .iter()
                .map(|reference| reference.entity_id())
                .fold(Vec::new(), |mut ids, id| {
                    if !ids.contains(&id) {
                        ids.push(id);
                    }
                    ids
                });
            let raw_ids: Vec<_> = raw_references
                .iter()
                .map(|reference| reference.entity_id())
                .collect();
            if typed_ids != raw_ids {
                return Err(ProtocolError::invalid_argument(
                    "refs",
                    "raw Fact references must equal the payload-declared references",
                ));
            }
        }
        let references = if typed_references.is_empty() {
            raw_references
        } else {
            typed_references
        };
        let additional = self
            .authorize_fact_link_targets(
                authority,
                &draft.additional_references,
                "additional_references",
                session_visible,
                session_visible_kinds,
            )
            .await?;
        let mut references = self
            .authorize_fact_link_targets(
                authority,
                &references,
                "references",
                session_visible,
                session_visible_kinds,
            )
            .await?;
        for target in additional {
            if !references.contains(&target) {
                references.push(target);
            }
        }
        Ok(AuthorizedNodeLinks::new(Vec::new(), references))
    }

    async fn authorize_fact_link_targets<A>(
        &self,
        authority: &A,
        targets: &[EdgeEndpoint],
        field: &str,
        session_visible: &[MemoryId],
        session_visible_kinds: &[(MemoryId, EntityKind)],
    ) -> Result<Vec<EdgeEndpoint>, ProtocolError>
    where
        A: EngineAuthority + ?Sized,
    {
        let mut out: Vec<EdgeEndpoint> = Vec::with_capacity(targets.len());
        // Kind comparison is deferred out of the admission loop and grouped
        // under the permit owner that admitted each target: one kind load per
        // owner, not one per target. A Vec and not a map because the order a
        // caller met the owners is the order their mismatches surface.
        let mut deferred_kinds: Vec<(OwnerId, Vec<(MemoryId, EntityKind)>)> = Vec::new();
        for target in targets {
            // A repeated declaration names the same endpoint, and the first
            // occurrence already validated and admitted it. Skipping here and
            // not at the push below is what keeps a duplicate off the read
            // path: shape, layering and `authorize_entry_read` all run once.
            if out.contains(target) {
                continue;
            }
            target
                .validate_shape()
                .map_err(|err| ProtocolError::invalid_argument(field, err))?;
            if field == "derived_from" && matches!(target.entity, crate::EntityRef::Goal(_)) {
                return Err(ProtocolError::invalid_argument(
                    field,
                    "Fact origins must target a Memory",
                ));
            }
            match target.kind {
                EntityKind::Fact | EntityKind::Goal => {}
                EntityKind::Abstraction | EntityKind::Perspective => {
                    return Err(ProtocolError::invalid_argument(
                        field,
                        format!(
                            "layering violation: a Fact cannot point at a {}",
                            target.kind.as_str()
                        ),
                    ));
                }
            }
            match target.entity {
                crate::EntityRef::Memory(memory_id) if session_visible.contains(&memory_id) => {
                    let actual = session_visible_kinds
                        .iter()
                        .find_map(|(id, kind)| (*id == memory_id).then_some(*kind))
                        .ok_or_else(|| {
                            ProtocolError::internal(
                                "session-visible Fact reference is missing its stored kind",
                            )
                        })?;
                    if actual != target.kind {
                        return Err(ProtocolError::invalid_argument(
                            field,
                            format!(
                                "declared target kind {} does not match stored kind {}",
                                target.kind.as_str(),
                                actual.as_str()
                            ),
                        ));
                    }
                }
                crate::EntityRef::Memory(memory_id) => {
                    let permit = self
                        .authorize_entry_read(authority, crate::EntityId::Memory(memory_id))
                        .await?;
                    let owner = *permit.owner();
                    match deferred_kinds
                        .iter_mut()
                        .find(|(admitted, _)| *admitted == owner)
                    {
                        Some((_, declared)) => declared.push((memory_id, target.kind)),
                        None => deferred_kinds.push((owner, vec![(memory_id, target.kind)])),
                    }
                }
                crate::EntityRef::Goal(goal_id) => {
                    self.authorize_entry_read(authority, crate::EntityId::Goal(goal_id))
                        .await?;
                }
            }
            out.push(*target);
        }
        for (owner, declared) in &deferred_kinds {
            let memory_ids: Vec<MemoryId> = declared.iter().map(|(id, _)| *id).collect();
            let stored = self
                .load_required_memory_kinds(
                    self.operation_authority(authority)?.authz().owner_scope(),
                    owner,
                    &memory_ids,
                )
                .await?;
            for ((_, target_kind), actual) in declared.iter().zip(stored) {
                if actual != *target_kind {
                    return Err(ProtocolError::invalid_argument(
                        field,
                        format!(
                            "declared target kind {} does not match stored kind {}",
                            target_kind.as_str(),
                            actual.as_str()
                        ),
                    ));
                }
            }
        }
        Ok(out)
    }

    /// The one owner a Fact write resolves to when the command names none.
    pub(in crate::engine) fn single_fact_write_owner<A>(
        &self,
        authority: &A,
    ) -> Result<OwnerId, ProtocolError>
    where
        A: EngineAuthority + ?Sized,
    {
        let authz = self.operation_authority(authority)?.authz();
        let owners = if authz.auth_path() == crate::authz::AuthPath::Denied {
            Vec::new()
        } else {
            authz.writable_owners(AccessKind::Fact)
        };
        match owners.as_slice() {
            [owner] => Ok(*owner),
            [] => Err(ProtocolError::forbidden(
                "requires Fact write on this owner",
            )),
            _ => Err(ProtocolError::invalid_argument(
                "owner",
                "FactWriteCommand is ownerless; authorization must resolve exactly one writable owner",
            )),
        }
    }

    /// Spaces a new memory of `schema_id` owned by `owner` is queued for:
    /// the route's current and next spaces, unless the schema has no embed unit.
    /// Fact and derived admission share this registry and routing-policy boundary;
    /// selecting spaces never requests a provider vector.
    ///
    /// # Errors
    ///
    /// `Internal` when the host cannot route `owner`: the write fails
    /// rather than landing without the job that would make it searchable.
    /// [`Self::embedding_spaces_for_write`] for the owner and schema an authorized Fact write names.
    pub(in crate::engine) async fn embedding_spaces_for_fact(
        &self,
        permit: &OwnerWritePermit,
        draft: &FactWriteCommand,
    ) -> Result<Vec<EmbeddingSpace>, ProtocolError> {
        self.embedding_spaces_for_write(permit.owner(), draft.schema_id.as_str())
            .await
    }

    pub(in crate::engine) async fn embedding_spaces_for_write(
        &self,
        owner: &OwnerId,
        schema_id: &str,
    ) -> Result<Vec<EmbeddingSpace>, ProtocolError> {
        if !self.registry().schema_is_embeddable(schema_id) {
            return Ok(Vec::new());
        }
        Ok(self.write_route(owner).await?.write_spaces())
    }

    /// Persist an already-authorized Fact with its typed sidecars and the
    /// citation its authorization settled ([`AuthorizedCitation`]), queued
    /// for the engine's embedding spaces unless the schema declines a vector.
    ///
    /// # Errors
    ///
    /// Returns caller-fixable storage rejections (concurrent citation, a
    /// missing or foreign cited object) as `InvalidArgument` and
    /// infrastructure faults as `Internal`.
    pub async fn ingest_authorized_fact(
        &self,
        authorized: &AuthorizedFactWrite,
    ) -> Result<FactIngestOutcome, ProtocolError> {
        self.validate_write_permit(authorized.owner_write_permit())?;
        let embedding_spaces = self
            .embedding_spaces_for_fact(authorized.owner_write_permit(), authorized.draft())
            .await?;
        self.storage()
            .ingest
            .fact_ingest
            .ingest_authorized_fact(authorized, &embedding_spaces)
            .await
            .map_err(super::errors::map_fact_ingest_error)
    }

    fn authorize_inline_citation(
        &self,
        cited_object: InlineCitedObjectDraft,
    ) -> Result<AuthorizedInlineCitedObject, ProtocolError> {
        let (_cited_object_info, cited_object_payload) = self.ingest_protocol_payload(
            &cited_object.schema_id,
            cited_object.schema_version,
            PayloadKind::CitedObject,
            &cited_object.payload_bytes,
            "cited_object.payload_bytes",
        )?;
        let content_hash = cited_object_payload.content_hash.ok_or_else(|| {
            ProtocolError::internal(format!(
                "cited object schema {} v{} did not produce a content hash",
                cited_object.schema_id.as_str(),
                cited_object.schema_version.into_inner(),
            ))
        })?;
        Ok(AuthorizedInlineCitedObject::new(
            cited_object.schema_id,
            cited_object.schema_version,
            content_hash,
            cited_object_payload.sidecar_payload,
        ))
    }

    fn ensure_fact_ingest_schema(
        &self,
        schema_id: &crate::SchemaId,
        schema_version: SchemaVersion,
    ) -> Result<(), ProtocolError> {
        if self.registry.lookup(schema_id, schema_version).is_none() {
            return Err(ProtocolError::unknown_schema(
                schema_id.as_str(),
                schema_version.into_inner(),
            ));
        }
        Ok(())
    }

    /// Resolve the publication plan a Fact admission must capture, or
    /// `None` when its schema is not listenable.
    ///
    /// Everything the envelope needs that is NOT the `t` storage mints is
    /// bound here, at authorization time, in the one place that holds all
    /// three inputs: the frozen schema declaration, the deployment's
    /// configured source, and the authenticated edge's trusted model
    /// identity. In particular the model label is
    /// [`crate::AuthzContext::trusted_model_id`] and never the caller's
    /// own `model_id` argument, which is a claim rather than a credential.
    ///
    /// A listenable schema written through a route that carries no typed
    /// payload is REFUSED here, before any write: the export snapshot IS
    /// the payload's serde JSON, and a receipt-only admission has none.
    ///
    /// The deployment's [`PublicationLimits`] ride along in the returned
    /// plan. They are read from THIS engine's [`PublicationConfig`] and
    /// travel with the draft all the way to the seal, so the ceiling a
    /// host configured is necessarily the ceiling capture enforces.
    ///
    /// [`PublicationLimits`]: crate::publication::PublicationLimits
    /// [`PublicationConfig`]: crate::publication::PublicationConfig
    fn resolve_publication<A>(
        &self,
        authority: &A,
        fact_info: &SchemaInfo,
        owner: OwnerId,
        sidecars: &[SidecarPayload],
    ) -> Result<Option<crate::publication::PublicationPlan>, ProtocolError>
    where
        A: EngineAuthority + ?Sized,
    {
        if !fact_info.listenable {
            return Ok(None);
        }
        let schema_id = fact_info.schema_id.clone();
        let Some(source) = self.publication.source.clone() else {
            return Err(super::errors::map_publication_error(
                &crate::publication::PublicationError::SourceUnbound {
                    schema_id: schema_id.as_str().to_owned(),
                },
            ));
        };
        let mut matches = sidecars.iter().filter(|payload| {
            payload.kind == PayloadKind::Fact
                && payload.schema_id == schema_id
                && payload.schema_version == fact_info.schema_version
        });
        let Some(payload) = matches.next() else {
            return Err(super::errors::map_publication_error(
                &crate::publication::PublicationError::UntypedListenableWrite {
                    schema_id: schema_id.as_str().to_owned(),
                },
            ));
        };
        if matches.next().is_some() {
            return Err(super::errors::map_publication_error(
                &crate::publication::PublicationError::ExportFailed(format!(
                    "listenable schema {} was supplied twice in one admission",
                    schema_id.as_str()
                )),
            ));
        }
        let data = payload.to_protocol_json().map_err(|err| {
            super::errors::map_publication_error(
                &crate::publication::PublicationError::ExportFailed(err),
            )
        })?;
        let operation = self.operation_authority(authority)?;
        let authz = operation.authz();
        let model_id = authz.trusted_model_id().map(ToOwned::to_owned);
        // Host-bound attributes ride the SAME authorization context as the
        // trusted model id: nothing the payload carries can reach them.
        let extensions = authz.publication_extensions().clone();
        Ok(Some(crate::publication::PublicationPlan::new(
            crate::publication::PublicationDraft::new(
                schema_id,
                fact_info.schema_version,
                source,
                owner,
                model_id,
                extensions,
                data,
            ),
            self.publication.limits,
        )))
    }

    /// Every engine-authorized Fact write names its schema here, so this is
    /// where `core/goal-change-v1` is refused: a scheduler acts on a goal
    /// change, so only a goal write may record one, in storage and under the
    /// goal's own authority (docs/host-integration.md).
    fn fact_schema_info(
        &self,
        schema_id: &crate::SchemaId,
        schema_version: SchemaVersion,
    ) -> Result<&SchemaInfo, ProtocolError> {
        if schema_id.as_str() == <crate::GoalChangeV1 as crate::FactPayload>::SCHEMA_ID {
            return Err(ProtocolError::invalid_argument(
                "schema_id",
                "core/goal-change-v1 is written only by a goal write (core_goal)",
            ));
        }
        self.registry
            .lookup_payload(schema_id, schema_version, PayloadKind::Fact)
            .ok_or_else(|| {
                ProtocolError::unknown_schema(schema_id.as_str(), schema_version.into_inner())
            })
    }

    pub(super) fn ingest_protocol_payload<'a>(
        &'a self,
        schema_id: &crate::SchemaId,
        schema_version: SchemaVersion,
        kind: PayloadKind,
        payload_bytes: &[u8],
        field: &str,
    ) -> Result<(&'a SchemaInfo, ProtocolPayload), ProtocolError> {
        let info = self
            .registry
            .lookup_payload(schema_id, schema_version, kind)
            .ok_or_else(|| {
                ProtocolError::unknown_schema(schema_id.as_str(), schema_version.into_inner())
            })?;
        let payload: serde_json::Value = serde_json::from_slice(payload_bytes).map_err(|e| {
            ProtocolError::invalid_argument(field, format!("invalid JSON payload: {e}"))
        })?;
        let payload = self
            .registry
            .ingest_protocol_payload(schema_id, schema_version, kind, &payload)
            .map_err(|e| ProtocolError::invalid_argument(field, e))?;
        Ok((info, payload))
    }
}

#[cfg(test)]
#[path = "ingest_tests.rs"]
mod tests;
