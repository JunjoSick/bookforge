use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use bookforge_core::{GlossaryCategory, GlossaryScopeKind, GlossaryStatus, GlossaryTerm};
use bookforge_store::{GlossaryFilter, JobStore};
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};

#[derive(Debug, Args)]
pub struct GlossaryArgs {
    #[command(subcommand)]
    command: GlossaryCommand,
}

#[derive(Debug, Subcommand)]
enum GlossaryCommand {
    List(ListArgs),
    Add(AddArgs),
    Remove(RemoveArgs),
    Clear(ClearArgs),
    Import(ImportArgs),
    Export(ExportArgs),
}

#[derive(Debug, Args)]
struct ListArgs {
    #[arg(long)]
    book: Option<String>,

    #[arg(long)]
    series: Option<String>,

    #[arg(long)]
    language: Option<String>,
}

#[derive(Debug, Args)]
struct AddArgs {
    source: String,
    target: String,

    #[arg(long, value_enum)]
    category: GlossaryCategory,

    #[arg(long, value_enum, default_value_t = GlossaryScopeKind::Global)]
    scope: GlossaryScopeKind,

    #[arg(long)]
    scope_id: Option<String>,

    #[arg(long)]
    source_lang: Option<String>,

    #[arg(long)]
    target_lang: Option<String>,

    #[arg(long)]
    case_sensitive: bool,

    #[arg(long)]
    always_active: bool,

    #[arg(long)]
    notes: Option<String>,
}

#[derive(Debug, Args)]
struct RemoveArgs {
    id: i64,
}

#[derive(Debug, Args)]
struct ClearArgs {
    #[arg(long, value_enum)]
    scope: GlossaryScopeKind,

    #[arg(long)]
    scope_id: Option<String>,
}

#[derive(Debug, Args)]
struct ImportArgs {
    file: PathBuf,
}

#[derive(Debug, Args)]
struct ExportArgs {
    file: PathBuf,

    #[arg(long, value_enum)]
    scope: Option<GlossaryScopeKind>,

    #[arg(long)]
    scope_id: Option<String>,

