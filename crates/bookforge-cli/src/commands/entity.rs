use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use bookforge_core::{
    GlossaryScopeKind,
    entity::{
        Entity, EntityGender, entities_fingerprint, merge_scope_entities,
        render_entity_agreement_block,
    },
};
use bookforge_store::{JobStore, NewEntity};
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};

#[derive(Debug, Args)]
pub struct EntitiesArgs {
    #[command(subcommand)]
    command: EntitiesCommand,
}

#[derive(Debug, Subcommand)]
enum EntitiesCommand {
    List(ListArgs),
    Import(ImportArgs),
    Clear(ClearArgs),
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
struct ClearArgs {
    #[arg(long, value_enum)]
    scope: GlossaryScopeKind,

    #[arg(long)]
    scope_id: Option<String>,
}

#[derive(Debug, Args)]
struct ShowArgs {
    #[arg(long)]
    source_language: String,

    #[arg(long)]
    target_language: String,

    #[arg(long)]
    book_id: Option<String>,

    #[arg(long)]
    series_id: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct EntitiesToml {
    meta: EntitiesTomlMeta,
    #[serde(default, rename = "entity")]
    entities: Vec<EntitiesTomlEntity>,
}

#[derive(Debug, Deserialize, Serialize)]
struct EntitiesTomlMeta {
    schema_version: u32,
    source_language: String,
    target_language: String,
    scope: EntitiesTomlScope,
}

#[derive(Debug, Deserialize, Serialize)]
struct EntitiesTomlScope {
    kind: GlossaryScopeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct EntitiesTomlEntity {
    source_name: String,
    target_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    gender_target: Option<EntityGender>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    notes: Option<String>,
}

pub async fn run(args: EntitiesArgs) -> Result<()> {
    let store = JobStore::open_default()?;
    match args.command {
        EntitiesCommand::List(args) => list_entities(&store, args),
        EntitiesCommand::Import(args) => import_entities(&store, args),
        EntitiesCommand::Clear(args) => clear_entities(&store, args),
        EntitiesCommand::Show(args) => show_entities(&store, args),
    }
}

pub(crate) fn read_entities_file(path: &PathBuf) -> Result<Vec<Entity>> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read entities file {}", path.display()))?;
    parse_entities_toml(&raw)
        .with_context(|| format!("failed to parse entities file {}", path.display()))
}

pub(crate) fn parse_entities_toml(raw: &str) -> Result<Vec<Entity>> {
    let parsed: EntitiesToml = toml::from_str(raw)?;
    if parsed.meta.schema_version != 1 {
        anyhow::bail!(
            "entities schema_version {} is not supported (expected 1)",
            parsed.meta.schema_version
        );
    }
    validate_scope(parsed.meta.scope.kind, parsed.meta.scope.id.as_deref())?;
    let mut out = Vec::with_capacity(parsed.entities.len());
    for entity in parsed.entities {
        if entity.source_name.is_empty() || entity.target_name.is_empty() {
            anyhow::bail!("each entity requires non-empty source_name and target_name");
        }
        out.push(Entity {
            id: None,
            scope_kind: parsed.meta.scope.kind,
            scope_id: parsed.meta.scope.id.clone(),
            source_name: entity.source_name,
            target_name: entity.target_name,
            gender_target: entity.gender_target,
            role: entity.role,
            notes: entity.notes,
            source_language: parsed.meta.source_language.clone(),
            target_language: parsed.meta.target_language.clone(),
        });
    }
    Ok(out)
}

fn validate_scope(scope: GlossaryScopeKind, scope_id: Option<&str>) -> Result<()> {
    match scope {
        GlossaryScopeKind::Global => Ok(()),
        GlossaryScopeKind::Series | GlossaryScopeKind::Book => {
            if scope_id.is_none() || scope_id.is_some_and(|id| id.is_empty()) {
                anyhow::bail!(
                    "entities file with scope.kind = {:?} requires a non-empty scope.id",
                    scope
                );
            }
            Ok(())
        }
    }
}

fn import_entities(store: &JobStore, args: ImportArgs) -> Result<()> {
    let entities = read_entities_file(&args.file)?;
    let count = upsert_entities(store, &entities)?;
    println!("Imported {count} entity rows.");
    Ok(())
}

pub(crate) fn upsert_entities(store: &JobStore, entities: &[Entity]) -> Result<usize> {
    let rows: Vec<NewEntity<'_>> = entities
        .iter()
        .map(|e| NewEntity {
            scope_kind: e.scope_kind,
            scope_id: e.scope_id.as_deref(),
            source_name: e.source_name.as_str(),
            target_name: e.target_name.as_str(),
            gender_target: e.gender_target,
            role: e.role.as_deref(),
            notes: e.notes.as_deref(),
            source_language: e.source_language.as_str(),
            target_language: e.target_language.as_str(),
        })
        .collect();
    let written = store.upsert_entities(&rows)?;
    Ok(written)
}

fn list_entities(store: &JobStore, args: ListArgs) -> Result<()> {
    let stored = store.list_entities(
        None,
        args.language.as_deref(),
        args.scope,
        args.scope_id.as_deref(),
    )?;
    if stored.is_empty() {
        println!("No entities matched.");
        return Ok(());
    }
    for record in stored {
        println!(
            "id={} scope={:?} scope_id={:?} {} -> {} ({})",
            record.id,
            record.scope_kind,
            record.scope_id,
            record.source_name,
            record.target_name,
            record
                .gender_target
                .map(|g| g.as_label())
                .unwrap_or("unspecified")
        );
    }
    Ok(())
}

fn clear_entities(store: &JobStore, args: ClearArgs) -> Result<()> {
    let count = store.clear_entities_scope(args.scope, args.scope_id.as_deref())?;
    println!("Cleared {count} entity rows.");
    Ok(())
}

fn show_entities(store: &JobStore, args: ShowArgs) -> Result<()> {
    let records = store.load_active_entities(
        &args.source_language,
        &args.target_language,
        args.book_id.as_deref(),
        args.series_id.as_deref(),
    )?;
    if records.is_empty() {
        println!("No active entities for the requested scope.");
        return Ok(());
    }
    let entities: Vec<Entity> = records
        .into_iter()
        .map(|r| Entity {
            id: Some(r.id),
            scope_kind: r.scope_kind,
            scope_id: r.scope_id,
            source_name: r.source_name,
            target_name: r.target_name,
            gender_target: r.gender_target,
            role: r.role,
            notes: r.notes,
            source_language: r.source_language,
            target_language: r.target_language,
        })
        .collect();
    let merged = merge_scope_entities(&entities);
    let block = render_entity_agreement_block(&merged);
    if block.is_empty() {
        println!("Active entities present but produced no agreement block.");
    } else {
        print!("{block}");
    }
    let _ = entities_fingerprint(&merged); // exercise the fn for compilation symmetry
    Ok(())
}
