use std::{collections::BTreeSet, fs, path::PathBuf};

use anyhow::Result;
use bookforge_core::{GlossaryCategory, GlossaryScopeKind, GlossaryStatus, GlossaryTerm};
use bookforge_store::{JobRecord, JobStore, NewSegmentFlag};
use clap::Args;
use serde::Deserialize;

#[derive(Debug, Args)]
pub struct IngestFlagsArgs {
    pub job_id: String,

    #[arg(long)]
    pub flags: PathBuf,

    #[arg(long, value_enum)]
    pub default_scope: Option<GlossaryScopeKind>,
}

#[derive(Debug, Deserialize)]
struct FlagsFile {
    schema_version: u32,
    job_id: String,
    #[allow(dead_code)]
    exported_at: Option<String>,
    flags: Vec<FlagEntry>,
}

#[derive(Debug, Deserialize)]
struct FlagEntry {
    segment_id: String,
    kind: String,
    note: Option<String>,
    suggested_source: Option<String>,
    suggested_target: Option<String>,
}

pub async fn run(args: IngestFlagsArgs) -> Result<()> {
    let store = JobStore::open_default()?;
    let Some(job) = store.get_job(&args.job_id)? else {
        anyhow::bail!("job '{}' was not found", args.job_id);
    };

    let parsed: FlagsFile = serde_json::from_str(&fs::read_to_string(&args.flags)?)
        .map_err(|err| anyhow::anyhow!("invalid flags JSON: {err}"))?;
    validate_flags(&args.job_id, &parsed)?;

    let known_segments = store
        .segment_records(&args.job_id)?
        .into_iter()
        .map(|record| record.id)
        .collect::<BTreeSet<_>>();
    for flag in &parsed.flags {
        if !known_segments.contains(&flag.segment_id) {
            anyhow::bail!(
                "flags file references unknown segment '{}' for job '{}'",
                flag.segment_id,
                args.job_id
            );
        }
    }

    let wrong_translation_ids = parsed
        .flags
        .iter()
        .filter(|flag| flag.kind == "wrong_translation")
        .map(|flag| flag.segment_id.clone())
        .collect::<BTreeSet<_>>();
    let glossary_terms = glossary_terms_from_flags(&job, args.default_scope, &parsed.flags)?;

    let new_flags = parsed
        .flags
        .iter()
        .map(|flag| NewSegmentFlag {
            job_id: &args.job_id,
            segment_id: &flag.segment_id,
            kind: &flag.kind,
            note: flag.note.as_deref(),
            suggested_source: flag.suggested_source.as_deref(),
            suggested_target: flag.suggested_target.as_deref(),
            consumed: flag.kind == "wrong_translation"
                || (flag.kind == "name" && flag.suggested_target.is_some())
                || flag.kind == "register",
        })
        .collect::<Vec<_>>();

    let inserted = store.insert_segment_flags(&new_flags)?;
    let glossary_added = store.upsert_glossary_terms(&glossary_terms)?;
    let wrong_translation_ids = wrong_translation_ids.into_iter().collect::<Vec<_>>();
    let marked = store.mark_segments_needs_review(
        &args.job_id,
        &wrong_translation_ids,
        "flagged wrong_translation via ingest-flags",
    )?;

    println!(
        "Ingested {inserted} flags. {marked} segments marked needs-review. {glossary_added} glossary terms saved."
    );

    Ok(())
}