    #[arg(long)]
    language: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct GlossaryToml {
    meta: GlossaryTomlMeta,
    #[serde(default, rename = "term")]
    terms: Vec<GlossaryTomlTerm>,
}

#[derive(Debug, Deserialize, Serialize)]
struct GlossaryTomlMeta {
    schema_version: u32,
    source_language: String,
    target_language: String,
    scope: GlossaryTomlScope,
}

#[derive(Debug, Deserialize, Serialize)]
struct GlossaryTomlScope {
    kind: GlossaryScopeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct GlossaryTomlTerm {
    source: String,
    target: String,
    category: GlossaryCategory,
    #[serde(default)]
    case_sensitive: bool,
    #[serde(default)]
    always_active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    notes: Option<String>,
    #[serde(default = "default_user_seeded")]
    status: GlossaryStatus,
    #[serde(default)]
    source_count: usize,
}

pub async fn run(args: GlossaryArgs) -> Result<()> {
    let store = JobStore::open_default()?;
    match args.command {
        GlossaryCommand::List(args) => list_terms(&store, args),
        GlossaryCommand::Add(args) => add_term(&store, args),
        GlossaryCommand::Remove(args) => remove_term(&store, args),
        GlossaryCommand::Clear(args) => clear_terms(&store, args),
        GlossaryCommand::Import(args) => import_terms(&store, args),
        GlossaryCommand::Export(args) => export_terms(&store, args),
    }
}

pub(crate) fn read_glossary_file(path: &PathBuf) -> Result<Vec<GlossaryTerm>> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read glossary file {}", path.display()))?;
    let parsed: GlossaryToml = toml::from_str(&raw)
        .with_context(|| format!("failed to parse glossary TOML {}", path.display()))?;
    glossary_toml_to_terms(parsed)
}

fn import_terms(store: &JobStore, args: ImportArgs) -> Result<()> {
    let terms = read_glossary_file(&args.file)?;
    let imported = store.upsert_glossary_terms(&terms)?;
    println!("Imported {imported} glossary terms.");
    Ok(())
}

fn export_terms(store: &JobStore, args: ExportArgs) -> Result<()> {
    let (source_language, target_language) = match args.language.as_deref() {
        Some(language) => {
            let (source, target) = parse_language_pair(language)?;
            (Some(source), Some(target))
        }
        None => (None, None),
    };
    let terms = store.list_glossary_terms(GlossaryFilter {
        scope_kind: args.scope,
        scope_id: args.scope_id.as_deref(),
        source_language: source_language.as_deref(),
        target_language: target_language.as_deref(),
        active_only: false,
    })?;
    if terms.is_empty() {
        anyhow::bail!("no glossary terms matched the export filters");
    }

    let output = terms_to_glossary_toml(&terms)?;
    fs::write(&args.file, toml::to_string_pretty(&output)?)?;
    println!("Exported {} glossary terms.", output.terms.len());
    Ok(())
}

fn list_terms(store: &JobStore, args: ListArgs) -> Result<()> {
    let (source_language, target_language) = match args.language.as_deref() {
        Some(language) => {
            let (source, target) = parse_language_pair(language)?;
            (Some(source), Some(target))
        }
        None => (None, None),
    };
    let (scope_kind, scope_id) = if let Some(book) = args.book.as_deref() {
        (Some(GlossaryScopeKind::Book), Some(book))
    } else if let Some(series) = args.series.as_deref() {
        (Some(GlossaryScopeKind::Series), Some(series))
    } else {
        (None, None)
    };
    let terms = store.list_glossary_terms(GlossaryFilter {
        scope_kind,
        scope_id,
        source_language: source_language.as_deref(),
        target_language: target_language.as_deref(),
        active_only: false,
    })?;
    if terms.is_empty() {
        println!("No glossary terms.");
        return Ok(());
    }
    for term in terms {
        println!(
            "{}\t{}\t{}\t{}\t{}\t{} -> {}",
            term.id.unwrap_or_default(),
            term.source_language,
            term.target_language,
            term.scope_kind,
            term.scope_id.as_deref().unwrap_or("-"),
            term.source_text,
            term.target_text
        );
    }
    Ok(())
}

fn add_term(store: &JobStore, args: AddArgs) -> Result<()> {
    let source_language = args
        .source_lang
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("--source-lang is required for glossary add"))?;
    let target_language = args
        .target_lang
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("--target-lang is required for glossary add"))?;
    validate_scope(args.scope, args.scope_id.as_deref())?;
    let term = GlossaryTerm {
        id: None,
        scope_kind: args.scope,
        scope_id: normalized_scope_id(args.scope, args.scope_id),
        source_text: args.source,
        target_text: args.target,
        category: args.category,
        notes: args.notes,
        case_sensitive: args.case_sensitive,
        always_active: args.always_active,
        status: GlossaryStatus::UserSeeded,
        source_language: source_language.to_string(),
        target_language: target_language.to_string(),
        source_count: 0,
    };
    let id = store.add_glossary_term(&term)?;
    println!("Glossary term {id} saved.");
    Ok(())
}

fn remove_term(store: &JobStore, args: RemoveArgs) -> Result<()> {
    let removed = store.remove_glossary_term(args.id)?;
    println!("Removed {removed} glossary terms.");
    Ok(())
}

fn clear_terms(store: &JobStore, args: ClearArgs) -> Result<()> {
    validate_scope(args.scope, args.scope_id.as_deref())?;
    let removed = store.clear_glossary_scope(args.scope, args.scope_id.as_deref())?;
    println!("Removed {removed} glossary terms.");
    Ok(())
}

fn glossary_toml_to_terms(parsed: GlossaryToml) -> Result<Vec<GlossaryTerm>> {
    if parsed.meta.schema_version != 1 {
        anyhow::bail!(
            "unsupported glossary schema_version {}; expected 1",
            parsed.meta.schema_version
        );
    }
    validate_scope(parsed.meta.scope.kind, parsed.meta.scope.id.as_deref())?;
    let scope_id = normalized_scope_id(parsed.meta.scope.kind, parsed.meta.scope.id);
    let terms = parsed
        .terms
        .into_iter()
        .map(|term| GlossaryTerm {
            id: None,
            scope_kind: parsed.meta.scope.kind,
            scope_id: scope_id.clone(),
            source_text: term.source,
            target_text: term.target,
            category: term.category,
            notes: term.notes,
            case_sensitive: term.case_sensitive,
            always_active: term.always_active,
            status: term.status,
            source_language: parsed.meta.source_language.clone(),
            target_language: parsed.meta.target_language.clone(),
            source_count: term.source_count,
        })
        .collect::<Vec<_>>();
    Ok(terms)
}

