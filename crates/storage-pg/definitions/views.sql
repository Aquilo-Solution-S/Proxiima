-- Core Memory reads choose a view by forgotten-row visibility.
-- In platform scope the view is the only filter for forgotten rows.
-- A row lock needs UPDATE, which the serving role lacks on core views.
-- A sidecar policy delegates visibility to the spine policy directly.
--
-- The columns are listed and a new one is appended: the definitions pass
-- replaces a view only when this file changes, so `SELECT *` would leave an
-- upgraded database without the column.
CREATE OR REPLACE VIEW proxiima_core.memory_including_forgotten WITH (security_invoker = true) AS
SELECT handle, t, kind, owner_id, schema_id, source_id, ingest_key, blob_id,
       content_id, origins, refs, sidecar_tables, goal_refs, occurred_at, "by",
       via, lexical_language, forgotten_at, guided_by, forked_from, reads
  FROM proxiima_core.memory;
COMMENT ON VIEW proxiima_core.memory_including_forgotten IS 'Core''s read of every memory the caller''s row security shows, forgotten ones included. Not part of the SQL contract for spiices and hosts.';

CREATE OR REPLACE VIEW proxiima_core.live_memory WITH (security_invoker = true) AS
SELECT * FROM proxiima_core.memory_including_forgotten WHERE forgotten_at IS NULL;
COMMENT ON VIEW proxiima_core.live_memory IS 'Core''s read of the memories that are not forgotten, under the caller''s row security. Not part of the SQL contract for spiices and hosts.';

-- The definer roles of search and Goal visibility read memory through the
-- views; they hold SELECT on the table already.
GRANT SELECT ON proxiima_core.live_memory, proxiima_core.memory_including_forgotten
    TO proxiima_search, proxiima_goal_reader;

-- Read-only views for SQL a spiice or host owns: its views and trigger
-- functions. Their columns are the contract; core's tables are not.

-- The join keeps the view non-updatable regardless of grants.
CREATE OR REPLACE VIEW proxiima_core.memory_version WITH (security_invoker = true) AS
SELECT m.t,
       m.handle,
       m.kind,
       m.owner_id,
       m.schema_id,
       m.origins,
       m.refs,
       m.forgotten_at,
       (h.t IS NOT NULL) AS head
  FROM proxiima_core.memory AS m
  LEFT JOIN proxiima_core.memory_head AS h ON h.handle = m.handle AND h.t = m.t;
COMMENT ON VIEW proxiima_core.memory_version IS 'Memory versions for spiice and host SQL: read-only, under the caller''s row security. head is true for the version its series currently points at (docs/host-integration.md §SQL contract).';

SELECT proxiima_core.restrict_view_grants();
