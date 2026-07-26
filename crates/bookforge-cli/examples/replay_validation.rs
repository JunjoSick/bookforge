//! Replay BookForge's batch translation validator over the on-disk job store.
//!
//! `batch_item_validation_error` is a pure function over (source block, model
//! output). Every ingredient it needs is already on disk: the `input.epub`
//! snapshot of every job plus the per-block translations the run stored. This
//! example re-derives the blocks from the snapshots, re-runs the validator over
//! the stored translations, and diffs the result against the flags the original
//! run recorded in `segments.error`. Zero API calls, zero cost.
//!
//! Run it with:
//!
//! ```text
//! cargo run --release --example replay_validation -- --db .bookforge/jobs.sqlite
//! ```
//!
//! The owner's database is never mutated: `JobStore::open` migrates on open, so
//! the store file (and its WAL sidecars) is copied into a temporary directory
//! first and the copy is what gets opened.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs,
    io::{BufWriter, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use bookforge_core::{
    config::{BatchConfig, SegmentationConfig, TranslationProfile},
    run_snapshot::RunConfigSnapshot,
    segment::{Segment, build_segments},
};
use bookforge_epub::read_epub;
use bookforge_llm::{
    TranslationBatchItem, batch::batch_item_validation_error, build_translation_batches,
};
use bookforge_store::{JobRecord, JobStore};
use clap::Parser;
use serde::Serialize;

/// Kinds the replay and the stored errors are bucketed into. Kept small and
/// stable: `--emit-pairs` consumers key off these strings.
const KIND_PROTECTED_SPAN_MISSING: &str = "protected_span_missing";
const KIND_INLINE_MARKER_MISSING: &str = "inline_marker_missing";
const KIND_INLINE_MARKER_DUPLICATED: &str = "inline_marker_duplicated";
const KIND_UNKNOWN_INLINE_MARKER: &str = "unknown_inline_marker";
const KIND_BATCH_BLOCK_MISMATCH: &str = "batch_translation_block_mismatch";
const KIND_TRANSLATION_UNCHANGED: &str = "translation_unchanged";
const KIND_TARGET_LANGUAGE_GATE: &str = "target_language_gate";
const KIND_OTHER: &str = "other";

const ALL_KINDS: &[&str] = &[
    KIND_PROTECTED_SPAN_MISSING,
    KIND_INLINE_MARKER_MISSING,
    KIND_INLINE_MARKER_DUPLICATED,
    KIND_UNKNOWN_INLINE_MARKER,
    KIND_BATCH_BLOCK_MISMATCH,
    KIND_TRANSLATION_UNCHANGED,
    KIND_TARGET_LANGUAGE_GATE,
    KIND_OTHER,
];

/// Batching only groups items; it never changes the per-item fields the
/// validator reads. Real runs often ran with batching disabled, which would
/// make `build_translation_batches` return nothing, so the replay forces it on
/// with a generous budget.
const REPLAY_TARGET_TOKENS: usize = 4_000;
const REPLAY_MAX_ITEMS: usize = 64;

#[derive(Debug, Parser)]
#[command(
    name = "replay_validation",
    about = "Replay the translation validator over stored jobs at zero API cost",
    long_about = "Re-derives every block from each job's input.epub snapshot, pairs it with the \
                  translation the run stored, re-runs batch_item_validation_error, and reports \
                  the delta against the flags recorded in segments.error."
)]
struct Args {
    /// Path to the job store. Opened read-only via a throwaway copy.
    #[arg(long, default_value = ".bookforge/jobs.sqlite")]
    db: PathBuf,

    /// Only replay these job ids (repeatable).
    #[arg(long = "job")]
    jobs: Vec<String>,

    /// Only replay jobs with these target languages (repeatable, case-insensitive).
    #[arg(long = "target-lang")]
    target_langs: Vec<String>,

    /// Write every flagged pair to this file as JSONL.
    #[arg(long)]
    emit_pairs: Option<PathBuf>,

    /// How many example rows to print per delta bucket.
    #[arg(long, default_value_t = 10)]
    max_examples: usize,

    /// Suppress the per-job progress lines.
    #[arg(long)]
    quiet: bool,
}

#[derive(Debug, Serialize)]
struct ValidatorFlag {
    kind: &'static str,
    message: String,
}

