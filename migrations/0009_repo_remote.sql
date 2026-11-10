-- A repo source can name a remote the server fetches by itself (POST /v1/repos/{id}/sync, the
-- poller, the GitHub webhook). The last_sync_* columns say how the last fetch went.
ALTER TABLE sources
  ADD COLUMN remote_url text,
  ADD COLUMN last_sync_at timestamptz,
  ADD COLUMN last_sync_commit text,
  ADD COLUMN last_sync_error text;
