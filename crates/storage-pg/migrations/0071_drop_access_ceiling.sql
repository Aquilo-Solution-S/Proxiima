-- The baseline created the enum proxiima_core.access_ceiling. No column,
-- function or cast uses it: a role's read and write limits are stored as
-- proxiima_core.memory_ceiling values. Immutable once released.

DROP TYPE proxiima_core.access_ceiling;