#[derive(Debug, Serialize)]
struct EmittedPair {
    job_id: String,
    segment_id: String,
    block_id: String,
    source_language: String,
    target_language: String,
    source_text: String,
    translated_text: String,
    validator_flags: Vec<ValidatorFlag>,
}

#[derive(Debug, Default)]
struct Totals {
    jobs_considered: usize,
    jobs_replayed: usize,
    settings_resolved: usize,
    skipped_settings_unreadable: usize,
    skipped_no_snapshot_path: usize,
    skipped_missing_snapshot: usize,
    skipped_epub_unreadable: usize,
    skipped_segmentation_failed: usize,

    stored_block_rows: usize,
    pairs_replayed: usize,
    unmatched_block_rows: usize,
    empty_translations: usize,
    items_without_translation: usize,
    preserved_source_pairs: usize,

    flagged_pairs: usize,
    flagged_preserved_source_pairs: usize,
}

#[derive(Debug, Default)]
struct DeltaTotals {
    stored_flagged_segments: usize,
    replay_flagged_segments: usize,
    new_flags: usize,
    lost_replayable: usize,
    lost_no_pairs: usize,
    both_kinds_agree: usize,
    both_kinds_differ: usize,
}

struct DeltaExample {
    job_id: String,
    segment_id: String,
    stored_kinds: Vec<&'static str>,
    replay_kinds: Vec<&'static str>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let db_path = args.db.clone();
    if !db_path.exists() {
        anyhow::bail!("job store not found: {}", db_path.display());
    }

    let temp = tempfile::tempdir().context("creating a scratch dir for the store copy")?;
    let copy_path = copy_store(&db_path, temp.path())?;
    println!(
        "store       : {} (replayed against a throwaway copy; the original is never written)",
        db_path.display()
    );

    let store = JobStore::open(&copy_path).context("opening the copied job store")?;
    let root = store_root(&db_path);

    let job_filter: HashSet<&str> = args.jobs.iter().map(String::as_str).collect();
    let lang_filter: Vec<String> = args
        .target_langs
        .iter()
        .map(|lang| lang.to_ascii_lowercase())
        .collect();

    let mut totals = Totals::default();
    let mut flagged_by_kind: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut flagged_by_kind_model_output: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut other_messages: BTreeMap<String, usize> = BTreeMap::new();

    let mut delta = DeltaTotals::default();
    let mut stored_segment_kinds: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut replay_segment_kinds: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut new_flag_examples: Vec<DeltaExample> = Vec::new();
    let mut lost_flag_examples: Vec<DeltaExample> = Vec::new();
    let mut differ_examples: Vec<DeltaExample> = Vec::new();

    let mut emitter = match args.emit_pairs.as_ref() {
        Some(path) => Some(BufWriter::new(fs::File::create(path).with_context(
            || format!("creating {} for --emit-pairs", path.display()),
        )?)),
        None => None,
    };

    let summaries = store
        .list_job_summaries()
        .context("listing jobs from the store")?;

