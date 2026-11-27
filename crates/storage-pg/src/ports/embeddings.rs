use proxiima_core::storage_ports::{
    EmbeddingJobPort, EmbeddingJobStatusCounts, EmbeddingMaintenancePort, EmbeddingTextPort,
    EmbeddingWritePort, OperatorMaintenanceProof, OwnerWritePermit,
};
use proxiima_core::{
    EmbeddingAnnObservability, EmbeddingJobClaim, EmbeddingOrphanSweepOutcome, EmbeddingSpace,
    EmbeddingWriteOutcome, MemoryId, OwnerId, StorageError,
};

use crate::{PgStorage, verbs};

#[async_trait::async_trait]
impl EmbeddingTextPort for PgStorage {
    async fn load_embedding_texts_for_host(
        &self,
        items: &[(OwnerId, proxiima_core::EntityKind, MemoryId)],
        non_embeddable_schemas: &[String],
        _proof: OperatorMaintenanceProof,
    ) -> Result<Vec<Option<String>>, StorageError> {
        let mut tx = self.platform_transaction().await?;
        let result = verbs::fact_embeddings::load_embedding_texts_on_connection(
            tx.as_mut(),
            items,
            non_embeddable_schemas,
            &self.embed_units,
        )
        .await;
        crate::owner_scope::finish_transaction(tx, result).await
    }

    async fn load_embedding_text(
        &self,
        owner_scope: Option<&proxiima_core::OwnerScope>,
        owner: &OwnerId,
        entity_kind: proxiima_core::EntityKind,
        memory_id: MemoryId,
        non_embeddable_schemas: &[String],
    ) -> Result<Option<String>, StorageError> {
        let mut tx = crate::owner_scope::begin_owner_transaction(
            &self.pool,
            crate::owner_scope::require_owner_scope(owner_scope)?,
        )
        .await?;
        let result = verbs::fact_embeddings::load_embedding_text_on_connection(
            tx.as_mut(),
            owner,
            entity_kind,
            memory_id,
            non_embeddable_schemas,
            &self.embed_units,
        )
        .await?;
        tx.commit().await.map_err(crate::error::map_err)?;
        Ok(result)
    }

    async fn load_embedding_texts(
        &self,
        owner_scope: Option<&proxiima_core::OwnerScope>,
        items: &[(proxiima_core::OwnerId, proxiima_core::EntityKind, MemoryId)],
        non_embeddable_schemas: &[String],
    ) -> Result<Vec<Option<String>>, StorageError> {
        let mut tx = crate::owner_scope::begin_owner_transaction(
            &self.pool,
            crate::owner_scope::require_owner_scope(owner_scope)?,
        )
        .await?;
        let result = verbs::fact_embeddings::load_embedding_texts_on_connection(
            tx.as_mut(),
            items,
            non_embeddable_schemas,
            &self.embed_units,
        )
        .await?;
        tx.commit().await.map_err(crate::error::map_err)?;
        Ok(result)
    }

    async fn list_memories_missing_embedding(
        &self,
        owner_scope: Option<&proxiima_core::OwnerScope>,
        owner: &OwnerId,
        space: &EmbeddingSpace,
        limit: usize,
        non_embeddable_schemas: &[String],
    ) -> Result<Vec<MemoryId>, StorageError> {
        let mut tx = crate::begin_owner_transaction(
            &self.pool,
            crate::owner_scope::require_owner_scope(owner_scope)?,
        )
        .await?;
        let result = verbs::fact_embeddings::list_memories_missing_embedding(
            tx.as_mut(),
            owner,
            space,
            limit,
            non_embeddable_schemas,
        )
        .await;
        crate::owner_scope::finish_transaction(tx, result).await
    }
}

