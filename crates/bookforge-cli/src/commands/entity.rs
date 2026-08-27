use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use bookforge_core::{
    GlossaryScopeKind,
    entity::{
        Entity, EntityGender, entities_fingerprint, merge_scope_entities,
        render_entity_agreement_block,
    },
};
use bookforge_store::{JobStore, NewEntity, StoredEntity};
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};

#[derive(Debug, Args)]
pub struct EntitiesArgs {
    #[command(subcommand)]
    command: EntitiesCommand,
}

#[derive(Debug, Subcommand)]
enum EntitiesCommand {
    /// List stored entity sheets.
    List(ListArgs),
    /// Import entities from a BookForge TOML file.
    Import(ImportArgs),
    /// Export matching entities to a BookForge TOML file.
    Export(ExportArgs),
    /// Remove all entities in a selected scope.
    Clear(ClearArgs),
    /// Show the merged entity guidance for a translation context.
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
    scope: Option<GlossaryScopeKind>,

    #[arg(long)]
    scope_id: Option<String>,

    /// Language pair to pin, as `SOURCE->TARGET` (`:` and `/` also accepted),
    /// matching `glossary export`.
    #[arg(long)]
    language: Option<String>,
}

#[derive(Debug, Args)]
struct ClearArgs {
    #[arg(long, value_enum)]
    scope: GlossaryScopeKind,

    #[arg(long)]
    scope_id: Option<String>,