    for (job, _summary) in &summaries {
        if !job_filter.is_empty() && !job_filter.contains(job.id.as_str()) {
            continue;
        }
        if !lang_filter.is_empty()
            && !lang_filter
                .iter()
                .any(|lang| job.target_lang.eq_ignore_ascii_case(lang))
        {
            continue;
        }
        totals.jobs_considered += 1;

        let snapshot = match store.load_job_config_snapshot(&job.id) {
            Ok(Some(snapshot)) => {
                totals.settings_resolved += 1;
                snapshot
            }
            Ok(None) => {
                totals.skipped_settings_unreadable += 1;
                warn(&format!(
                    "{}: no run configuration snapshot; skipping rather than replaying with defaults",
                    job.id
                ));
                continue;
            }
            Err(error) => {
                totals.skipped_settings_unreadable += 1;
                warn(&format!("{}: config snapshot unreadable: {error}", job.id));
                continue;
            }
        };

        let Some(epub_path) = resolve_snapshot_path(&root, job, &snapshot) else {
            totals.skipped_no_snapshot_path += 1;
            warn(&format!("{}: no input snapshot recorded", job.id));
            continue;
        };
        if !epub_path.exists() {
            totals.skipped_missing_snapshot += 1;
            warn(&format!(
                "{}: input snapshot missing at {}",
                job.id,
                epub_path.display()
            ));
            continue;
        }

        let book = match read_epub(&epub_path) {
            Ok(book) => book,
            Err(error) => {
                totals.skipped_epub_unreadable += 1;
                warn(&format!(
                    "{}: input snapshot does not parse: {error}",
                    job.id
                ));
                continue;
            }
        };

        let (segmentation, batch_config, profile) = replay_settings(&snapshot);
        let segments = match build_segments(&book, &segmentation) {
            Ok(segments) => segments,
            Err(error) => {
                totals.skipped_segmentation_failed += 1;
                warn(&format!("{}: segmentation failed: {error}", job.id));
                continue;
            }
        };

        let items = derive_items(&segments, &batch_config, profile);
        let section_titles = section_titles(&segments);

        let source_lang = snapshot
            .source_language
            .clone()
            .filter(|lang| !lang.trim().is_empty());
        let target_lang = snapshot.target_language.clone();
        let provider = snapshot.provider.clone();

        // Mirrors bookforge_llm::validation::should_validate_source_copy, which
        // is pub(crate) and therefore not reachable from an example.
        let is_mock = provider.eq_ignore_ascii_case("mock");
        let validate_source_copy = !is_mock
            && source_lang
                .as_deref()
                .map(|source| !source.eq_ignore_ascii_case(&target_lang))
                .unwrap_or(true);
        // Mirrors the call site in bookforge_llm::batch::execution.
        let target_language_arg = (!is_mock).then_some(target_lang.as_str());

        let stored_blocks = match store.load_block_translations(&job.id) {
            Ok(blocks) => blocks,
            Err(error) => {
                warn(&format!(
                    "{}: block translations unreadable: {error}",
                    job.id
                ));
                continue;
            }
        };
        let segment_records = match store.segment_records(&job.id) {
            Ok(records) => records,
            Err(error) => {
                warn(&format!("{}: segment records unreadable: {error}", job.id));
                continue;
            }
        };

        totals.jobs_replayed += 1;
        totals.stored_block_rows += stored_blocks.len();

        let mut job_pairs = 0usize;
        let mut job_flagged = 0usize;
        let mut segments_with_pairs: HashSet<String> = HashSet::new();
        let mut replay_flags_by_segment: HashMap<String, BTreeSet<&'static str>> = HashMap::new();
        let mut matched_items: HashSet<(String, String)> = HashSet::new();

        for stored in &stored_blocks {
            let key = (stored.segment_id.clone(), stored.block_id.clone());
            let Some(item) = items.get(&key) else {
                totals.unmatched_block_rows += 1;
                continue;
            };
            matched_items.insert(key);
            if stored.text.trim().is_empty() {
                totals.empty_translations += 1;
                continue;
            }

            let preserved_source = item.source_text == stored.text;
            if preserved_source {
                totals.preserved_source_pairs += 1;
            }
            totals.pairs_replayed += 1;
            job_pairs += 1;
            segments_with_pairs.insert(stored.segment_id.clone());

            let section_title = section_titles.get(&stored.segment_id).map(String::as_str);
            let Some(message) = batch_item_validation_error(
                item,
                &stored.text,
                validate_source_copy,
                section_title,
                target_language_arg,
            ) else {
                continue;
            };

            let kinds = classify_kinds(&message);
            totals.flagged_pairs += 1;
            job_flagged += 1;
            for kind in &kinds {
                *flagged_by_kind.entry(*kind).or_default() += 1;
                if !preserved_source {
                    *flagged_by_kind_model_output.entry(*kind).or_default() += 1;
                }
                if *kind == KIND_OTHER {
                    *other_messages.entry(message_prefix(&message)).or_default() += 1;
                }
            }
            if preserved_source {
                totals.flagged_preserved_source_pairs += 1;
            }
            replay_flags_by_segment
                .entry(stored.segment_id.clone())
                .or_default()
                .extend(kinds.iter().copied());

            if let Some(writer) = emitter.as_mut() {
                let pair = EmittedPair {
                    job_id: job.id.clone(),
                    segment_id: stored.segment_id.clone(),
                    block_id: stored.block_id.clone(),
                    source_language: source_lang.clone().unwrap_or_default(),
                    target_language: target_lang.clone(),
                    source_text: item.source_text.clone(),
                    translated_text: stored.text.clone(),
                    validator_flags: kinds
                        .iter()
                        .map(|kind| ValidatorFlag {
                            kind,
                            message: message.clone(),
                        })
                        .collect(),
                };
                let line = serde_json::to_string(&pair)
                    .context("serializing a flagged pair for --emit-pairs")?;
                writer.write_all(line.as_bytes())?;
                writer.write_all(b"\n")?;
            }
        }

        totals.items_without_translation += items.len() - matched_items.len();

        for record in &segment_records {
            let stored_kinds: BTreeSet<&'static str> = record
                .error
                .as_deref()
                .map(str::trim)
                .filter(|error| !error.is_empty())
                .map(|error| classify_kinds(error).into_iter().collect())
                .unwrap_or_default();
            let replay_kinds = replay_flags_by_segment
                .get(&record.id)
                .cloned()
                .unwrap_or_default();

            if !stored_kinds.is_empty() {
                delta.stored_flagged_segments += 1;
                for kind in &stored_kinds {
                    *stored_segment_kinds.entry(*kind).or_default() += 1;
                }
            }
            if !replay_kinds.is_empty() {
                delta.replay_flagged_segments += 1;
                for kind in &replay_kinds {
                    *replay_segment_kinds.entry(*kind).or_default() += 1;
                }
            }

            match (stored_kinds.is_empty(), replay_kinds.is_empty()) {
                (true, true) => {}
                (true, false) => {
                    delta.new_flags += 1;
                    push_example(
                        &mut new_flag_examples,
                        args.max_examples,
                        &job.id,
                        &record.id,
                        &stored_kinds,
                        &replay_kinds,
                    );
                }
                (false, true) => {
                    if segments_with_pairs.contains(&record.id) {
                        delta.lost_replayable += 1;
                        push_example(
                            &mut lost_flag_examples,
                            args.max_examples,
                            &job.id,
                            &record.id,
                            &stored_kinds,
                            &replay_kinds,
                        );
                    } else {
                        delta.lost_no_pairs += 1;
                    }
                }
                (false, false) => {
                    if stored_kinds == replay_kinds {
                        delta.both_kinds_agree += 1;
                    } else {
                        delta.both_kinds_differ += 1;
                        push_example(
                            &mut differ_examples,
                            args.max_examples,
                            &job.id,
                            &record.id,
                            &stored_kinds,
                            &replay_kinds,
                        );
                    }
                }
            }
        }

        if !args.quiet {
            println!(
                "  {} [{}] pairs={job_pairs} flagged={job_flagged}",
                job.id, target_lang
            );
        }
    }

