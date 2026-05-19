-- v1.3 milestone (ROADMAP §6): style sheets and entity sheets.
--
-- style_sheets keeps the full TOML body verbatim: the rendered prompt
-- block is recomputed from TOML at run time, and the schema is small
-- enough that a normalized column-per-field layout would churn faster
-- than the data evolves.

CREATE TABLE IF NOT EXISTS style_sheets (
  id INTEGER PRIMARY KEY,
  scope_kind TEXT NOT NULL CHECK(scope_kind IN ('global', 'series', 'book')),
  scope_id TEXT,
  target_language TEXT NOT NULL,
  content_toml TEXT NOT NULL,
  fingerprint TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE(scope_kind, scope_id, target_language)
);

CREATE INDEX IF NOT EXISTS idx_style_lookup
  ON style_sheets(target_language, scope_kind, scope_id);

-- Entities (PR3) — schema from ROADMAP §6.6.
CREATE TABLE IF NOT EXISTS entities (
  id INTEGER PRIMARY KEY,
  scope_kind TEXT NOT NULL CHECK(scope_kind IN ('global', 'series', 'book')),
  scope_id TEXT,
  source_name TEXT NOT NULL,
  target_name TEXT NOT NULL,
  gender_target TEXT CHECK(gender_target IS NULL OR gender_target IN ('m', 'f', 'n')),
  role TEXT,
  notes TEXT,
  source_language TEXT NOT NULL,
  target_language TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE(scope_kind, scope_id, source_name, source_language, target_language)
);

CREATE INDEX IF NOT EXISTS idx_entity_lookup
  ON entities(source_language, target_language, scope_kind, scope_id);
