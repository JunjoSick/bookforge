use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use bookforge_core::segment::{BlockTranslation, Segment, build_segments};
use bookforge_epub::{
    rebuild_epub_with_options, validate_block_translations, validate_translated_epub,
};
use bookforge_store::{JobStore, SaveManualCorrection};
use clap::Args;
use serde::{Deserialize, Serialize};

use super::{
    review::resolve_review_input,
    translate::{rebuild_options_from_snapshot, regenerate_report_after_correction},
};

#[derive(Debug, Args)]
pub struct CorrectArgs {
    pub job_id: String,

    #[arg(long)]
    pub segment: String,

    #[arg(
        long,
        conflicts_with = "from_file",
        required_unless_present = "from_file"
    )]
    pub text: Option<String>,

    #[arg(long, value_name = "PATH", conflicts_with = "text")]
    pub from_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CorrectionBlock {
    pub block_id: String,
    pub text: String,
}

#[derive(Debug, Clone, Deserialize)]
struct CorrectionFile {
    blocks: Vec<CorrectionBlock>,
}

#[derive(Debug, Clone)]
pub(crate) enum CorrectionPayload {
    Text(String),
    Blocks(Vec<CorrectionBlock>),
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct CorrectionOutcome {
    pub job_id: String,
    pub segment_id: String,
    pub corrected_blocks: usize,
    pub output_path: String,
    pub job_status: String,
}

pub async fn run(args: CorrectArgs) -> Result<()> {
    let payload = if let Some(text) = args.text {
        CorrectionPayload::Text(text)
    } else {
        let path = args.from_file.expect("clap requires one correction source");
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read correction file {}", path.display()))?;
        match serde_json::from_str::<CorrectionFile>(&raw) {
            Ok(file) => CorrectionPayload::Blocks(file.blocks),
            Err(_) => CorrectionPayload::Text(raw),
        }
    };

    let store = JobStore::open_default()?;
    let outcome = correct_job_segment(&store, &args.job_id, &args.segment, payload)?;
    println!(
        "Corrected {} block(s) in segment {}",
        outcome.corrected_blocks, outcome.segment_id
    );
    println!("Output: {}", outcome.output_path);
    println!("Job status: {}", outcome.job_status);
    Ok(())
}

pub(crate) fn correct_job_segment(
    store: &JobStore,
    job_id: &str,
    segment_id: &str,
    payload: CorrectionPayload,
) -> Result<CorrectionOutcome> {
    let job = store
        .get_job(job_id)?
        .ok_or_else(|| anyhow::anyhow!("job '{job_id}' was not found"))?;
    let snapshot = store
        .load_job_config_snapshot(job_id)?
        .ok_or_else(|| anyhow::anyhow!("job '{job_id}' has no run configuration snapshot"))?;
    let input = resolve_review_input(&job, &snapshot)?;
    let book = bookforge_epub::read_epub(&input)?;
    let segments = build_segments(&book, &snapshot.settings.to_settings().segmentation)?;
    let segment = segments
        .iter()
        .find(|segment| segment.id.0 == segment_id)
        .ok_or_else(|| anyhow::anyhow!("segment '{segment_id}' was not found in job '{job_id}'"))?;
    let corrected = normalize_payload(segment, payload)?;
    let corrected_ids = corrected
        .iter()
        .map(|block| block.block_id.0.as_str())
        .collect::<std::collections::HashSet<_>>();

    // Merge the pending correction over the segment's other block translations
    // in-memory. This merged view — not a reload from the store — is what the
    // staged rebuild below renders, so the rebuild always sees the corrected
    // text even though nothing has been persisted to the database yet.
    let mut all_blocks = store
        .load_block_translations(job_id)?
        .into_iter()
        .map(|block| {
            let block_id = bookforge_core::ir::BlockId(block.block_id);
            (
                block_id.clone(),
                BlockTranslation {
                    block_id,
                    text: block.text,
                },
            )
        })
        .collect::<HashMap<_, _>>();
    for block in &corrected {
        all_blocks.insert(block.block_id.clone(), block.clone());
    }
    let all_blocks = all_blocks.into_values().collect::<Vec<_>>();
    let blocking_issues = validate_block_translations(&segments, &all_blocks)
        .into_iter()
        .filter(|issue| {
            issue.severity == bookforge_epub::ValidationSeverity::Error
                && issue
                    .block_id
                    .as_deref()
                    .is_some_and(|id| corrected_ids.contains(id))
        })
        .map(|issue| issue.message)
        .collect::<Vec<_>>();
    if !blocking_issues.is_empty() {
        anyhow::bail!(
            "manual correction violates EPUB marker constraints: {}",
            blocking_issues.join("; ")
        );
    }

    // Rebuild to a staged sibling of the final output *before* touching the
    // database or the real output file. Only once the staged EPUB passes
    // structural validation do we persist the correction and atomically swap
    // the staged file into place, so a failed rebuild/validation can never
    // leave the DB and the on-disk output disagreeing about a correction.
    let staged_path = staged_output_path(&job.output_path);
    let rebuild_options = rebuild_options_from_snapshot(&snapshot);
    if let Err(error) =
        rebuild_epub_with_options(&book, &all_blocks, &staged_path, &rebuild_options)
    {
        let _ = fs::remove_file(&staged_path);
        return Err(error.into());
    }
    let validation = validate_translated_epub(&staged_path, &segments, &all_blocks);
    if !validation.xml_valid
        || validation
            .issues
            .iter()
            .any(|issue| issue.severity == bookforge_epub::ValidationSeverity::Error)
    {
        let _ = fs::remove_file(&staged_path);
        anyhow::bail!(
            "corrected EPUB failed structural validation; the correction was not saved and the \
             existing output at {} is unchanged",
            job.output_path.display()
        );
    }

    let translated_text = corrected
        .iter()
        .map(|block| block.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    if let Err(error) = store.save_manual_correction(SaveManualCorrection {
        job_id,
        segment_id,
        translated_text: &translated_text,
        blocks: &corrected,
    }) {
        let _ = fs::remove_file(&staged_path);
        return Err(error.into());
    }

    // The correction is durably persisted now. If the atomic swap below fails,
    // we must NOT delete the staged file: it is the only place the corrected
    // EPUB content exists on disk, and the DB already claims the correction is
    // saved.
    if let Err(error) = fs::rename(&staged_path, &job.output_path) {
        anyhow::bail!(
            "manual correction for segment '{segment_id}' in job '{job_id}' was saved to the \
             database, but replacing the output EPUB failed: {error}. The rebuilt EPUB is staged \
             at {} and the previous output at {} was left untouched. Retry `bookforge correct` \
             for this segment (it will rebuild and swap again), run `bookforge resume {job_id}` \
             to regenerate the output from the saved corrections, or manually move the staged \
             file over the output to recover.",
            staged_path.display(),
            job.output_path.display()
        );
    }

    let refreshed_job = store.get_job(job_id)?;
    let status = refreshed_job
        .as_ref()
        .map(|job| job.status.clone())
        .unwrap_or_else(|| "unknown".to_string());

    // The correction (DB + swapped-in EPUB) is already durable at this point.
    // Refreshing the QA report is a best-effort convenience — if it fails we
    // log and move on rather than reporting the whole `correct` invocation as
    // failed, since the actual correction succeeded. Only bother if the job
    // has ever had a report written (auto-written at translate/resume
    // finalization); jobs that predate that or never finalized have nothing
    // to keep in sync.
    if let Some(current_job) = refreshed_job.as_ref()
        && (current_job.report_json_path.is_some() || current_job.report_markdown_path.is_some())
        && let Err(error) = regenerate_report_after_correction(store, current_job, &segments)
    {
        tracing::warn!(
            "failed to refresh QA report for job '{job_id}' after correction: {error:#}"
        );
    }

    Ok(CorrectionOutcome {
        job_id: job_id.to_string(),
        segment_id: segment_id.to_string(),
        corrected_blocks: corrected.len(),
        output_path: job.output_path.display().to_string(),
        job_status: status,
    })
}

/// Builds a same-directory sibling path for staging a rebuilt EPUB before it
/// is atomically swapped into place over `output`. Living next to `output`
/// keeps the eventual `fs::rename` on the same volume, which is required for
/// the rename to be atomic (on Windows this maps to `MoveFileExW` with
/// `MOVEFILE_REPLACE_EXISTING`).
fn staged_output_path(output: &Path) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let stem = output
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("output");
    let extension = output
        .extension()
        .and_then(|value| value.to_str())
        .map(|ext| format!(".{ext}"))
        .unwrap_or_default();
    output.with_file_name(format!(
        "{stem}.staged-{}-{nonce}{extension}",
        std::process::id()
    ))
}

fn normalize_payload(
    segment: &Segment,
    payload: CorrectionPayload,
) -> Result<Vec<BlockTranslation>> {
    match payload {
        CorrectionPayload::Text(text) => {
            if segment.block_ids.len() != 1 {
                anyhow::bail!(
                    "segment '{}' contains {} blocks; use --from-file with JSON {{\"blocks\":[{{\"block_id\":\"...\",\"text\":\"...\"}}]}}",
                    segment.id.0,
                    segment.block_ids.len()
                );
            }
            Ok(vec![BlockTranslation {
                block_id: segment.block_ids[0].clone(),
                text,
            }])
        }
        CorrectionPayload::Blocks(blocks) => {
            let mut by_id = HashMap::new();
            for block in blocks {
                if by_id.insert(block.block_id.clone(), block.text).is_some() {
                    anyhow::bail!(
                        "correction contains duplicate block id '{}'",
                        block.block_id
                    );
                }
            }
            let mut corrected = Vec::with_capacity(segment.block_ids.len());
            for id in &segment.block_ids {
                let text = by_id
                    .remove(&id.0)
                    .ok_or_else(|| anyhow::anyhow!("correction is missing block '{}'", id.0))?;
                corrected.push(BlockTranslation {
                    block_id: id.clone(),
                    text,
                });
            }
            if !by_id.is_empty() {
                anyhow::bail!(
                    "correction contains block ids not present in segment '{}': {}",
                    segment.id.0,
                    by_id.keys().cloned().collect::<Vec<_>>().join(", ")
                );
            }
            Ok(corrected)
        }
    }
}
