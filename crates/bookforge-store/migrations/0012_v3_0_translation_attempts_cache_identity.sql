-- v3.0 (translation-state audit): structured cache identity + append-only
-- translation provenance ledger.
--
-- This file documents the schema delta; it is NOT executed at runtime
-- (schema.rs builds the schema procedurally — see STORE-5). Migration 12 in
-- schema.rs applies the equivalent change, gated behind a `migration_applied`
-- check so reopened stores never take a write lock just to re-record it.
--
-- `segments.cache_fingerprint` persists the single structured cache identity
-- (bookforge_core::segment::CacheIdentity, versioned by
-- CACHE_IDENTITY_SCHEMA_VERSION) that captures every output-affecting input —
-- source hash, effective provider/model, languages, prompt template version
-- and extras, segmentation, context window/budget/scope, the strict-context
-- completion fence, batch shape, compact-prompt mode, style/glossary/entity
-- fingerprints, bilingual rendering, and provider runtime request shaping.
-- Legacy rows carry the empty string and are permanently ineligible for cache
-- reuse, so ambiguous old entries can never match a new lookup.
--
-- `jobs.cache_policy_json` persists the durable cache-policy record
-- (CachePolicySnapshot) that is not part of the historical RunConfigSnapshot
-- surface — currently the strict-context choice. Absent (NULL) reads back the
-- conservative default (unknown strictness), which is hashed distinctly from
-- either explicit value so legacy jobs can never reuse incompatible cache.
--
-- `translation_attempts` is the append-only attempt ledger: one row per
-- provider/QA attempt (phase, attempt ordinal, effective provider/model,
-- outcome, tokens, cost), written transactionally with the final translation
-- state. Rows are inserted once and never updated (a BEFORE UPDATE trigger
-- rejects in-place edits); deletion is reserved for job retention/prune.
-- The composite foreign key pins each attempt to the segment row that owns
-- it, and the unique (job, segment, attempt_ordinal) constraint makes the
-- ordinal sequence immutable and monotonic. Aggregates prefer the ledger over
-- the legacy segments token columns while still reading legacy rows for jobs
-- that predate the ledger.
ALTER TABLE segments ADD COLUMN cache_fingerprint TEXT NOT NULL DEFAULT '';
ALTER TABLE jobs ADD COLUMN cache_policy_json TEXT;

CREATE TABLE IF NOT EXISTS translation_attempts (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  job_id TEXT NOT NULL,
  segment_id TEXT NOT NULL,
  batch_id TEXT,
  phase TEXT NOT NULL CHECK(phase IN
    ('primary', 'fallback', 'repair', 'qa', 'double_check')),
  attempt_ordinal INTEGER NOT NULL,
  provider TEXT NOT NULL,
  model TEXT NOT NULL,
  outcome TEXT NOT NULL CHECK(outcome IN
    ('success', 'failure', 'partial', 'skipped')),
  error TEXT,
  input_tokens INTEGER,
  input_cached_tokens INTEGER,
  output_tokens INTEGER,
  cost_estimate REAL,
  created_at TEXT NOT NULL,
  UNIQUE(job_id, segment_id, attempt_ordinal),
  FOREIGN KEY(job_id, segment_id) REFERENCES segments(job_id, id)
);

CREATE TRIGGER IF NOT EXISTS translation_attempts_immutable_update
BEFORE UPDATE ON translation_attempts
BEGIN
  SELECT RAISE(ABORT, 'translation_attempts is append-only');
END;

CREATE INDEX IF NOT EXISTS idx_translation_attempts_segment
  ON translation_attempts(job_id, segment_id);
CREATE INDEX IF NOT EXISTS idx_translation_attempts_job
  ON translation_attempts(job_id);
