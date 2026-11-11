-- Core's own reads of the memory spine name one of two views, so the statement
-- says whether it wants forgotten memories: memory_including_forgotten is every
-- row, live_memory the rows not forgotten. Both run under the caller's row
-- security (security_invoker). In owner scope the read policy on memory already
-- hides a forgotten row unless the lifecycle scope is on; in platform scope the
-- view is the only filter.
--
-- The table itself is named in two places only. A read that locks its rows
-- (FOR UPDATE, FOR SHARE) names it, because locking through a view needs
-- UPDATE on the view and a core view is SELECT-only. A row-security policy
-- names it, because the policy defers to memory's own policy and a policy on a
-- spiice table must not depend on a core view.
-- scripts/check-architecture-guardrails.py refuses any other read.
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

-- One row per memory version the caller may read. The caller's row security
-- applies (security_invoker), so a forgotten or foreign memory is hidden the
-- way it is on the tables. The join keeps the view from being updatable: a
-- write through it is refused whatever a role was granted.
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

-- Default privileges grant DML on a new view; a view is read-only.
SELECT proxiima_core.restrict_view_grants();