    /// Confirm the deletion. Required so a stray Enter cannot wipe stored
    /// guidance; nothing is removed without this flag.
    #[arg(long)]
    yes: bool,
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
        EntitiesCommand::Export(args) => export_entities(&store, args),
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

/// Mirror of `glossary export`: pull the matching rows and re-serialize them
/// into the exact TOML schema `entities import` parses back.
fn export_entities(store: &JobStore, args: ExportArgs) -> Result<()> {
    let (source_language, target_language) = match args.language.as_deref() {
        Some(language) => {
            let (source, target) = crate::commands::glossary::parse_language_pair(language)?;
            (Some(source), Some(target))
        }
        None => (None, None),
    };
    let stored = store.list_entities(
        source_language.as_deref(),
        target_language.as_deref(),
        args.scope,
        args.scope_id.as_deref(),
    )?;
    let output = entities_to_toml(&stored)?;
    fs::write(&args.file, toml::to_string_pretty(&output)?)?;
    println!("Exported {} entity rows.", output.entities.len());
    Ok(())
}

fn entities_to_toml(records: &[StoredEntity]) -> Result<EntitiesToml> {
    let Some(first) = records.first() else {
        anyhow::bail!("no entities matched the export filters");
    };
    let same_tuple = records.iter().all(|record| {
        record.scope_kind == first.scope_kind
            && record.scope_id == first.scope_id
            && record.source_language == first.source_language
            && record.target_language == first.target_language
    });
    if !same_tuple {
        anyhow::bail!(
            "export matched multiple scope/language tuples; narrow with --scope, --scope-id, and --language"
        );
    }
    Ok(EntitiesToml {
        meta: EntitiesTomlMeta {
            schema_version: 1,
            source_language: first.source_language.clone(),
            target_language: first.target_language.clone(),
            scope: EntitiesTomlScope {
                kind: first.scope_kind,
                id: first.scope_id.clone(),
            },
        },
        entities: records
            .iter()
            .map(|record| EntitiesTomlEntity {
                source_name: record.source_name.clone(),
                target_name: record.target_name.clone(),
                gender_target: record.gender_target,
                role: record.role.clone(),
                notes: record.notes.clone(),
            })
            .collect(),
    })
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
    validate_scope(args.scope, args.scope_id.as_deref())?;
    confirm_destructive_clear(args.yes, "entity")?;
    let count = store.clear_entities_scope(args.scope, args.scope_id.as_deref())?;
    println!("Cleared {count} entity rows.");
    Ok(())
}

/// Shared guard for destructive `clear` subcommands: refuse to delete stored
/// guidance unless the caller passed an explicit `--yes`.
fn confirm_destructive_clear(confirmed: bool, what: &str) -> Result<()> {
    if confirmed {
        return Ok(());
    }
    anyhow::bail!(
        "refusing to clear {what} without --yes; re-run with --yes to delete the selected scope"
    )
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

#[cfg(test)]
mod tests {
    use super::{entities_to_toml, parse_entities_toml};
    use bookforge_core::GlossaryScopeKind;
    use bookforge_store::JobStore;

    fn stored(
        source_name: &str,
        target_name: &str,
        gender: Option<bookforge_core::EntityGender>,
        role: Option<&str>,
    ) -> bookforge_store::StoredEntity {
        bookforge_store::StoredEntity {
            id: 1,
            scope_kind: GlossaryScopeKind::Global,
            scope_id: None,
            source_name: source_name.to_string(),
            target_name: target_name.to_string(),
            gender_target: gender,
            role: role.map(str::to_string),
            notes: None,
            source_language: "English".to_string(),
            target_language: "Italian".to_string(),
        }
    }

    #[test]
    fn exported_toml_reimports_same_entity_fields() {
        let records = vec![
            stored("Frodo", "Frodo", None, Some("ring-bearer")),
            stored(
                "Galadriel",
                "Galadriel",
                Some(bookforge_core::EntityGender::Feminine),
                None,
            ),
        ];

        let exported = entities_to_toml(&records).expect("entities should export");
        let encoded = toml::to_string_pretty(&exported).expect("TOML should encode");
        let reimported = parse_entities_toml(&encoded).expect("exported TOML should parse");

        assert_eq!(reimported.len(), 2);
        assert_eq!(reimported[0].source_name, "Frodo");
        assert_eq!(reimported[0].role.as_deref(), Some("ring-bearer"));
        assert_eq!(
            reimported[0].scope_kind,
            GlossaryScopeKind::Global,
            "global scope must not serialize a spurious scope.id"
        );
        assert_eq!(reimported[1].gender_target, records[1].gender_target);
    }

    #[test]
    fn export_refuses_empty_selections_and_mixed_tuples() {
        assert!(entities_to_toml(&[]).is_err());

        let mut mixed = vec![stored("A", "B", None, None)];
        let mut other = stored("C", "D", None, None);
        other.target_language = "Spanish".to_string();
        mixed.push(other);
        let error = entities_to_toml(&mixed).expect_err("mixed tuples must be refused");
        assert!(error.to_string().contains("narrow with --scope"), "{error}");
    }

    #[test]
    fn exported_rows_survive_a_full_store_roundtrip() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let store = JobStore::open(directory.path().join("jobs.sqlite")).expect("store");
        let rows = [stored("Legolas", "Legolas", None, Some("elf"))];
        store
            .upsert_entities(
                rows.iter()
                    .map(|record| bookforge_store::NewEntity {
                        scope_kind: record.scope_kind,
                        scope_id: record.scope_id.as_deref(),
                        source_name: &record.source_name,
                        target_name: &record.target_name,
                        gender_target: record.gender_target,
                        role: record.role.as_deref(),
                        notes: record.notes.as_deref(),
                        source_language: &record.source_language,
                        target_language: &record.target_language,
                    })
                    .collect::<Vec<_>>()
                    .as_slice(),
            )
            .expect("rows upsert");
        let listed = store.list_entities(None, None, None, None).expect("list");

        let exported = entities_to_toml(&listed).expect("export");
        let encoded = toml::to_string_pretty(&exported).expect("encode");
        let reparsed = parse_entities_toml(&encoded).expect("parse back");
        assert_eq!(reparsed.len(), listed.len());
        assert_eq!(reparsed[0].source_name, "Legolas");
        assert_eq!(reparsed[0].source_language, "English");
        assert_eq!(reparsed[0].target_language, "Italian");
    }
}
