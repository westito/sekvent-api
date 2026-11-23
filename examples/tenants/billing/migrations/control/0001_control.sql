CREATE TABLE tenants (
    id uuid NOT NULL,
    subdomain text NOT NULL,
    display_name text NOT NULL,
    state text NOT NULL,
    provision_step text NOT NULL DEFAULT '',
    db_cluster text NOT NULL DEFAULT 'main',
    db_name text NOT NULL,
    db_role text NOT NULL,
    db_role_key_id text NOT NULL,
    db_role_generation integer NOT NULL DEFAULT 1,
    schema_version bigint,
    created_at timestamptz NOT NULL,
    updated_at timestamptz NOT NULL,
    CONSTRAINT tenants_pkey PRIMARY KEY (id),
    CONSTRAINT tenants_db_role_key UNIQUE (db_cluster, db_role),
    CONSTRAINT tenants_state_check
        CHECK (state IN ('provisioning', 'active', 'suspended', 'purging', 'purged')),
    CONSTRAINT tenants_db_role_generation_check CHECK (db_role_generation >= 1)
);

CREATE UNIQUE INDEX tenants_db_name_key ON tenants (db_cluster, db_name) WHERE state <> 'purged';

CREATE TABLE tenant_hosts (
    host text NOT NULL,
    tenant_id uuid NOT NULL,
    is_primary boolean NOT NULL DEFAULT false,
    CONSTRAINT tenant_hosts_pkey PRIMARY KEY (host),
    CONSTRAINT tenant_hosts_tenant_id_fkey
        FOREIGN KEY (tenant_id) REFERENCES tenants (id) ON DELETE CASCADE
);

CREATE UNIQUE INDEX tenant_hosts_primary_key ON tenant_hosts (tenant_id) WHERE is_primary;

CREATE INDEX tenant_hosts_tenant_id_idx ON tenant_hosts (tenant_id);

CREATE TABLE admin_audit (
    token_id uuid NOT NULL,
    occurred_at timestamptz NOT NULL,
    operation text NOT NULL,
    actor text NOT NULL,
    outcome text NOT NULL DEFAULT 'started',
    tenant_id uuid,
    CONSTRAINT admin_audit_pkey PRIMARY KEY (token_id)
);
