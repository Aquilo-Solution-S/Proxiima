-- The five-field role is stored once, in one shape.
--
-- A role is a read ceiling and a write ceiling over the memory ladder
-- (memory_ceiling), what it may do with Goals (goal_access), whether it
-- administers the owner and whether it manages the group. The group role
-- projection, the Goal access grants and the delegated authority grants
-- stored it on a four-rung ladder in three ways; each now holds these columns.
-- A delegated grant stores no `manage`: a delegated ceiling never manages.
--
-- Every column is converted in place, so no row trigger fires and the
-- projection's write guard stays installed. The new columns are derived from
-- the old ones before the old ones are converted. A row whose write ceiling is
-- above its read ceiling stops the migration at the new CHECK; none is
-- coerced.
--
-- The enum `access_ceiling` stays, now used by no column: the historical
-- definitions replay (`rename_bootstrap`) installs the released
-- `read_subject_authority` that returns it, whenever a historical spiice lane
-- is added to a finalized core. The definitions pass retires that function.

CREATE TYPE proxiima_core.memory_ceiling AS ENUM ('none', 'fact', 'abstraction', 'perspective');
CREATE TYPE proxiima_core.goal_access AS ENUM ('none', 'reads', 'works');

-- Projection and delegated grants: the 'goal' rung above Perspective was Goal
-- work on a read column and administration on a write column.

ALTER TABLE proxiima_core.group_role_projection
    DROP CONSTRAINT group_role_projection_role_chk,
    ADD COLUMN goal_access text,
    ADD COLUMN administers boolean;
ALTER TABLE proxiima_core.group_role_projection
    ALTER COLUMN goal_access TYPE proxiima_core.goal_access
        USING (CASE WHEN read_ceiling = 'goal' THEN 'works' ELSE 'none' END)::proxiima_core.goal_access,
    ALTER COLUMN administers TYPE boolean
        USING (write_ceiling = 'goal');
ALTER TABLE proxiima_core.group_role_projection
    ALTER COLUMN read_ceiling TYPE proxiima_core.memory_ceiling
        USING (CASE read_ceiling WHEN 'goal' THEN 'perspective' ELSE read_ceiling::text END)::proxiima_core.memory_ceiling,
    ALTER COLUMN write_ceiling TYPE proxiima_core.memory_ceiling
        USING (CASE write_ceiling WHEN 'goal' THEN 'perspective' ELSE write_ceiling::text END)::proxiima_core.memory_ceiling,
    ALTER COLUMN goal_access SET NOT NULL,
    ALTER COLUMN administers SET NOT NULL;
ALTER TABLE proxiima_core.group_role_projection
    ADD CONSTRAINT group_role_projection_role_chk CHECK (write_ceiling <= read_ceiling);

ALTER TABLE proxiima_core.delegated_authority_grants
    ADD COLUMN goal_access text,
    ADD COLUMN administers boolean;
ALTER TABLE proxiima_core.delegated_authority_grants
    ALTER COLUMN goal_access TYPE proxiima_core.goal_access
        USING (CASE WHEN read_ceiling = 'goal' THEN 'works' ELSE 'none' END)::proxiima_core.goal_access,
    ALTER COLUMN administers TYPE boolean
        USING (write_ceiling = 'goal');
ALTER TABLE proxiima_core.delegated_authority_grants
    ALTER COLUMN read_ceiling TYPE proxiima_core.memory_ceiling
        USING (CASE read_ceiling WHEN 'goal' THEN 'perspective' ELSE read_ceiling::text END)::proxiima_core.memory_ceiling,
    ALTER COLUMN write_ceiling TYPE proxiima_core.memory_ceiling
        USING (CASE write_ceiling WHEN 'goal' THEN 'perspective' ELSE write_ceiling::text END)::proxiima_core.memory_ceiling,
    ALTER COLUMN goal_access SET NOT NULL,
    ALTER COLUMN administers SET NOT NULL;
ALTER TABLE proxiima_core.delegated_authority_grants
    ADD CONSTRAINT delegated_authority_grants_role_chk CHECK (write_ceiling <= read_ceiling);

-- Goal access grants: ranks 0..3 were the memory ladder, and the pair (4, 4)
-- was a guest who works the Goal. Every other guest reads it. No grant carries
-- owner administration.

ALTER TABLE proxiima_core.goal_access_grant
    DROP CONSTRAINT goal_access_grant_read_ceiling_check,
    DROP CONSTRAINT goal_access_grant_check,
    ADD COLUMN goal_access text,
    ADD COLUMN administers boolean;
ALTER TABLE proxiima_core.goal_access_grant
    ALTER COLUMN goal_access TYPE proxiima_core.goal_access
        USING (CASE WHEN read_ceiling = 4 AND write_ceiling = 4 THEN 'works' ELSE 'reads' END)::proxiima_core.goal_access,
    ALTER COLUMN administers TYPE boolean
        USING (false);
ALTER TABLE proxiima_core.goal_access_grant
    ALTER COLUMN read_ceiling TYPE proxiima_core.memory_ceiling
        USING (CASE read_ceiling WHEN 0 THEN 'none' WHEN 1 THEN 'fact' WHEN 2 THEN 'abstraction'
                                 WHEN 3 THEN 'perspective' WHEN 4 THEN 'perspective' END)::proxiima_core.memory_ceiling,
    ALTER COLUMN write_ceiling TYPE proxiima_core.memory_ceiling
        USING (CASE write_ceiling WHEN 0 THEN 'none' WHEN 1 THEN 'fact' WHEN 2 THEN 'abstraction'
                                  WHEN 3 THEN 'perspective' WHEN 4 THEN 'perspective' END)::proxiima_core.memory_ceiling,
    ALTER COLUMN goal_access SET NOT NULL,
    ALTER COLUMN administers SET NOT NULL;
ALTER TABLE proxiima_core.goal_access_grant
    ADD CONSTRAINT goal_access_grant_role_chk CHECK (write_ceiling <= read_ceiling),
    ADD CONSTRAINT goal_access_grant_no_administers_chk CHECK (NOT administers);
