CREATE TABLE IF NOT EXISTS glossary_terms (
  id INTEGER PRIMARY KEY,
  scope_kind TEXT NOT NULL CHECK(scope_kind IN ('global', 'series', 'book')),
  scope_id TEXT,
  source_text TEXT NOT NULL,
  target_text TEXT NOT NULL,
  category TEXT NOT NULL CHECK(category IN
    ('person', 'place', 'object', 'invented', 'style', 'phrase', 'other')),
  notes TEXT,
  case_sensitive INTEGER NOT NULL DEFAULT 0,
  always_active INTEGER NOT NULL DEFAULT 0,
  status TEXT NOT NULL CHECK(status IN
    ('user_seeded', 'auto_candidate', 'accepted', 'rejected'))
    DEFAULT 'user_seeded',
  source_language TEXT NOT NULL,
  target_language TEXT NOT NULL,
  source_count INTEGER DEFAULT 0,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE(scope_kind, scope_id, source_text, source_language, target_language)
);

CREATE INDEX IF NOT EXISTS idx_glossary_lookup
  ON glossary_terms(source_language, target_language, scope_kind, scope_id, status);

ALTER TABLE jobs ADD COLUMN book_id TEXT;
ALTER TABLE jobs ADD COLUMN series_id TEXT;
