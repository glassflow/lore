-- Chunks that define a closed list of values: an enum, a block of constants, a constant array,
-- seed rows, a CHECK ... IN list. "Which X are there" questions are answered by these, and search
-- lifts the first few among its vector hits. Rows chunked before this column are false until their
-- file is chunked again (lore repo push --full).
ALTER TABLE chunks ADD COLUMN enumerates boolean NOT NULL DEFAULT false;
