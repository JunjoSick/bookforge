use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use bookforge_core::{
    GlossaryScopeKind,
    style::{
        DoNotFields, RegisterFields, StyleSheet, VoiceFields, merge_style_sheets,
        render_style_block, style_fingerprint,
    },
};
use bookforge_store::{JobStore, NewStyleSheet};
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};

#[derive(Debug, Args)]
pub struct StyleArgs {
    #[command(subcommand)]
    command: StyleCommand,
}

#[derive(Debug, Subcommand)]
enum StyleCommand {
    /// List stored style sheets.
    List(ListArgs),
    /// Import a BookForge style TOML file.
    Import(ImportArgs),
    /// Export a stored style sheet to TOML.
    Export(ExportArgs),
    /// Remove all style sheets in a selected scope.
    Clear(ClearArgs),
    /// Show the merged style guidance for a translation context.
    Show(ShowArgs),
}

#[derive(Debug, Args)]
struct ListArgs {
    #[arg(long)]
    language: Option<String>,

    #[arg(long, value_enum)]
    scope: Option<GlossaryScopeKind>,

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
    scope: GlossaryScopeKind,

    #[arg(long)]
    scope_id: Option<String>,

    #[arg(long)]
    language: String,
}

#[derive(Debug, Args)]
struct ClearArgs {
    #[arg(long, value_enum)]
    scope: GlossaryScopeKind,

    #[arg(long)]
    scope_id: Option<String>,
}

#[derive(Debug, Args)]
struct ShowArgs {
    #[arg(long)]
    language: String,

    #[arg(long)]
    book_id: Option<String>,

    #[arg(long)]
    series_id: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct StyleToml {
    meta: StyleTomlMeta,
    #[serde(default)]
    register: RegisterFields,
    #[serde(default)]
    voice: VoiceFields,
    #[serde(default)]
    do_not: DoNotFields,
    #[serde(default)]
    free_text: Option<FreeText>,
}

#[derive(Debug, Deserialize, Serialize)]
struct StyleTomlMeta {
    schema_version: u32,
    target_language: String,
    scope: StyleTomlScope,
}

#[derive(Debug, Deserialize, Serialize)]
struct StyleTomlScope {
    kind: GlossaryScopeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct FreeText {
    #[serde(default)]
    instructions: Option<String>,
}

pub async fn run(args: StyleArgs) -> Result<()> {
    let store = JobStore::open_default()?;
    match args.command {
        StyleCommand::List(args) => list_styles(&store, args),
        StyleCommand::Import(args) => import_style(&store, args),
        StyleCommand::Export(args) => export_style(&store, args),
        StyleCommand::Clear(args) => clear_style(&store, args),
        StyleCommand::Show(args) => show_style(&store, args),
    }
}

pub(crate) fn read_style_file(path: &PathBuf) -> Result<StyleSheet> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read style sheet {}", path.display()))?;
    parse_style_toml(&raw)
        .with_context(|| format!("failed to parse style sheet {}", path.display()))
}

pub(crate) fn parse_style_toml(raw: &str) -> Result<StyleSheet> {
    let parsed: StyleToml = toml::from_str(raw)?;
    if parsed.meta.schema_version != 1 {
        anyhow::bail!(
            "style sheet schema_version {} is not supported (expected 1)",
            parsed.meta.schema_version
        );
    }
    validate_scope(parsed.meta.scope.kind, parsed.meta.scope.id.as_deref())?;
    let free_text_instructions = parsed.free_text.and_then(|f| f.instructions);
    Ok(StyleSheet {
        scope_kind: parsed.meta.scope.kind,
        scope_id: parsed.meta.scope.id,
        target_language: parsed.meta.target_language,
        register: parsed.register,
        voice: parsed.voice,
        free_text_instructions,
        do_not: parsed.do_not,
    })
}

fn validate_scope(scope: GlossaryScopeKind, scope_id: Option<&str>) -> Result<()> {
    match scope {
        GlossaryScopeKind::Global => Ok(()),
        GlossaryScopeKind::Series | GlossaryScopeKind::Book => {
            if scope_id.is_none() || scope_id.is_some_and(|id| id.is_empty()) {
                anyhow::bail!(
                    "style sheet with scope.kind = {:?} requires a non-empty scope.id",
                    scope
                );
            }
            Ok(())
        }
    }
}

fn import_style(store: &JobStore, args: ImportArgs) -> Result<()> {
    let sheet = read_style_file(&args.file)?;
    let content_toml = fs::read_to_string(&args.file)?;
    let one = vec![sheet.clone()];
    let merged = merge_style_sheets(&one);
    let fp = style_fingerprint(merged.as_ref());
    store.upsert_style_sheet(&NewStyleSheet {
        scope_kind: sheet.scope_kind,
        scope_id: sheet.scope_id.as_deref(),
        target_language: &sheet.target_language,
        content_toml: &content_toml,
        fingerprint: &fp,
    })?;
    println!("Imported style sheet for {}.", sheet.target_language);
    Ok(())
}

fn list_styles(store: &JobStore, args: ListArgs) -> Result<()> {
    let stored = store.list_style_sheets(
        args.language.as_deref(),
        args.scope,
        args.scope_id.as_deref(),
    )?;
    if stored.is_empty() {
        println!("No style sheets matched.");
        return Ok(());
    }
    for record in stored {
        println!(
            "id={} scope={:?} scope_id={:?} target={} fingerprint={}",
            record.id,
            record.scope_kind,
            record.scope_id,
            record.target_language,
            &record.fingerprint[..16]
        );
    }
    Ok(())
}

fn export_style(store: &JobStore, args: ExportArgs) -> Result<()> {
    let records = store.list_style_sheets(
        Some(&args.language),
        Some(args.scope),
        args.scope_id.as_deref(),
    )?;
    let record = records
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("no style sheet matched the export filters"))?;
    fs::write(&args.file, record.content_toml)?;
    println!(
        "Exported style sheet id={} to {}",
        record.id,
        args.file.display()
    );
    Ok(())
}

fn clear_style(store: &JobStore, args: ClearArgs) -> Result<()> {
    validate_scope(args.scope, args.scope_id.as_deref())?;
    let count = store.clear_style_scope(args.scope, args.scope_id.as_deref())?;
    println!("Cleared {count} style sheets.");
    Ok(())
}

fn show_style(store: &JobStore, args: ShowArgs) -> Result<()> {
    let records = store.load_active_style_sheets(
        &args.language,
        args.book_id.as_deref(),
        args.series_id.as_deref(),
    )?;
    if records.is_empty() {
        println!("No active style sheets for the requested scope.");
        return Ok(());
    }
    let mut sheets: Vec<StyleSheet> = Vec::new();
    for record in &records {
        match parse_style_toml(&record.content_toml) {
            Ok(s) => sheets.push(s),
            Err(err) => eprintln!(
                "warn: skipping malformed style sheet id={}: {err}",
                record.id
            ),
        }
    }
    let merged = merge_style_sheets(&sheets);
    let block = render_style_block(merged.as_ref());
    if block.is_empty() {
        println!("Active sheets parsed but produced no content.");
    } else {
        print!("{block}");
    }
    Ok(())
}
