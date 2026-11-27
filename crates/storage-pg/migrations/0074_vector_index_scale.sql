-- Host vector index scale hardening:
-- 1. Allocate strictly monotonic, never-reused generations for remote vector
--    index publications and delete fences so search rejects uncommitted scores
--    and providers fence delayed writes after erase or retry.
-- 2. Allow `head` debt rows when a memory series head advances or reverts so
--    external indexes can filter `HeadsOnly` without scanning superseded rows,
--    and stamp write-ahead publish intent rows with their generation.
-- 3. Cover `embedding_heads` in `VectorKey::order` (`entity_id`, bytewise
--    `model_id`, `dim`) so keyset index sweeps stay index-only under any
--    database collation.
CREATE SEQUENCE proxiima_core.vector_index_generation_seq
    AS bigint
    START WITH 1
    INCREMENT BY 1;

ALTER TABLE proxiima_core.embedding_heads
    ADD COLUMN vector_generation bigint NOT NULL DEFAULT 0;

ALTER TABLE proxiima_core.vector_index_debt
    ADD COLUMN generation bigint NOT NULL DEFAULT 0,
    ADD COLUMN moved_from_generation bigint NOT NULL DEFAULT 0,
    DROP CONSTRAINT vector_index_debt_op_chk,
    ADD CONSTRAINT vector_index_debt_op_chk
        CHECK ((op = ANY (ARRAY['delete'::text, 'owner'::text, 'head'::text])));

COMMENT ON TABLE proxiima_core.vector_index_debt IS 'What a store owes its host vector index: delete the key''s chunks, record owner_id as their owner, or synchronize series-head status. Written by write-ahead publish intent and embedding_heads/memory_head triggers; deleted once applied.';

CREATE INDEX embedding_heads_sweep_key_idx
    ON proxiima_core.embedding_heads
    USING btree (entity_id, (model_id COLLATE "C"), dim)
    INCLUDE (owner_id, embedding_version, vector_generation);

CREATE TABLE proxiima_core.vector_index_unsent_intents (
    entity_id uuid NOT NULL,
    model_id text NOT NULL,
    dim smallint NOT NULL,
    generation bigint NOT NULL,
    tx1_xact xid8 NOT NULL,
    prior_op text,
    prior_generation bigint,
    prior_moved_from_generation bigint,
    prior_xact xid8,
    CONSTRAINT vector_index_unsent_intents_pkey PRIMARY KEY (entity_id, model_id, dim, generation),
    CONSTRAINT vector_index_unsent_intents_dim_lane_chk
        CHECK ((dim = ANY (ARRAY[384, 768, 1024, 1536, 2048, 3072]))),
    CONSTRAINT vector_index_unsent_intents_prior_op_chk
        CHECK (prior_op IS NULL OR prior_op = ANY (ARRAY['delete'::text, 'owner'::text, 'head'::text]))
);

ALTER TABLE proxiima_core.vector_index_unsent_intents ENABLE ROW LEVEL SECURITY;
ALTER TABLE ONLY proxiima_core.vector_index_unsent_intents FORCE ROW LEVEL SECURITY;

COMMENT ON TABLE proxiima_core.vector_index_unsent_intents IS 'Durable unsent disposition for host vector-index publish intents whose cleanup hit row contention or takeover on vector_index_debt; cleared by hard erase under the erase barrier and reconciled non-blockingly by drain or the next publication.';
