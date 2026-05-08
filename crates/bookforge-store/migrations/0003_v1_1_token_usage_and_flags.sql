ALTER TABLE segments ADD COLUMN tokens_input INTEGER;
ALTER TABLE segments ADD COLUMN tokens_input_cached INTEGER;
ALTER TABLE segments ADD COLUMN tokens_output INTEGER;
ALTER TABLE segments ADD COLUMN tokens_estimated INTEGER NOT NULL DEFAULT 0;

CREATE TABLE IF NOT EXISTS segment_flags (
  id INTEGER PRIMARY KEY,
  job_id TEXT NOT NULL,
  segment_id TEXT NOT NULL,
  kind TEXT NOT NULL,
  note TEXT,
  suggested_source TEXT,
  suggested_target TEXT,
  ingested_at TEXT NOT NULL,
  consumed INTEGER NOT NULL DEFAULT 0,
  FOREIGN KEY (job_id) REFERENCES jobs(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_segment_flags_job ON segment_flags(job_id, consumed);
