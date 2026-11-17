-- Documents of kind 'decision' carry who decided, when and where in their metadata.
ALTER TABLE documents DROP CONSTRAINT documents_kind_check;
ALTER TABLE documents ADD CONSTRAINT documents_kind_check
  CHECK (kind IN ('code','doc','session','note','decision'));
