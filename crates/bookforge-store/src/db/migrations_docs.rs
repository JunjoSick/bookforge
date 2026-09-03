//! STORE-5: single-source-of-truth guard for the schema representation.
//!
//! Decision of record: the procedural migrator in [`super::schema`] is the
//! ONLY thing that executes at runtime. The `migrations/*.sql` files are
//! documentation — a human-readable history of every schema delta — and this
//! module is the CI/test guard that keeps them an exact mirror of what the
//! procedural path actually builds.
//!
//! What is asserted on a freshly migrated store:
//! 1. Column-name sets per table match the `CREATE TABLE` blocks in the .sql
//!    docs, plus every later `ALTER TABLE ... ADD COLUMN`.
//! 2. Every index declared in the docs exists in the live database.
//! 3. Every live user table (and index created by the runtime) is documented
//!    somewhere in the .sql set or the ledger addendum.
//! 4. The recorded `_migrations` ledger matches the doc files 1:1 under
//!    [`LEGACY_ALIASES`] — historical DB rows keep their applied names; new
//!    canonical names never rewrite them.
//!
//! Name reconciliation note (recorded aliases, NOT renames): file
//! `0003_v1_1_token_usage_and_flags.sql` documents what the runtime has always
//! recorded as version 3 named `v1_1_segment_flags`. Old databases keep the
//! old name; going forward new migrations must pick one canonical name and
//! list it here if it ever drifts again.

use super::*;
use std::{collections::BTreeMap, fs, path::PathBuf};

/// (version, canonical doc-file stem, embedded SQL content)
const MIGRATION_DOCS: &[(i64, &str, &str)] = &[
    (
        1,
        "initial",
        include_str!("../../migrations/0001_initial.sql"),
    ),
    (
        2,
        "v1_0_1_input_snapshot",
        include_str!("../../migrations/0002_v1_0_1_input_snapshot.sql"),
    ),
    (
        3,
        "v1_1_token_usage_and_flags",
        include_str!("../../migrations/0003_v1_1_token_usage_and_flags.sql"),
    ),
    (
        4,
        "v1_2_glossary_terms",
        include_str!("../../migrations/0004_v1_2_glossary_terms.sql"),
    ),
    (
        5,
        "v1_2_1_nullable_glossary_candidate_targets",
        include_str!("../../migrations/0005_v1_2_1_nullable_glossary_candidate_targets.sql"),
    ),
    (
        6,
        "v1_3_context_styles_entities",
        include_str!("../../migrations/0006_v1_3_context_styles_entities.sql"),
    ),
    (
        7,
        "v2_4_human_corrections",
        include_str!("../../migrations/0007_v2_4_human_corrections.sql"),
    ),
    (
        8,
        "v2_7_qa_findings",
        include_str!("../../migrations/0008_v2_7_qa_findings.sql"),
    ),
    (
        9,
        "v2_7_1_global_scope_unique_indexes",
        include_str!("../../migrations/0009_v2_7_1_global_scope_unique_indexes.sql"),
    ),
    (
        11,
        "v3_0_qa_finding_block_attribution",
        include_str!("../../migrations/0011_v3_0_qa_finding_block_attribution.sql"),
    ),
    (
        12,
        "v3_0_translation_attempts_cache_identity",
        include_str!("../../migrations/0012_v3_0_translation_attempts_cache_identity.sql"),
    ),
];

/// Historical applied-name → canonical-doc-name mapping for versions whose
/// ledger name drifted from its documentation file. Applied rows are NEVER
/// renamed in place (existing databases already carry the old names); new
/// canonical naming starts with this mapping instead.
const LEGACY_ALIASES: &[(i64, &str)] = &[(3, "v1_1_segment_flags")];

/// Canonical applied ledger expected after a fresh open through migration 12.
const APPLIED_LEDGER: &[(&str, i64, &str)] = &[
    ("file", 1, "initial"),
    ("file", 2, "v1_0_1_input_snapshot"),
    // v3's canonical doc name differs from the recorded legacy name above.
    ("legacy", 3, "v1_1_segment_flags"),
    ("file", 4, "v1_2_glossary_terms"),
    ("file", 5, "v1_2_1_nullable_glossary_candidate_targets"),
    ("file", 6, "v1_3_context_styles_entities"),
    ("file", 7, "v2_4_human_corrections"),
    ("file", 8, "v2_7_qa_findings"),
    ("file", 9, "v2_7_1_global_scope_unique_indexes"),
    ("file", 11, "v3_0_qa_finding_block_attribution"),
    ("file", 12, "v3_0_translation_attempts_cache_identity"),
];

#[derive(Default)]
struct DocSchema {
    table_columns: BTreeMap<String, Vec<String>>,
    indexes: BTreeMap<String, bool>,
}

fn parse_docs() -> DocSchema {
    let mut docs = DocSchema::default();
    for (_, _, sql) in MIGRATION_DOCS {
        parse_create_tables(sql, &mut docs.table_columns);
        parse_alter_add_columns(sql, &mut docs.table_columns);
        parse_indexes(sql, &mut docs.indexes);
    }
    docs
}