    if let Some(mut writer) = emitter {
        writer.flush().context("flushing --emit-pairs output")?;
    }

    print_report(
        &args,
        &totals,
        &flagged_by_kind,
        &flagged_by_kind_model_output,
        &other_messages,
        &delta,
        &stored_segment_kinds,
        &replay_segment_kinds,
        &new_flag_examples,
        &lost_flag_examples,
        &differ_examples,
    );

    Ok(())
}

fn warn(message: &str) {
    eprintln!("warning: {message}");
}

fn copy_store(db_path: &Path, dir: &Path) -> Result<PathBuf> {
    let name = db_path
        .file_name()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("jobs.sqlite"));
    let target = dir.join(&name);
    fs::copy(db_path, &target)
        .with_context(|| format!("copying {} into a scratch dir", db_path.display()))?;
    for suffix in ["-wal", "-shm"] {
        let sidecar = sidecar_path(db_path, suffix);
        if sidecar.exists() {
            let sidecar_target = sidecar_path(&target, suffix);
            fs::copy(&sidecar, &sidecar_target)
                .with_context(|| format!("copying {}", sidecar.display()))?;
        }
    }
    Ok(target)
}

fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// `<root>/.bookforge/jobs.sqlite` -> `<root>`; run snapshot paths are stored
/// relative to that root.
fn store_root(db_path: &Path) -> PathBuf {
    db_path
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn resolve_snapshot_path(
    root: &Path,
    job: &JobRecord,
    snapshot: &RunConfigSnapshot,
) -> Option<PathBuf> {
    let recorded = job
        .input_snapshot_path
        .clone()
        .or_else(|| snapshot.input_snapshot_path.clone());
    if let Some(path) = recorded {
        let resolved = if path.is_absolute() {
            path
        } else {
            root.join(path)
        };
        if resolved.exists() {
            return Some(resolved);
        }
    }
    let fallback = root
        .join(".bookforge")
        .join("runs")
        .join(&job.id)
        .join("input.epub");
    fallback.exists().then_some(fallback)
}

fn replay_settings(
    snapshot: &RunConfigSnapshot,
) -> (SegmentationConfig, BatchConfig, TranslationProfile) {
    let settings = snapshot.settings.to_settings();
    let batch = BatchConfig {
        enabled: true,
        target_tokens: settings.batch.target_tokens.max(REPLAY_TARGET_TOKENS),
        max_items: settings.batch.max_items.max(REPLAY_MAX_ITEMS),
        ..settings.batch.clone()
    };
    (settings.segmentation, batch, settings.profile)
}

fn derive_items(
    segments: &[Segment],
    batch_config: &BatchConfig,
    profile: TranslationProfile,
) -> HashMap<(String, String), TranslationBatchItem> {
    build_translation_batches(segments, batch_config, profile)
        .into_iter()
        .flat_map(|batch| batch.items)
        .map(|item| ((item.segment_id.0.clone(), item.block_id.0.clone()), item))
        .collect()
}

fn section_titles(segments: &[Segment]) -> HashMap<String, String> {
    segments
        .iter()
        .filter_map(|segment| {
            segment
                .metadata
                .section_title
                .as_ref()
                .map(|title| (segment.id.0.clone(), title.clone()))
        })
        .collect()
}

/// Bucket a validator message. Stored `segments.error` strings are `; `-joined
/// and can mention several failures, so this returns every kind it finds.
fn classify_kinds(message: &str) -> Vec<&'static str> {
    let mut kinds = Vec::new();
    if message.contains("protected span missing") {
        kinds.push(KIND_PROTECTED_SPAN_MISSING);
    }
    if message.contains("inline marker missing") {
        kinds.push(KIND_INLINE_MARKER_MISSING);
    }
    if message.contains("inline marker duplicated") {
        kinds.push(KIND_INLINE_MARKER_DUPLICATED);
    }
    if message.contains("unknown inline marker") {
        kinds.push(KIND_UNKNOWN_INLINE_MARKER);
    }
    if message.contains("batch translation block mismatch")
        || message.contains("batch translation missing block translation")
        || message.contains("item missing from batch response")
    {
        kinds.push(KIND_BATCH_BLOCK_MISMATCH);
    }
    if message.contains("translation is unchanged") {
        kinds.push(KIND_TRANSLATION_UNCHANGED);
    }
    if message.contains("Toki Pona")
        || message.contains("unapproved lowercase word")
        || message.contains("pathological repeated")
        || message.contains("must not be followed by li")
        || message.contains("pi must group")
        || message.contains("en may only coordinate")
    {
        kinds.push(KIND_TARGET_LANGUAGE_GATE);
    }
    if kinds.is_empty() {
        kinds.push(KIND_OTHER);
    }
    kinds
}

