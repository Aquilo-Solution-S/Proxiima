-- `Proxiima.Embeddings.settleClaim`. A job the provider failed is deferred:
-- it takes a new queue position behind every job queued before the deferral,
-- so a job that fails on every attempt cannot hold the head of the queue.
-- A nullable column without a default changes no stored row.
ALTER TABLE proxiima_core.embedding_jobs
    ADD COLUMN retry_position uuid;

DROP INDEX proxiima_core.embedding_jobs_pending_claim_idx;

CREATE INDEX embedding_jobs_pending_claim_idx
    ON proxiima_core.embedding_jobs
    USING btree ((COALESCE(retry_position, job_id)), job_id)
    WHERE (status = 'pending'::proxiima_core.embedding_job_status);
