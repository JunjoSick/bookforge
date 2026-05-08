CREATE TABLE IF NOT EXISTS _migrations (
  version INTEGER PRIMARY KEY,
  name TEXT NOT NULL,
  applied_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS jobs (
  id TEXT PRIMARY KEY,
  input_path TEXT NOT NULL DEFAULT '',
  output_path TEXT NOT NULL DEFAULT '',
  input_hash TEXT NOT NULL,
  source_lang TEXT,
  target_lang TEXT NOT NULL,
  provider TEXT NOT NULL,
  model TEXT NOT NULL,
  base_url TEXT,
  api_key_env TEXT,
  status TEXT NOT NULL,
  config_json TEXT,
  events_path TEXT,
  report_json_path TEXT,
  report_markdown_path TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS segments (
  id TEXT NOT NULL,
  job_id TEXT NOT NULL,
  section_id TEXT NOT NULL,
  ordinal INTEGER NOT NULL,
  source_hash TEXT NOT NULL,
  prompt_version TEXT NOT NULL,
  provider TEXT NOT NULL,
  model TEXT NOT NULL,
  status TEXT NOT NULL,
  attempts INTEGER NOT NULL DEFAULT 0,
  input_tokens INTEGER,
  output_tokens INTEGER,
  cost_estimate REAL,
  error TEXT,
  translated_hash TEXT,
  cache_namespace TEXT NOT NULL DEFAULT '',
  PRIMARY KEY (job_id, id),
  FOREIGN KEY(job_id) REFERENCES jobs(id)
);

CREATE TABLE IF NOT EXISTS translations (
  segment_id TEXT NOT NULL,
  job_id TEXT NOT NULL,
  translated_text TEXT NOT NULL,
  provider TEXT NOT NULL,
  model TEXT NOT NULL,
  prompt_version TEXT NOT NULL,
  created_at TEXT NOT NULL,
  PRIMARY KEY (job_id, segment_id),
  FOREIGN KEY(job_id, segment_id) REFERENCES segments(job_id, id)
);

CREATE TABLE IF NOT EXISTS translation_blocks (
  segment_id TEXT NOT NULL,
  job_id TEXT NOT NULL,
  block_id TEXT NOT NULL,
  translated_text TEXT NOT NULL,
  PRIMARY KEY (job_id, segment_id, block_id),
  FOREIGN KEY(job_id, segment_id) REFERENCES segments(job_id, id)
);

CREATE TABLE IF NOT EXISTS qa_findings (
  id TEXT PRIMARY KEY,
  segment_id TEXT NOT NULL,
  job_id TEXT NOT NULL,
  severity TEXT NOT NULL,
  kind TEXT NOT NULL,
  message TEXT NOT NULL,
  FOREIGN KEY(job_id, segment_id) REFERENCES segments(job_id, id)
);