impl PgStorage {
    /// Publish `writes` to a remote host `index` using single-connection
    /// two-phase write-ahead intent (`Proxiima.Embeddings.hostPublication`):
    ///
    /// 1. Short transaction 1 holds levels 1–3 and level 5, verifies job
    ///    claims, allocates monotonic `vector_index_generation_seq` tokens,
    ///    commits durable `'delete'` intent rows stamped with each key's
    ///    generation, and releases the PostgreSQL connection.
    /// 2. `index.put_batch(&puts)` runs with zero database connections held.
    ///    Because `embedding_heads` still holds the prior `vector_generation`,
    ///    remote search never admits uncommitted scores while the RPC is open
    ///    or after a failure before draining.
    /// 3. Short transaction 2 re-acquires levels 1–3 and level 5, completes
    ///    job claims, verifies each eligible key's intent row still carries
    ///    its generation, commits `embedding_heads`, and re-enqueues `'head'`
    ///    debt if `is_head` flipped while the RPC ran.
    ///
    /// Because at most one platform connection is held at a time, publication
    /// works even at `minPoolConnections = 1`, across shared platform pools,
    /// and behind transaction-mode `PgBouncer`.
    async fn insert_remote_embeddings(
        &self,
        index: &dyn proxiima_core::storage_ports::VectorIndex,
        writes: &[proxiima_core::storage_ports::EmbeddingWrite<'_>],
        spaces: &[&EmbeddingSpace],
    ) -> Result<Vec<EmbeddingWriteOutcome>, StorageError> {
        let head_writes: Vec<verbs::fact_embeddings::EmbeddingHeadWrite<'_>> = writes
            .iter()
            .map(|write| verbs::fact_embeddings::EmbeddingHeadWrite {
                owner: write.owner,
                entity: write.entity,
                vectors: write.vectors,
            })
            .collect();
        let lock_keys = verbs::fact_embeddings::embedding_head_lock_keys(&head_writes)?;
        let fence_targets: Vec<(OwnerId, proxiima_core::EmbeddableEntityRef)> = writes
            .iter()
            .map(|write| (*write.owner, write.entity))
            .collect();

        // Phase 1: validate provider limits, snapshot prior debt, commit
        // durable write-ahead `'delete'` intent stamped with `clock_timestamp()`
        // and monotonic generations, then release the connection before RPC I/O.
        let (puts, staged) = {
            let mut tx1 = self.platform_transaction().await?;
            let fences =
                verbs::fact_embeddings::lock_embedding_write_fences(&mut tx1, &fence_targets)
                    .await?;
            verbs::fact_embeddings::lock_embedding_keys_batch(tx1.as_mut(), &lock_keys).await?;
            for (write, space) in writes.iter().zip(spaces) {
                verbs::fact_embeddings::verify_embedding_job_claim(
                    &mut tx1,
                    write.owner,
                    write.entity,
                    space,
                    write.proof,
                )
                .await?;
            }
            let (puts, staged) = verbs::fact_embeddings::prepare_remote_publish_intent(
                &mut tx1,
                &fences,
                &head_writes,
                index,
            )
            .await?;
            if puts.is_empty() {
                verbs::fact_embeddings::complete_embedding_job_claims(&mut tx1, writes, spaces)
                    .await?;
                tx1.commit().await.map_err(crate::error::map_err)?;
                return Ok(vec![
                    EmbeddingWriteOutcome {
                        embedding_version: 0,
                    };
                    writes.len()
                ]);
            }
            tx1.commit().await.map_err(crate::error::map_err)?;
            (puts, staged)
        };

        // Phase 2: push vectors with zero database connections held across
        // network I/O, renewing the publish-intent lease periodically so long
        // healthy batches never expire. On error, immediately restore prior
        // debt for unsent keys and release uncertain keys for fenced cleanup.
        if let Err(batch_err) = self
            .put_batch_with_lease_renewal(index, &puts, &staged)
            .await
        {
            let unsent: std::collections::HashSet<usize> =
                batch_err.unsent_indices.iter().copied().collect();
            let _ = crate::vector_index::release_failed_publish_intents(
                &self.pool,
                &self.platform_scope,
                &staged,
                &unsent,
            )
            .await;
            return Err(batch_err.into_storage_error());
        }

        // Phase 3: complete claims and commit `embedding_heads` only if our
        // write-ahead intent still owns every eligible key. If Phase 3 rolls
        // back (for example after a stolen claim or concurrent invalidation),
        // release any remaining intents belonging to this batch immediately so
        // ordinary debt drain cleans up uncommitted provider vectors without
        // waiting for the lease timeout.
        match self
            .commit_remote_publish_tx2(
                writes,
                spaces,
                &head_writes,
                &fence_targets,
                &lock_keys,
                &puts,
            )
            .await
        {
            Ok(outcomes) => Ok(outcomes),
            Err(err) => {
                let empty = std::collections::HashSet::new();
                let _ = crate::vector_index::release_failed_publish_intents(
                    &self.pool,
                    &self.platform_scope,
                    &staged,
                    &empty,
                )
                .await;
                Err(err)
            }
        }
    }

    async fn commit_remote_publish_tx2(
        &self,
        writes: &[proxiima_core::storage_ports::EmbeddingWrite<'_>],
        spaces: &[&EmbeddingSpace],
        head_writes: &[verbs::fact_embeddings::EmbeddingHeadWrite<'_>],
        fence_targets: &[(OwnerId, proxiima_core::EmbeddableEntityRef)],
        lock_keys: &[(uuid::Uuid, String, i16)],
        puts: &[proxiima_core::storage_ports::VectorPut<'_>],
    ) -> Result<Vec<EmbeddingWriteOutcome>, StorageError> {
        let mut tx2 = self.platform_transaction().await?;
        let fences =
            verbs::fact_embeddings::lock_embedding_write_fences(&mut tx2, fence_targets).await?;
        verbs::fact_embeddings::lock_embedding_keys_batch(tx2.as_mut(), lock_keys).await?;
        verbs::fact_embeddings::complete_embedding_job_claims(&mut tx2, writes, spaces).await?;
        let outcomes = verbs::fact_embeddings::commit_remote_publish_heads(
            &mut tx2,
            &fences,
            head_writes,
            puts,
        )
        .await?;
        tx2.commit().await.map_err(crate::error::map_err)?;
        Ok(outcomes)
    }

    async fn put_batch_with_lease_renewal(
        &self,
        index: &dyn proxiima_core::storage_ports::VectorIndex,
        puts: &[proxiima_core::storage_ports::VectorPut<'_>],
        staged: &[crate::vector_index::StagedPublishIntent],
    ) -> Result<(), proxiima_core::storage_ports::VectorPutBatchError> {
        let put_fut = index.put_batch(puts);
        tokio::pin!(put_fut);
        let mut ticker = tokio::time::interval(self.vector_debt_renew_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;
        loop {
            tokio::select! {
                res = &mut put_fut => return res,
                _ = ticker.tick() => {
                    let _ = crate::vector_index::renew_publish_intents(
                        &self.pool,
                        &self.platform_scope,
                        staged,
                    )
                    .await;
                }
            }
        }
    }
}

#[async_trait::async_trait]
impl EmbeddingWritePort for PgStorage {
    async fn insert_embeddings(
        &self,
        writes: &[proxiima_core::storage_ports::EmbeddingWrite<'_>],
    ) -> Result<Vec<EmbeddingWriteOutcome>, StorageError> {
        if writes.is_empty() {
            return Ok(Vec::new());
        }
        let mut spaces = Vec::with_capacity(writes.len());
        for write in writes {
            spaces.push(verbs::fact_embeddings::validate_embedding_vectors(
                write.vectors,
            )?);
        }
        // `Proxiima.Embeddings.publishChunks`, `Proxiima.Embeddings.hostPublication`,
        // and `Proxiima.Embeddings.vectorStorageParts`.
        match &self.vectors {
            crate::vector_index::StoreVectors::Local => {
                let mut tx = self.platform_transaction().await?;
                let fence_targets: Vec<(OwnerId, proxiima_core::EmbeddableEntityRef)> = writes
                    .iter()
                    .map(|write| (*write.owner, write.entity))
                    .collect();
                let _fences =
                    verbs::fact_embeddings::lock_embedding_write_fences(&mut tx, &fence_targets)
                        .await?;
                verbs::fact_embeddings::complete_embedding_job_claims(&mut tx, writes, &spaces)
                    .await?;
                let lock_keys: Vec<(uuid::Uuid, String, i16)> = writes
                    .iter()
                    .zip(&spaces)
                    .map(|(write, space)| {
                        (
                            write.entity.entity_id(),
                            space.model_id().to_owned(),
                            crate::pgvector::Lane::of(space.dim()).width,
                        )
                    })
                    .collect();
                verbs::fact_embeddings::lock_embedding_keys_batch(tx.as_mut(), &lock_keys).await?;
                let mut outcomes = Vec::with_capacity(writes.len());
                for write in writes {
                    outcomes.push(
                        verbs::fact_embeddings::insert_embedding(
                            &mut tx,
                            write.owner,
                            write.entity,
                            write.vectors,
                        )
                        .await?,
                    );
                }
                tx.commit().await.map_err(crate::error::map_err)?;
                Ok(outcomes)
            }
            crate::vector_index::StoreVectors::Remote(index) => {
                self.insert_remote_embeddings(index.as_ref(), writes, &spaces)
                    .await
            }
        }
    }
}

#[async_trait::async_trait]
impl EmbeddingJobPort for PgStorage {
    async fn claim_pending_embedding_jobs(
        &self,
        limit: i64,
        skip_owners: &[OwnerId],
        skip_owner_spaces: &[(OwnerId, EmbeddingSpace)],
    ) -> Result<Vec<EmbeddingJobClaim>, StorageError> {
        let mut tx = self.platform_transaction().await?;
        let rows = verbs::fact_embeddings::claim_pending_embedding_jobs(
            tx.as_mut(),
            limit,
            skip_owners,
            skip_owner_spaces,
        )
        .await?;
        tx.commit().await.map_err(crate::error::map_err)?;
        Ok(rows)
    }

    async fn complete_embedding_job(&self, claim: &EmbeddingJobClaim) -> Result<(), StorageError> {
        let mut tx = self.platform_transaction().await?;
        verbs::fact_embeddings::complete_embedding_job(tx.as_mut(), claim).await?;
        tx.commit().await.map_err(crate::error::map_err)
    }

    async fn drain_vector_index_debt(&self, limit: i64) -> Result<u64, StorageError> {
        PgStorage::drain_vector_index_debt(self, limit).await
    }

    async fn renew_embedding_jobs(
        &self,
        claims: &[EmbeddingJobClaim],
    ) -> Result<u64, StorageError> {
        let mut tx = self.platform_transaction().await?;
        let count = verbs::fact_embeddings::renew_embedding_jobs(tx.as_mut(), claims).await?;
        tx.commit().await.map_err(crate::error::map_err)?;
        Ok(count)
    }

    async fn reclaim_stale_embedding_jobs(
        &self,
        older_than_seconds: i64,
    ) -> Result<u64, StorageError> {
        let mut tx = self.platform_transaction().await?;
        let count =
            verbs::fact_embeddings::reclaim_stale_embedding_jobs(tx.as_mut(), older_than_seconds)
                .await?;
        tx.commit().await.map_err(crate::error::map_err)?;
        Ok(count)
    }

    async fn fail_embedding_job(
        &self,
        claim: &EmbeddingJobClaim,
        error: &str,
    ) -> Result<(), StorageError> {
        let mut tx = self.platform_transaction().await?;
        verbs::fact_embeddings::fail_embedding_job(tx.as_mut(), claim, error).await?;
        tx.commit().await.map_err(crate::error::map_err)
    }

    async fn fail_embedding_job_permanently(
        &self,
        claim: &EmbeddingJobClaim,
        error: &str,
    ) -> Result<(), StorageError> {
        let mut tx = self.platform_transaction().await?;
        verbs::fact_embeddings::fail_embedding_job_permanently(tx.as_mut(), claim, error).await?;
        tx.commit().await.map_err(crate::error::map_err)
    }

    async fn release_embedding_jobs(
        &self,
        claims: &[EmbeddingJobClaim],
        error: &str,
    ) -> Result<(), StorageError> {
        let mut tx = self.platform_transaction().await?;
        verbs::fact_embeddings::release_embedding_jobs_on_connection(tx.as_mut(), claims, error)
            .await?;
        tx.commit().await.map_err(crate::error::map_err)
    }

    async fn defer_embedding_jobs(
        &self,
        claims: &[EmbeddingJobClaim],
        error: &str,
    ) -> Result<(), StorageError> {
        let mut tx = self.platform_transaction().await?;
        verbs::fact_embeddings::defer_embedding_jobs_on_connection(tx.as_mut(), claims, error)
            .await?;
        tx.commit().await.map_err(crate::error::map_err)
    }

    async fn enqueue_missing_embedding_jobs(
        &self,
        permit: &OwnerWritePermit,
        space: &EmbeddingSpace,
        limit: i64,
        non_embeddable_schemas: &[String],
    ) -> Result<u64, StorageError> {
        verbs::fact_embeddings::enqueue_missing_embedding_jobs(
            &self.pool,
            permit,
            space,
            limit,
            non_embeddable_schemas,
        )
        .await
    }

    async fn count_embedding_job_status(
        &self,
        owner_scope: Option<&proxiima_core::OwnerScope>,
        owner: &OwnerId,
    ) -> Result<EmbeddingJobStatusCounts, StorageError> {
        let mut tx = crate::begin_owner_transaction(
            &self.pool,
            crate::owner_scope::require_owner_scope(owner_scope)?,
        )
        .await?;
        let result = verbs::fact_embeddings::count_embedding_job_status(tx.as_mut(), owner).await;
        crate::owner_scope::finish_transaction(tx, result).await
    }

    async fn embedding_job_diagnostics(
        &self,
        owner_scope: Option<&proxiima_core::OwnerScope>,
        owner: &OwnerId,
        entities: &[proxiima_core::MemoryId],
    ) -> Result<Vec<proxiima_core::EmbeddingJobDiagnostic>, StorageError> {
        let mut tx = crate::begin_owner_transaction(
            &self.pool,
            crate::owner_scope::require_owner_scope(owner_scope)?,
        )
        .await?;
        let result =
            verbs::fact_embeddings::embedding_job_diagnostics(tx.as_mut(), owner, entities).await;
        crate::owner_scope::finish_transaction(tx, result).await
    }
}

#[async_trait::async_trait]
impl EmbeddingMaintenancePort for PgStorage {
    async fn embedding_ann_observability(
        &self,
        policy: proxiima_core::EmbeddingRuntimePolicy,
        _proof: OperatorMaintenanceProof,
    ) -> Result<EmbeddingAnnObservability, StorageError> {
        verbs::fact_embeddings::embedding_ann_observability(
            &self.pool,
            &self.platform_scope,
            policy.stale_claim_timeout_seconds(),
            self.vector_storage(),
        )
        .await
    }

    async fn sweep_orphan_embedding_rows(
        &self,
        _proof: OperatorMaintenanceProof,
    ) -> Result<EmbeddingOrphanSweepOutcome, StorageError> {
        PgStorage::sweep_orphan_embedding_rows(self).await
    }

    async fn reconcile_embeddings(
        &self,
        options: proxiima_core::EmbeddingReconcileOptions<'_>,
        policy: proxiima_core::EmbeddingRuntimePolicy,
        _proof: OperatorMaintenanceProof,
    ) -> Result<proxiima_core::EmbeddingReconcileOutcome, StorageError> {
        verbs::fact_embeddings::reconcile_embeddings_with_platform(
            &self.pool,
            &self.platform_scope,
            options,
            policy.stale_claim_timeout_seconds(),
        )
        .await
    }

    async fn embedding_owner_page(
        &self,
        after: Option<OwnerId>,
        limit: i64,
        _proof: OperatorMaintenanceProof,
    ) -> Result<Vec<OwnerId>, StorageError> {
        verbs::fact_embeddings::embedding_owner_page(&self.pool, &self.platform_scope, after, limit)
            .await
    }

    async fn embedding_coverage(
        &self,
        owner: &OwnerId,
        spaces: &[proxiima_core::EmbeddingSpace],
        non_embeddable_schemas: &[String],
        _proof: OperatorMaintenanceProof,
    ) -> Result<
        Vec<(
            proxiima_core::EmbeddingSpace,
            proxiima_core::EmbeddingSpaceCounts,
        )>,
        StorageError,
    > {
        verbs::fact_embeddings::embedding_coverage(
            &self.pool,
            &self.platform_scope,
            owner,
            spaces,
            non_embeddable_schemas,
        )
        .await
    }

    async fn purge_embedding_spaces(
        &self,
        owner: &OwnerId,
        keep: &[proxiima_core::EmbeddingSpace],
        limit: i64,
        _proof: OperatorMaintenanceProof,
    ) -> Result<proxiima_core::EmbeddingPurgeOutcome, StorageError> {
        verbs::fact_embeddings::purge_embedding_spaces(
            &self.pool,
            &self.platform_scope,
            owner,
            keep,
            limit,
        )
        .await
    }
}