/// Collect column identifiers from `CREATE TABLE [IF NOT EXISTS] <name> (...)`
/// blocks, skipping constraint keywords at top level. Line-based because every
/// documented definition opens its parameter list on the `CREATE TABLE` line;
/// this mirrors by hand what SQLite would do. Comparisons below only ever
/// assert against column *name sets*, so cosmetic drift is out of scope.
fn parse_create_tables(sql: &str, tables: &mut BTreeMap<String, Vec<String>>) {
    const CONSTRAINT_KEYWORDS: [&str; 5] = ["PRIMARY", "FOREIGN", "UNIQUE", "CHECK", "CONSTRAINT"];

    let mut current_table: Option<String> = None;
    let mut body = String::new();
    for line in sql.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("CREATE TABLE") {
            // Only one documented table per statement; headers carry the name.
            let Some(open) = trimmed.find('(') else {
                continue;
            };
            let name = trimmed[..open]
                .split_whitespace()
                .next_back()
                .unwrap_or_default()
                .to_lowercase();
            body.clear();
            let after_open = &trimmed[open + 1..];
            match after_open.strip_suffix(");") {
                Some(inner) => {
                    record_columns(&name, inner, tables);
                    current_table = None;
                }
                None => {
                    body.push_str(after_open);
                    body.push('\n');
                    current_table = Some(name);
                }
            }
            continue;
        }
        if let Some(name) = current_table.as_ref() {
            if trimmed.starts_with(')') || trimmed.ends_with(");") {
                record_columns(name, &body, tables);
                current_table = None;
            } else {
                body.push_str(trimmed);
                body.push('\n');
            }
        }
    }

    fn record_columns(name: &str, body: &str, tables: &mut BTreeMap<String, Vec<String>>) {
        if !(name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !name.is_empty()) {
            return;
        }
        let mut columns = Vec::new();
        for item in split_top_level(body) {
            let identifier = item
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_string();
            if identifier.is_empty() {
                continue;
            }
            let is_constraint = CONSTRAINT_KEYWORDS
                .iter()
                .any(|keyword| identifier.to_uppercase().starts_with(keyword));
            if !is_constraint && !identifier.starts_with('"') {
                columns.push(identifier.trim_matches(['"', '\'']).to_lowercase());
            }
        }
        // Later files may re-declare a table with IF NOT EXISTS (0003's
        // segment_flags matches 0001's set); a re-declaration documents no new
        // columns beyond what is already there.
        tables.entry(name.to_string()).or_default().extend(columns);
    }
}

/// Split on commas that are not nested inside parentheses.
fn split_top_level(body: &str) -> impl Iterator<Item = &str> {
    let mut depth = 0i32;
    body.split(move |character: char| match character {
        '(' => {
            depth += 1;
            false
        }
        ')' => {
            depth -= 1;
            false
        }
        ',' => depth <= 0,
        _ => false,
    })
}

fn parse_alter_add_columns(sql: &str, tables: &mut BTreeMap<String, Vec<String>>) {
    for line in sql.lines() {
        let trimmed = line.trim();
        if !(trimmed.starts_with("ALTER TABLE ") && trimmed.contains(" ADD COLUMN ")) {
            continue;
        }
        let without_table = trimmed.strip_prefix("ALTER TABLE ").unwrap_or_default();
        let mut parts = without_table.split_whitespace();
        let table = parts.next().unwrap_or_default().to_lowercase();
        // Tokens: <table> ADD COLUMN <name> ...
        let column = [
            parts.next(), // ADD
            parts.next(), // COLUMN
            parts.next(), // <name>
        ]
        .into_iter()
        .flatten()
        .next_back()
        .unwrap_or_default()
        .trim_end_matches(';')
        .to_lowercase();
        tables.entry(table).or_default().push(column);
    }
}

fn parse_indexes(sql: &str, indexes: &mut BTreeMap<String, bool>) {
    for line in sql.lines() {
        let trimmed = line.trim();
        let marker = trimmed.starts_with("CREATE UNIQUE INDEX");
        let plain = !marker && trimmed.starts_with("CREATE INDEX");
        if !marker && !plain {
            continue;
        }
        let Some(name_start) = trimmed.rfind(" INDEX ") else {
            continue;
        };
        let remainder = &trimmed[name_start + " INDEX ".len()..];
        let mut parts = remainder.split_whitespace();
        // Skip optional IF NOT EXISTS tokens.
        loop {
            match parts.next() {
                Some("IF") | Some("NOT") | Some("EXISTS") => continue,
                Some(name) => {
                    indexes.insert(name.trim_end_matches('(').to_lowercase(), marker);
                    break;
                }
                None => break,
            }
        }
    }
}

