-- v2.7.1: global-scope identity + jobs listing index.
--
-- This file documents the schema delta; it is NOT executed at runtime
-- (schema.rs builds the schema procedurally — see STORE-5). Migration 9 in
-- schema.rs applies the equivalent changes and is gated behind an
-- `migration_applied` check because it drives a one-time data cleanup.
--
-- 1. STORE-13: table-level UNIQUE(scope_kind, scope_id, ...) cannot enforce
--    identity for global rows because SQL compares NULLs as distinct, so two
--    processes inserting the same global row concurrently could both succeed.
--    The Rust migration first removes duplicates accumulated by the old
--    constraints (keeping the most recently updated row), then these partial
--    unique indexes enforce identity for global scope:
CREATE UNIQUE INDEX IF NOT EXISTS ux_glossary_terms_global_identity
  ON glossary_terms(source_text, source_language, target_language)
  WHERE scope_kind = 'global';

CREATE UNIQUE INDEX IF NOT EXISTS ux_style_sheets_global_identity
  ON style_sheets(target_language)
  WHERE scope_kind = 'global';

CREATE UNIQUE INDEX IF NOT EXISTS ux_entities_global_identity
  ON entities(source_name, source_language, target_language)
  WHERE scope_kind = 'global';

-- 2. STORE-16: the dashboard/watch job lists sort by creation date on every
--    refresh; without this index each refresh sorted the whole jobs table.
CREATE INDEX IF NOT EXISTS idx_jobs_created_at ON jobs(created_at);
