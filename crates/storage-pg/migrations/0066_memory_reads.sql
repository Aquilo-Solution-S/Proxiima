-- Host-supplied read set (#677): the memories the host certified the writing
-- session read before this write. A third pin kind beside origins and refs;
-- context, never grounding. Source-owned and immutable like the other pin
-- arrays; the definitions check existence and the storage verb checks the
-- caller could read each target inside the write's transaction.

ALTER TABLE proxiima_core.memory
    ADD COLUMN reads uuid[] DEFAULT '{}'::uuid[] NOT NULL;

ALTER TABLE proxiima_core.memory
    ADD CONSTRAINT memory_reads_no_null_chk
    CHECK (array_position(reads, NULL::uuid) IS NULL);

CREATE INDEX memory_reads_gin
    ON proxiima_core.memory USING gin (reads);

-- An other-store read is recorded beside other-store origins and references,
-- one row per (t, memory); a declared origin or reference replaces it.
ALTER TABLE proxiima_core.memory_external_origin
    DROP CONSTRAINT memory_external_origin_edge_kind_chk,
    ADD CONSTRAINT memory_external_origin_edge_kind_chk
    CHECK (edge_kind = ANY (ARRAY['origin'::text, 'reference'::text, 'read'::text]));