fn message_prefix(message: &str) -> String {
    let trimmed: String = message.chars().take(60).collect();
    trimmed.replace(['\n', '\r'], " ")
}

fn push_example(
    sink: &mut Vec<DeltaExample>,
    limit: usize,
    job_id: &str,
    segment_id: &str,
    stored: &BTreeSet<&'static str>,
    replay: &BTreeSet<&'static str>,
) {
    if sink.len() >= limit {
        return;
    }
    sink.push(DeltaExample {
        job_id: job_id.to_string(),
        segment_id: segment_id.to_string(),
        stored_kinds: stored.iter().copied().collect(),
        replay_kinds: replay.iter().copied().collect(),
    });
}

#[allow(clippy::too_many_arguments)]
fn print_report(
    args: &Args,
    totals: &Totals,
    flagged_by_kind: &BTreeMap<&'static str, usize>,
    flagged_model_output: &BTreeMap<&'static str, usize>,
    other_messages: &BTreeMap<String, usize>,
    delta: &DeltaTotals,
    stored_segment_kinds: &BTreeMap<&'static str, usize>,
    replay_segment_kinds: &BTreeMap<&'static str, usize>,
    new_flag_examples: &[DeltaExample],
    lost_flag_examples: &[DeltaExample],
    differ_examples: &[DeltaExample],
) {
    println!("\n=== jobs ===");
    println!("considered                 : {}", totals.jobs_considered);
    println!("replayed                   : {}", totals.jobs_replayed);
    println!("settings resolved          : {}", totals.settings_resolved);
    println!(
        "skipped: settings unreadable: {}",
        totals.skipped_settings_unreadable
    );
    println!(
        "skipped: no snapshot path  : {}",
        totals.skipped_no_snapshot_path
    );
    println!(
        "skipped: snapshot missing  : {}",
        totals.skipped_missing_snapshot
    );
    println!(
        "skipped: epub unreadable   : {}",
        totals.skipped_epub_unreadable
    );
    println!(
        "skipped: segmentation fail : {}",
        totals.skipped_segmentation_failed
    );
    println!("\n=== pairs ===");
    println!("stored block rows          : {}", totals.stored_block_rows);
    println!("pairs replayed             : {}", totals.pairs_replayed);
    println!(
        "  of which preserved source: {}",
        totals.preserved_source_pairs
    );
    println!(
        "block rows with no item    : {}",
        totals.unmatched_block_rows
    );
    println!("empty translations         : {}", totals.empty_translations);
    println!(
        "items with no translation  : {}",
        totals.items_without_translation
    );

    println!("\n=== flagged pairs (current code) ===");
    println!("total flagged              : {}", totals.flagged_pairs);
    print_kind_table(flagged_by_kind);

    let model_output_flagged = totals
        .flagged_pairs
        .saturating_sub(totals.flagged_preserved_source_pairs);
    println!(
        "\n=== flagged pairs excluding preserved-source pairs ({model_output_flagged}) ===\n\
         (a needs-review segment stores the preserved SOURCE text, so those pairs say nothing\n\
          about the model's output; this is the number that measures the validator)"
    );
    print_kind_table(flagged_model_output);

    if !other_messages.is_empty() {
        println!("\n=== `other` messages (top 15 by count) ===");
        let mut rows: Vec<(&String, &usize)> = other_messages.iter().collect();
        rows.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        for (prefix, count) in rows.into_iter().take(15) {
            println!("  {count:>6}  {prefix}");
        }
    }

    println!("\n=== delta vs the stored run (per segment) ===");
    println!(
        "stored flagged segments    : {}",
        delta.stored_flagged_segments
    );
    print_kind_table(stored_segment_kinds);
    println!(
        "replay flagged segments    : {}",
        delta.replay_flagged_segments
    );
    print_kind_table(replay_segment_kinds);
    println!("new flags (replay only)    : {}", delta.new_flags);
    println!(
        "lost flags (stored only)    : {} replayable + {} with nothing to replay",
        delta.lost_replayable, delta.lost_no_pairs
    );
    println!(
        "flagged on both sides       : {} agree / {} differ",
        delta.both_kinds_agree, delta.both_kinds_differ
    );

    print_examples("new flags", new_flag_examples, args.max_examples);
    print_examples(
        "lost flags (replayable)",
        lost_flag_examples,
        args.max_examples,
    );
    print_examples("kinds differ", differ_examples, args.max_examples);
}

fn print_kind_table(counts: &BTreeMap<&'static str, usize>) {
    let mut rows: Vec<(&'static str, usize)> = ALL_KINDS
        .iter()
        .map(|kind| (*kind, counts.get(kind).copied().unwrap_or(0)))
        .collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    for (kind, count) in rows {
        println!("  {count:>6}  {kind}");
    }
}

fn print_examples(label: &str, examples: &[DeltaExample], limit: usize) {
    if examples.is_empty() {
        return;
    }
    println!("\n--- {label} (up to {limit}) ---");
    for example in examples {
        println!(
            "  {} {} stored=[{}] replay=[{}]",
            example.job_id,
            example.segment_id,
            example.stored_kinds.join(","),
            example.replay_kinds.join(",")
        );
    }
}
