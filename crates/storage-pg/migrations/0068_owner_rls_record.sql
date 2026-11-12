-- The class and owner column each installer call applied to a table, one row
-- per table. The installers write it in the call that
-- creates the policies; every migration run ends by reinstalling the
-- generated policies of every recorded table.
CREATE TABLE proxiima_core.owner_rls_table (
    schema_name text NOT NULL,
    table_name text NOT NULL,
    class proxiima_core.owner_rls_class NOT NULL,
    owner_column text NOT NULL,
    PRIMARY KEY (schema_name, table_name),
    CONSTRAINT owner_rls_table_schema_chk CHECK (schema_name <> 'proxiima_core')
);

ALTER TABLE proxiima_core.owner_rls_table ENABLE ROW LEVEL SECURITY;
ALTER TABLE proxiima_core.owner_rls_table FORCE ROW LEVEL SECURITY;

COMMENT ON TABLE proxiima_core.owner_rls_table IS 'The owner-RLS class and owner column of each table outside proxiima_core that install_owner_rls or install_owner_rls_table last installed. Written by the installers only, in platform scope; every migration run reinstalls the generated policies from it at its end. A Follows table is governed by owner_follows and has no row here.';

-- The owner policies of each table as core created them, rendered by
-- owner_rls_policy_text. owner_rls_apply_policies, the one creator of those
-- policies, writes it; boot compares it with the catalog.
CREATE TABLE proxiima_core.owner_rls_installed (
    schema_name text NOT NULL,
    table_name text NOT NULL,
    policies text NOT NULL,
    PRIMARY KEY (schema_name, table_name)
);

ALTER TABLE proxiima_core.owner_rls_installed ENABLE ROW LEVEL SECURITY;
ALTER TABLE proxiima_core.owner_rls_installed FORCE ROW LEVEL SECURITY;

COMMENT ON TABLE proxiima_core.owner_rls_installed IS 'The text of the three owner policies of each table at the moment owner_rls_apply_policies created them, as owner_rls_policy_text renders it from the catalog. Written by that function only, in platform scope; boot refuses a table of a composed schema whose policies differ from it.';