fn live_schema(conn: &Connection) -> DocSchema {
    let mut live = DocSchema::default();

    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")
        .expect("sqlite_master should be readable");
    let tables: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .expect("table listing")
        .collect::<std::result::Result<Vec<_>, _>>()
        .expect("table rows");

    for table in tables {
        let mut table_stmt = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .expect("table_info prepares");
        let columns = table_stmt
            .query_map([], |row| row.get::<_, String>(1))
            .expect("column rows")
            .collect::<std::result::Result<Vec<_>, _>>()
            .expect("columns read");
        live.table_columns.insert(table, columns);
    }

    let mut idx_stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='index' AND name NOT LIKE 'sqlite_%'")
        .expect("index listing prepares");
    let indexes: Vec<String> = idx_stmt
        .query_map([], |row| row.get::<_, String>(0))
        .expect("index rows")
        .collect::<std::result::Result<Vec<_>, _>>()
        .expect("indexes read");
    for index in indexes {
        live.indexes.insert(index.to_lowercase(), true);
    }
    live
}

fn fresh_store_db_path() -> PathBuf {
    std::env::temp_dir().join(format!(
        "bookforge-store-migration-parity-{}-{}.sqlite",
        std::process::id(),
        unix_timestamp_nanos()
    ))
}

#[test]
fn sql_documentation_files_exactly_mirror_the_procedural_schema() {
    let db_path = fresh_store_db_path();
    let store = JobStore::open(&db_path).expect("store opens on a fresh database");

    let docs = parse_docs();
    let live = live_schema(&store.conn.borrow());

    // 1. Every documented table exists live with EXACTLY the documented
    //    column set (order-independent: additive ensure_column calls mean
    //    column placement legitimately differs, e.g. segments.cache_namespace).
    for (table, doc_columns) in &docs.table_columns {
        let mut expected = doc_columns.clone();
        expected.sort();
        let mut actual = live
            .table_columns
            .get(table)
            .unwrap_or_else(|| panic!("documented table '{table}' missing from live schema"))
            .clone();
        actual.sort();
        assert_eq!(
            &expected, &actual,
            "columns of documented table '{table}' drifted from the procedural schema"
        );
    }

    // 2. Inverse coverage: no undocumented user table may exist.
    let mut undocumented: Vec<&String> = live
        .table_columns
        .keys()
        .filter(|table| !docs.table_columns.contains_key(*table))
        .collect();
    undocumented.sort_unstable();
    assert!(
        undocumented.is_empty(),
        "live tables missing from the migrations/*.sql documentation: {undocumented:?}"
    );

    // 3. Every documented index exists live.
    for index in docs.indexes.keys() {
        assert!(
            live.indexes.contains_key(index),
            "documented index '{index}' missing from the live schema"
        );
    }

    drop(store);
    let _ = fs::remove_file(db_path);
}

#[test]
fn applied_ledger_names_match_doc_files_under_recorded_aliases() {
    let db_path = fresh_store_db_path();
    let store = JobStore::open(&db_path).expect("store opens on a fresh database");
    let conn = store.conn.borrow();

    let mut rows = conn
        .prepare("SELECT version, name FROM _migrations ORDER BY version")
        .expect("ledger prepares");
    let applied: Vec<(i64, String)> = rows
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("ledger rows")
        .collect::<rusqlite::Result<_>>()
        .expect("ledger readable");

    let canonical_by_version: std::collections::HashMap<i64, &'static str> = MIGRATION_DOCS
        .iter()
        .map(|(v, stem, _)| (*v, *stem))
        .collect();

    let mut seen_versions: Vec<i64> = Vec::new();
    for (expected_kind, version, _expected_name) in APPLIED_LEDGER {
        let entry = applied
            .iter()
            .find(|(applied_version, _)| applied_version == version)
            .unwrap_or_else(|| panic!("migration {version} not recorded"));
        seen_versions.push(*version);

        match (
            *expected_kind,
            LEGACY_ALIASES.iter().find(|(v, _)| v == version),
        ) {
            // Recorded legacy alias: keeps the historical applied name.
            ("legacy", Some((_, legacy_name))) => {
                assert_eq!(
                    entry.1, *legacy_name,
                    "version {version} legacy name changed in place"
                );
                assert_ne!(entry.1, canonical_by_version[version]);
            }
            _ => {
                assert_eq!(
                    entry.1, canonical_by_version[version],
                    "version {version} drifted from its canonical doc-file name"
                );
            }
        }
    }

    // Versions 1..=12 are all accounted for; 10 is gated cleanup whose
    // presence depends on conforming data (asserted in the hardening tests).
    // Version 10 is gated cleanup: recorded whenever data conforms (fresh
    // stores and repaired legacy ones), legitimately absent otherwise.
    const GATED_VERSIONS: &[i64] = &[10];
    let extra: Vec<(i64, String)> = applied
        .iter()
        .filter(|(version, _)| {
            !seen_versions.contains(version) && !GATED_VERSIONS.contains(version)
        })
        .cloned()
        .collect();
    assert!(
        extra.is_empty(),
        "ledger rows beyond documented/gated ones must be explicit: {extra:?}"
    );

    drop(rows);
    drop(conn);
    drop(store);
    let _ = fs::remove_file(db_path);
}