fn glossary_terms_from_flags(
    job: &JobRecord,
    default_scope: Option<GlossaryScopeKind>,
    flags: &[FlagEntry],
) -> Result<Vec<GlossaryTerm>> {
    let scope_kind = default_scope.unwrap_or(GlossaryScopeKind::Book);
    let scope_id = match scope_kind {
        GlossaryScopeKind::Global => None,
        GlossaryScopeKind::Book => Some(job.book_id.clone().unwrap_or_else(|| job.id.clone())),
        GlossaryScopeKind::Series => Some(job.series_id.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "--default-scope series requires job '{}' to have a series_id",
                job.id
            )
        })?),
    };
    let source_language = job
        .source_lang
        .clone()
        .unwrap_or_else(|| "auto".to_string());
    let mut terms = Vec::new();
    for flag in flags {
        match flag.kind.as_str() {
            "name"
                if flag
                    .suggested_target
                    .as_deref()
                    .is_some_and(|value| !value.trim().is_empty()) =>
            {
                let target = flag.suggested_target.clone().unwrap_or_default();
                let source = flag
                    .suggested_source
                    .clone()
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or_else(|| target.clone());
                terms.push(GlossaryTerm {
                    id: None,
                    scope_kind,
                    scope_id: scope_id.clone(),
                    source_text: source,
                    target_text: target,
                    category: GlossaryCategory::Person,
                    notes: flag.note.clone(),
                    case_sensitive: true,
                    always_active: false,
                    status: GlossaryStatus::UserSeeded,
                    source_language: source_language.clone(),
                    target_language: job.target_lang.clone(),
                    source_count: 0,
                });
            }
            "register" => {
                let source = flag
                    .suggested_source
                    .clone()
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or_else(|| format!("__register:{}", flag.segment_id));
                let target = flag
                    .suggested_target
                    .clone()
                    .or_else(|| flag.note.clone())
                    .unwrap_or_else(|| "register".to_string());
                terms.push(GlossaryTerm {
                    id: None,
                    scope_kind,
                    scope_id: scope_id.clone(),
                    source_text: source,
                    target_text: target,
                    category: GlossaryCategory::Style,
                    notes: flag.note.clone(),
                    case_sensitive: false,
                    always_active: true,
                    status: GlossaryStatus::UserSeeded,
                    source_language: source_language.clone(),
                    target_language: job.target_lang.clone(),
                    source_count: 0,
                });
            }
            _ => {}
        }
    }
    Ok(terms)
}

fn validate_flags(job_id: &str, parsed: &FlagsFile) -> Result<()> {
    if parsed.schema_version != 1 {
        anyhow::bail!(
            "unsupported flags schema_version {}; expected 1",
            parsed.schema_version
        );
    }
    if parsed.job_id != job_id {
        anyhow::bail!(
            "flags job_id '{}' does not match requested job '{}'",
            parsed.job_id,
            job_id
        );
    }
    for (index, flag) in parsed.flags.iter().enumerate() {
        if flag.segment_id.trim().is_empty() {
            anyhow::bail!("flags[{index}].segment_id is required");
        }
        if !valid_kind(&flag.kind) {
            anyhow::bail!(
                "flags[{index}].kind '{}' is invalid; expected one of name, register, wrong_translation, formatting, tone, other",
                flag.kind
            );
        }
    }
    Ok(())
}

fn valid_kind(kind: &str) -> bool {
    matches!(
        kind,
        "name" | "register" | "wrong_translation" | "formatting" | "tone" | "other"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_flags_rejects_wrong_job() {
        let parsed = FlagsFile {
            schema_version: 1,
            job_id: "job_other".to_string(),
            exported_at: None,
            flags: Vec::new(),
        };
        let error = validate_flags("job_expected", &parsed).expect_err("should reject job");
        assert!(error.to_string().contains("does not match"));
    }

    #[test]
    fn validate_flags_rejects_unknown_kind() {
        let parsed = FlagsFile {
            schema_version: 1,
            job_id: "job_1".to_string(),
            exported_at: None,
            flags: vec![FlagEntry {
                segment_id: "seg_1".to_string(),
                kind: "bad".to_string(),
                note: None,
                suggested_source: None,
                suggested_target: None,
            }],
        };
        let error = validate_flags("job_1", &parsed).expect_err("should reject kind");
        assert!(error.to_string().contains("invalid"));
    }

    #[test]
    fn name_flags_seed_book_scoped_glossary_terms() {
        let job = JobRecord {
            id: "job_1".to_string(),
            input_path: "input.epub".into(),
            input_snapshot_path: None,
            input_sha256: None,
            output_path: "out.epub".into(),
            input_hash: "hash".to_string(),
            source_lang: Some("English".to_string()),
            target_lang: "Italian".to_string(),
            provider: "mock".to_string(),
            model: "mock".to_string(),
            base_url: None,
            api_key_env: None,
            status: "succeeded".to_string(),
            events_path: None,
            report_json_path: None,
            report_markdown_path: None,
            book_id: Some("fellowship".to_string()),
            series_id: Some("lotr".to_string()),
        };
        let terms = glossary_terms_from_flags(
            &job,
            None,
            &[FlagEntry {
                segment_id: "seg_1".to_string(),
                kind: "name".to_string(),
                note: Some("preserve".to_string()),
                suggested_source: Some("Aragorn".to_string()),
                suggested_target: Some("Aragorn".to_string()),
            }],
        )
        .expect("terms should build");

        assert_eq!(terms.len(), 1);
        assert_eq!(terms[0].scope_kind, GlossaryScopeKind::Book);
        assert_eq!(terms[0].scope_id.as_deref(), Some("fellowship"));
        assert_eq!(terms[0].category, GlossaryCategory::Person);
    }
}
