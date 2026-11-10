-- Vectors by content, shared across tenants and sources: the key is sha256 of the embedder name
-- and the exact text that was embedded (see store::index_text), so a repo pushed into a second
-- tenant, or a moved file, finds its vectors here instead of embedding them again. Nothing reads
-- this table except indexing, and no API returns its vectors. Rows reference no tenant, so
-- deleting tenants, sources or documents leaves them in place; a missing row only costs an embed.
CREATE TABLE embedding_cache (
  key bytea PRIMARY KEY,
  embedding vector(384) NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now()
);
