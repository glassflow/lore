-- What each code chunk defines and calls, by name, for the definition and callers lookups and the
-- symbol list of search. Calls are not resolved here: a call is tied to a definition when it is
-- looked up. Rows chunked before these columns are empty until their file is chunked again
-- (lore repo push --full).
ALTER TABLE chunks
  ADD COLUMN defines text[] NOT NULL DEFAULT '{}',
  ADD COLUMN calls text[] NOT NULL DEFAULT '{}';
-- Reads run during bulk pushes too. Avoid a large GIN pending list making a 64-name lookup
-- scan recent inserts; pay the index update cost on ingest instead.
CREATE INDEX chunks_defines ON chunks USING gin (defines) WITH (fastupdate = off);
CREATE INDEX chunks_calls ON chunks USING gin (calls) WITH (fastupdate = off);