fn terms_to_glossary_toml(terms: &[GlossaryTerm]) -> Result<GlossaryToml> {
    let Some(first) = terms.first() else {
        anyhow::bail!("cannot export an empty glossary");
    };
    let same_tuple = terms.iter().all(|term| {
        term.scope_kind == first.scope_kind
            && term.scope_id == first.scope_id
            && term.source_language == first.source_language
            && term.target_language == first.target_language
    });
    if !same_tuple {
        anyhow::bail!(
            "export matched multiple scope/language tuples; narrow with --scope, --scope-id, and --language"
        );
    }
    Ok(GlossaryToml {
        meta: GlossaryTomlMeta {
            schema_version: 1,
            source_language: first.source_language.clone(),
            target_language: first.target_language.clone(),
            scope: GlossaryTomlScope {
                kind: first.scope_kind,
                id: first.scope_id.clone(),
            },
        },
        terms: terms
            .iter()
            .map(|term| GlossaryTomlTerm {
                source: term.source_text.clone(),
                target: term.target_text.clone(),
                category: term.category,
                case_sensitive: term.case_sensitive,
                always_active: term.always_active,
                notes: term.notes.clone(),
                status: term.status,
                source_count: term.source_count,
            })
            .collect(),
    })
}

fn validate_scope(scope: GlossaryScopeKind, scope_id: Option<&str>) -> Result<()> {
    match scope {
        GlossaryScopeKind::Global => Ok(()),
        GlossaryScopeKind::Series | GlossaryScopeKind::Book => {
            if scope_id.is_some_and(|id| !id.trim().is_empty()) {
                Ok(())
            } else {
                anyhow::bail!("--scope-id is required for {scope} glossary terms")
            }
        }
    }
}

fn normalized_scope_id(scope: GlossaryScopeKind, scope_id: Option<String>) -> Option<String> {
    if scope == GlossaryScopeKind::Global {
        None
    } else {
        scope_id
    }
}

fn parse_language_pair(value: &str) -> Result<(String, String)> {
    for delimiter in ["->", ":", "/"] {
        if let Some((source, target)) = value.split_once(delimiter) {
            let source = source.trim();
            let target = target.trim();
            if !source.is_empty() && !target.is_empty() {
                return Ok((source.to_string(), target.to_string()));
            }
        }
    }
    anyhow::bail!("language must be formatted as SOURCE->TARGET, SOURCE:TARGET, or SOURCE/TARGET")
}

fn default_user_seeded() -> GlossaryStatus {
    GlossaryStatus::UserSeeded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_glossary_toml() {
        let parsed: GlossaryToml = toml::from_str(
            r#"
[meta]
schema_version = 1
source_language = "English"
target_language = "Italian"

[meta.scope]
kind = "book"
id = "fellowship"

[[term]]
source = "Aragorn"
target = "Aragorn"
category = "person"
case_sensitive = true
"#,
        )
        .expect("TOML should parse");

        let terms = glossary_toml_to_terms(parsed).expect("terms should convert");
        assert_eq!(terms.len(), 1);
        assert_eq!(terms[0].scope_kind, GlossaryScopeKind::Book);
        assert_eq!(terms[0].scope_id.as_deref(), Some("fellowship"));
        assert!(terms[0].case_sensitive);
    }

    #[test]
    fn parses_language_pair() {
        assert_eq!(
            parse_language_pair("English->Italian").expect("pair"),
            ("English".to_string(), "Italian".to_string())
        );
    }

    #[test]
    fn exported_toml_reimports_same_term_fields() {
        let terms = vec![GlossaryTerm {
            id: Some(7),
            scope_kind: GlossaryScopeKind::Series,
            scope_id: Some("lotr".to_string()),
            source_text: "the One Ring".to_string(),
            target_text: "l'Unico Anello".to_string(),
            category: GlossaryCategory::Object,
            notes: Some("canonical series term".to_string()),
            case_sensitive: false,
            always_active: false,
            status: GlossaryStatus::UserSeeded,
            source_language: "English".to_string(),
            target_language: "Italian".to_string(),
            source_count: 12,
        }];

        let exported = terms_to_glossary_toml(&terms).expect("terms should export");
        let encoded = toml::to_string_pretty(&exported).expect("TOML should encode");
        let reparsed: GlossaryToml = toml::from_str(&encoded).expect("TOML should parse");
        let imported = glossary_toml_to_terms(reparsed).expect("terms should import");

        assert_eq!(imported.len(), 1);
        assert_eq!(imported[0].scope_kind, terms[0].scope_kind);
        assert_eq!(imported[0].scope_id, terms[0].scope_id);
        assert_eq!(imported[0].source_text, terms[0].source_text);
        assert_eq!(imported[0].target_text, terms[0].target_text);
        assert_eq!(imported[0].category, terms[0].category);
        assert_eq!(imported[0].notes, terms[0].notes);
        assert_eq!(imported[0].source_count, terms[0].source_count);
    }
}
