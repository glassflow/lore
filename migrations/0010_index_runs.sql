-- One row per index run of a repo source (a bundle push, a sync, later a Kubernetes Job). The
-- indexer writes its progress here through its own connection, so the row is visible while
-- the index transaction is still open, to any lore process, and survives a restart.
-- A run that is not `done` or `failed` and whose updated_at is old was killed: readers call it
-- stale and the row is not rewritten.
CREATE TABLE index_runs (
  id bigserial PRIMARY KEY,
  source_id uuid NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
  tenant_id uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  commit text NOT NULL,
  "full" boolean NOT NULL DEFAULT false,
  stage text NOT NULL CHECK (stage IN ('queued','cloning','indexing','done','failed')),
  files_done int NOT NULL DEFAULT 0,
  files_total int,
  chunks int NOT NULL DEFAULT 0,
  chunks_embedded int NOT NULL DEFAULT 0,
  embed_ms bigint NOT NULL DEFAULT 0,
  error text,
  started_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now(),
  finished_at timestamptz
);
-- The latest run of one source.
CREATE INDEX index_runs_source_latest ON index_runs (source_id, id DESC);
-- The latest runs of every tenant, and the runs still going, for the admin page.
CREATE INDEX index_runs_latest ON index_runs (id DESC);
CREATE INDEX index_runs_live ON index_runs (updated_at) WHERE stage NOT IN ('done','failed');
-- The tenant foreign key: deleting a tenant removes its runs.
CREATE INDEX index_runs_tenant ON index_runs (tenant_id);
