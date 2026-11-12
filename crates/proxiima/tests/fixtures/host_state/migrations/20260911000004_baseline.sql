CREATE SCHEMA IF NOT EXISTS host_fixture;

CREATE TABLE host_fixture.execution (
    invocation_id uuid PRIMARY KEY,
    owner_id uuid NOT NULL,
    status text NOT NULL,
    version integer NOT NULL,
    principal_id uuid,
    maintenance_origin boolean NOT NULL,
    payload_saved_by_id uuid,
    CONSTRAINT execution_status_check CHECK (status IN ('created', 'finalized')),
    CONSTRAINT execution_origin_shape CHECK (
        (maintenance_origin AND principal_id IS NULL)
        OR
        (NOT maintenance_origin AND principal_id IS NOT NULL)
    )
);

CREATE INDEX execution_by_owner ON host_fixture.execution (owner_id);

CREATE TABLE host_fixture.auxiliary (
    invocation_id uuid PRIMARY KEY,
    owner_id uuid NOT NULL
);

CREATE TABLE host_fixture.deferred_reference (
    invocation_id uuid PRIMARY KEY,
    execution_id uuid NOT NULL,
    owner_id uuid NOT NULL,
    CONSTRAINT deferred_reference_execution_fk
        FOREIGN KEY (execution_id) REFERENCES host_fixture.execution(invocation_id)
        DEFERRABLE INITIALLY DEFERRED
);

SELECT proxiima_core.install_owner_rls_table('host_fixture', 'execution', 'owner_id');
SELECT proxiima_core.install_owner_rls_table('host_fixture', 'auxiliary', 'owner_id');
SELECT proxiima_core.install_owner_rls_table('host_fixture', 'deferred_reference', 'owner_id');
