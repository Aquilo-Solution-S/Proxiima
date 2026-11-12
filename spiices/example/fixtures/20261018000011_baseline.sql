-- proxiima-example baseline: a test and tutorial fixture, not a product.
CREATE SCHEMA proxiima_example;
CREATE TABLE proxiima_example.notebooks (
    notebook_id uuid PRIMARY KEY,
    owner_id uuid NOT NULL,
    name text NOT NULL
);
CREATE TABLE proxiima_example.settings (
    key text PRIMARY KEY,
    value text NOT NULL
);
CREATE TABLE proxiima_example.note_v1 (
    t uuid PRIMARY KEY,
    notebook_id uuid NOT NULL,
    key text NOT NULL,
    body text NOT NULL,
    written_at timestamptz NOT NULL
);
CREATE TABLE proxiima_example.digest_v1 (
    t uuid PRIMARY KEY,
    notebook_id uuid NOT NULL,
    summary text NOT NULL
);
SELECT proxiima_core.install_sidecar('proxiima_example', 'note_v1', 'proxiima-example');
SELECT proxiima_core.install_sidecar('proxiima_example', 'digest_v1', 'proxiima-example');
-- owner-rls: begin
SELECT proxiima_core.install_owner_rls_table('proxiima_example', 'notebooks', 'owner_id');
SELECT proxiima_core.install_owner_rls_table('proxiima_example', 'settings', 'ownerless');
SELECT proxiima_core.assert_owner_rls_census('proxiima_example');
-- owner-rls: end
