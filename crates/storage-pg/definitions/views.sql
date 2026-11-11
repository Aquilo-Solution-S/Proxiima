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
