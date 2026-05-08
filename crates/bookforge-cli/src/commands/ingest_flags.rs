use std::{collections::BTreeSet, fs, path::PathBuf};

use anyhow::Result;
use bookforge_store::{JobStore, NewSegmentFlag};
use clap::Args;
use serde::Deserialize;

#[derive(Debug, Args)]
pub struct IngestFlagsArgs {
    pub job_id: String,

    #[arg(long)]
    pub flags: PathBuf,
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
    if store.get_job(&args.job_id)?.is_none() {
        anyhow::bail!("job '{}' was not found", args.job_id);
    }

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
            consumed: flag.kind == "wrong_translation",
        })
        .collect::<Vec<_>>();

    let inserted = store.insert_segment_flags(&new_flags)?;
    let wrong_translation_ids = wrong_translation_ids.into_iter().collect::<Vec<_>>();
    let marked = store.mark_segments_needs_review(
        &args.job_id,
        &wrong_translation_ids,
        "flagged wrong_translation via ingest-flags",
    )?;

    println!(
        "Ingested {inserted} flags. {marked} segments marked needs-review. Glossary integration will be available in v1.2."
    );

    Ok(())
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
}
