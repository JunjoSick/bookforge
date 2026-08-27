//! `judge_translation` — measure translation quality with passage-level defects.
//!
//! This is dev-time measurement tooling. It is deliberately not part of
//! `translate`, never rewrites a translation, and never sends malformed judge
//! output to a repair model. An unparseable response is recorded and excluded
//! from the quality-rate denominator.
//!
//! The primary input is BookForge's job store. `JobStore::open` can migrate a
//! database, so this example copies the database and its WAL sidecars to a
//! temporary directory and opens only that throwaway copy.
//!
//! Start with a dry run. It renders every sampled prompt and prints a catalog-
//! based maximum-cost estimate without calling a provider or writing outputs:
//!
//! ```text
//! cargo run --release --example judge_translation -- --dry-run --sample 25
//! ```
//!
//! A paid run writes passage results as JSONL plus a machine-readable summary:
//!
//! ```text
//! cargo run --release --example judge_translation -- --sample 25
//! ```
//!
//! The API key is never a CLI value. `--api-key-env` accepts only the name of
//! the environment variable that the provider should read at request time.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use bookforge_core::{
    JsonMode, RetryAfterPolicy,
    run_snapshot::RunConfigSnapshot,
    segment::{SegmentBlock, build_segments},
};
use bookforge_epub::read_epub;
use bookforge_llm::{
    CompletionRequest, LlmProvider, OpenAiCompatibleConfig, OpenAiCompatibleProvider,
    RequestMetadata, ResponseFormat,
};
use bookforge_store::{JobRecord, JobStore};
use clap::Parser;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Prompt changes must invalidate the content-addressed cache.
const PROMPT_VERSION: &str = "judge_translation/v1";
const OUTPUT_SCHEMA_VERSION: u32 = 2;
const SUMMARY_SCHEMA_VERSION: u32 = 2;
const DEFAULT_SEED: u64 = 0xB00F_0A6E_2026_0729;
const MAX_JUDGE_RESPONSE_BYTES: usize = 128 * 1024;

const ALL_CATEGORIES: [DefectCategory; 6] = [
    DefectCategory::MeaningChanged,
    DefectCategory::ContentDropped,
    DefectCategory::ContentAdded,
    DefectCategory::TerminologyInconsistent,
    DefectCategory::RegisterShift,
    DefectCategory::TargetLanguageError,
];

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Debug, Parser)]
#[command(
    name = "judge_translation",
    about = "Measure passage-level translation defects (dev-time tooling only)",
    long_about = "Builds contiguous passage-sized units from translated blocks in a throwaway \
                  copy of the BookForge job store, asks a judge to enumerate fixed defect \
                  categories, and reports defects per 1,000 source characters. Start with --dry-run."
)]
struct Args {
    /// Path to the owner's job store. It is copied before JobStore::open.
    #[arg(long, default_value = ".bookforge/jobs.sqlite")]
    db: PathBuf,

    /// Only include these job ids (repeatable).
    #[arg(long = "job")]
    jobs: Vec<String>,

    /// Only include these target languages (repeatable, case-insensitive).
    #[arg(long = "target-lang")]
    target_langs: Vec<String>,

    /// Target source-character budget per passage. Blocks are never split.
    #[arg(long, default_value_t = 1_500)]
    passage_chars: usize,

    /// Number of passages to sample after a seeded shuffle. 0 selects all.
    #[arg(long, default_value_t = 25)]
    sample: usize,

    /// Seed for deterministic passage sampling.
    #[arg(long, default_value_t = DEFAULT_SEED)]
    seed: u64,

    /// Passage result JSONL.
    #[arg(long, default_value = "judge-translation.jsonl")]
    out: PathBuf,

    /// Summary JSON. Defaults beside --out as <stem>.summary.json.
    #[arg(long)]
    summary: Option<PathBuf>,

    /// Prior summary JSON used to print and emit per-category deltas.
    #[arg(long)]
    baseline: Option<PathBuf>,

    /// Provider preset: deepseek, openrouter, or openai-compatible.
    #[arg(long, default_value = "deepseek")]
    provider: String,

    /// Judge model override. Defaults to the provider preset's model.
    #[arg(long)]
    model: Option<String>,

    /// Base URL override. Required for --provider openai-compatible.
    #[arg(long)]
    base_url: Option<String>,

    /// NAME of the environment variable holding the API key, never its value.
    #[arg(long)]
    api_key_env: Option<String>,

    /// Judge sampling temperature.
    #[arg(long, default_value_t = 0.0)]
    temperature: f32,

    /// Judge output cap per passage.
    ///
    /// Reasoning models can return HTTP 200 with empty content when starved of
    /// output tokens. Enumerated defects also need room for two quotes each.
    #[arg(long, default_value_t = 4_096)]
    max_output_tokens: u32,

    /// Per-request timeout in seconds.
    #[arg(long, default_value_t = 120)]
    timeout_seconds: u64,

    /// Render sampled prompts and price the maximum run without provider calls.
    #[arg(long)]
    dry_run: bool,

    /// Directory for content-addressed judge results.
    #[arg(long, default_value = ".bookforge/translation-judge-cache")]
    cache: PathBuf,

    /// Disable the on-disk judge-result cache.
    #[arg(long)]
    no_cache: bool,

    /// Pricing catalog override. The embedded CLI catalog is used by default.
    #[arg(long)]
    pricing: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// Frozen schemas
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DefectCategory {
    MeaningChanged,
    ContentDropped,
    ContentAdded,
    TerminologyInconsistent,
    RegisterShift,
    TargetLanguageError,
}

impl DefectCategory {
    fn as_str(self) -> &'static str {
        match self {
            Self::MeaningChanged => "meaning_changed",
            Self::ContentDropped => "content_dropped",
            Self::ContentAdded => "content_added",
            Self::TerminologyInconsistent => "terminology_inconsistent",
            Self::RegisterShift => "register_shift",
            Self::TargetLanguageError => "target_language_error",
        }
    }

    fn group(self) -> DefectGroup {
        match self {
            Self::MeaningChanged | Self::ContentDropped | Self::ContentAdded => DefectGroup::Hard,
            Self::TerminologyInconsistent | Self::RegisterShift | Self::TargetLanguageError => {
                DefectGroup::Soft
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DefectGroup {
    Hard,
    Soft,
}

impl DefectGroup {
    fn as_str(self) -> &'static str {
        match self {
            Self::Hard => "hard",
            Self::Soft => "soft",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Defect {
    category: DefectCategory,
    source_quote: String,
    translation_quote: String,
    explanation: String,
}

/// A missing quote is represented as `None` so that one bad finding can be
/// dropped without making an otherwise valid response unparseable.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDefect {
    category: DefectCategory,
    source_quote: Option<String>,
    translation_quote: Option<String>,
    explanation: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JudgeResponse {
    defects: Vec<RawDefect>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RecordStatus {
    Parsed,
    Unparseable,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PassageRecord {
    schema_version: u32,
    prompt_version: String,
    seed: u64,
    passage_id: String,
    content_hash: String,
    job_id: String,
    section_id: String,
    section_title: Option<String>,
    segment_ids: Vec<String>,
    block_ids: Vec<String>,
    source_language: String,
    target_language: String,
    source_chars: usize,
    status: RecordStatus,
    defects: Vec<Defect>,
    raw_defect_count: usize,
    dropped_missing_quote: usize,
    dropped_non_verbatim_quote: usize,
    #[serde(default)]
    dropped_self_refuting: usize,
    cached: bool,
    input_tokens: u64,
    output_tokens: u64,
    error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CategoryMetric {
    category: DefectCategory,
    count: usize,
    per_1k_source_chars: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GroupMetric {
    group: DefectGroup,
    count: usize,
    per_1k_source_chars: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CategoryDelta {
    category: DefectCategory,
    count_delta: i64,
    per_1k_source_chars_delta: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BaselineDelta {
    baseline_seed: u64,
    baseline_passages_judged: usize,
    categories: Vec<CategoryDelta>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct QualitySummary {
    schema_version: u32,
    prompt_version: String,
    provider: String,
    model: String,
    seed: u64,
    passage_chars: usize,
    passages_available: usize,
    passages_sampled: usize,
    passages_judged: usize,
    source_chars_judged: usize,
    unparseable_responses: usize,
    request_errors: usize,
    dropped_defects: usize,
    #[serde(default)]
    dropped_missing_quote: usize,
    #[serde(default)]
    dropped_non_verbatim_quote: usize,
    #[serde(default)]
    dropped_self_refuting: usize,
    cache_hits: usize,
    provider_calls: usize,
    input_tokens: u64,
    output_tokens: u64,
    categories: Vec<CategoryMetric>,
    groups: Vec<GroupMetric>,
    #[serde(default)]
    baseline_delta: Option<BaselineDelta>,
}

// ---------------------------------------------------------------------------
// Passage assembly
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct EvaluationBlock {
    job_id: String,
    section_id: String,
    section_title: Option<String>,
    section_position: usize,
    segment_id: String,
    block_id: String,
    source_language: String,
    target_language: String,
    source_text: String,
    translated_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Passage {
    passage_id: String,
    content_hash: String,
    job_id: String,
    section_id: String,
    section_title: Option<String>,
    segment_ids: Vec<String>,
    block_ids: Vec<String>,
    source_language: String,
    target_language: String,
    source_text: String,
    translated_text: String,
    source_chars: usize,
}

/// Assemble a greedy contiguous run. The next block starts a new passage when
/// it belongs to another job/section, follows a missing block, or would take a
/// non-empty passage over the source-character budget. A single oversized block
/// remains whole and therefore may exceed the budget.
fn assemble_passages(blocks: &[EvaluationBlock], passage_chars: usize) -> Vec<Passage> {
    let mut passages = Vec::new();
    let mut current = Vec::<EvaluationBlock>::new();

    for block in blocks {
        let source_chars = block.source_text.chars().count();
        let current_chars = current
            .iter()
            .map(|item| item.source_text.chars().count())
            .sum::<usize>();
        let discontinuous = current.last().is_some_and(|previous| {
            previous.job_id != block.job_id
                || previous.section_id != block.section_id
                || previous.section_position.checked_add(1) != Some(block.section_position)
        });
        let over_budget =
            !current.is_empty() && current_chars.saturating_add(source_chars) > passage_chars;

        if discontinuous || over_budget {
            push_passage(&mut passages, &current);
            current.clear();
        }
        current.push(block.clone());
    }
    push_passage(&mut passages, &current);
    passages
}

fn push_passage(passages: &mut Vec<Passage>, blocks: &[EvaluationBlock]) {
    let Some(first) = blocks.first() else {
        return;
    };
    let source_text = blocks
        .iter()
        .map(|block| block.source_text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    let translated_text = blocks
        .iter()
        .map(|block| block.translated_text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    let block_ids = blocks
        .iter()
        .map(|block| block.block_id.clone())
        .collect::<Vec<_>>();
    let mut segment_ids = Vec::new();
    for block in blocks {
        if segment_ids.last() != Some(&block.segment_id) {
            segment_ids.push(block.segment_id.clone());
        }
    }
    let passage_id = passage_identity(&first.job_id, &first.section_id, &block_ids, &source_text);
    let content_hash = content_hash(&source_text, &translated_text);

    passages.push(Passage {
        passage_id,
        content_hash,
        job_id: first.job_id.clone(),
        section_id: first.section_id.clone(),
        section_title: first.section_title.clone(),
        segment_ids,
        block_ids,
        source_language: first.source_language.clone(),
        target_language: first.target_language.clone(),
        source_chars: blocks
            .iter()
            .map(|block| block.source_text.chars().count())
            .sum(),
        source_text,
        translated_text,
    });
}

fn passage_identity(
    job_id: &str,
    section_id: &str,
    block_ids: &[String],
    source_text: &str,
) -> String {
    let digest = hash_fields(
        std::iter::once(job_id)
            .chain(std::iter::once(section_id))
            .chain(block_ids.iter().map(String::as_str))
            .chain(std::iter::once(source_text)),
    );
    format!("passage_{}", &digest[..20])
}

fn content_hash(source_text: &str, translated_text: &str) -> String {
    hash_fields([source_text, translated_text])
}

// ---------------------------------------------------------------------------
// Store loading
// ---------------------------------------------------------------------------

struct CopiedStore {
    store: JobStore,
    _scratch: tempfile::TempDir,
}

fn open_copied_store(db_path: &Path) -> Result<CopiedStore> {
    let scratch = tempfile::tempdir().context("creating a scratch dir for the store copy")?;
    let copy_path = copy_store(db_path, scratch.path())?;
    let store = JobStore::open(&copy_path).context("opening the copied job store")?;
    Ok(CopiedStore {
        store,
        _scratch: scratch,
    })
}

fn copy_store(db_path: &Path, dir: &Path) -> Result<PathBuf> {
    let name = db_path
        .file_name()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("jobs.sqlite"));
    let target = dir.join(name);
    fs::copy(db_path, &target)
        .with_context(|| format!("copying {} into a scratch dir", db_path.display()))?;
    for suffix in ["-wal", "-shm"] {
        let sidecar = sidecar_path(db_path, suffix);
        if sidecar.exists() {
            fs::copy(&sidecar, sidecar_path(&target, suffix))
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

#[derive(Debug, Default)]
struct CorpusStats {
    jobs_considered: usize,
    jobs_loaded: usize,
    jobs_skipped_config: usize,
    jobs_skipped_snapshot: usize,
    jobs_skipped_epub: usize,
    jobs_skipped_segmentation: usize,
    blocks_eligible: usize,
    blocks_skipped_needs_review: usize,
    blocks_missing_translation: usize,
    blocks_empty_translation: usize,
}

fn load_evaluation_blocks(
    store: &JobStore,
    owner_db_path: &Path,
    args: &Args,
) -> Result<(Vec<EvaluationBlock>, CorpusStats)> {
    let summaries = store
        .list_job_summaries()
        .context("listing jobs from the copied store")?;
    let root = store_root(owner_db_path);
    let job_filter: HashSet<&str> = args.jobs.iter().map(String::as_str).collect();
    let target_filter = args
        .target_langs
        .iter()
        .map(|language| language.to_ascii_lowercase())
        .collect::<Vec<_>>();
    let mut blocks = Vec::new();
    let mut stats = CorpusStats::default();

    for (job, _summary) in summaries {
        if !job_filter.is_empty() && !job_filter.contains(job.id.as_str()) {
            continue;
        }
        if !target_filter.is_empty()
            && !target_filter
                .iter()
                .any(|language| job.target_lang.eq_ignore_ascii_case(language))
        {
            continue;
        }
        stats.jobs_considered += 1;

        let snapshot = match store.load_job_config_snapshot(&job.id) {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => {
                stats.jobs_skipped_config += 1;
                warn(&format!("{}: no run configuration snapshot", job.id));
                continue;
            }
            Err(error) => {
                stats.jobs_skipped_config += 1;
                warn(&format!(
                    "{}: unreadable run configuration: {error}",
                    job.id
                ));
                continue;
            }
        };
        let Some(epub_path) = resolve_snapshot_path(&root, &job, &snapshot) else {
            stats.jobs_skipped_snapshot += 1;
            warn(&format!("{}: input EPUB snapshot is missing", job.id));
            continue;
        };
        let book = match read_epub(&epub_path) {
            Ok(book) => book,
            Err(error) => {
                stats.jobs_skipped_epub += 1;
                warn(&format!("{}: input EPUB cannot be read: {error}", job.id));
                continue;
            }
        };
        let segmentation = snapshot.settings.to_settings().segmentation;
        let segments = match build_segments(&book, &segmentation) {
            Ok(segments) => segments,
            Err(error) => {
                stats.jobs_skipped_segmentation += 1;
                warn(&format!("{}: source segmentation failed: {error}", job.id));
                continue;
            }
        };
        let stored = match store.load_terminal_segment_translations(&job.id) {
            Ok(stored) => stored
                .into_iter()
                .map(|translation| (translation.segment_id.clone(), translation))
                .collect::<HashMap<_, _>>(),
            Err(error) => {
                warn(&format!("{}: translations cannot be read: {error}", job.id));
                continue;
            }
        };

        stats.jobs_loaded += 1;
        let source_language = snapshot
            .source_language
            .clone()
            .filter(|language| !language.trim().is_empty())
            .or_else(|| book.metadata.language.clone())
            .unwrap_or_else(|| "unknown".to_string());
        let target_language = snapshot.target_language.clone();
        let mut section_positions = HashMap::<String, usize>::new();

        for segment in segments {
            let stored_segment = stored.get(&segment.id.0);
            let translated_blocks = stored_segment
                .map(|translation| {
                    translation
                        .blocks
                        .iter()
                        .map(|block| (block.block_id.0.as_str(), block.text.as_str()))
                        .collect::<HashMap<_, _>>()
                })
                .unwrap_or_default();

            for source_block in &segment.source.blocks {
                let position = section_positions
                    .entry(segment.section_id.0.clone())
                    .or_default();
                let section_position = *position;
                *position += 1;

                let Some(stored_segment) = stored_segment else {
                    stats.blocks_missing_translation += 1;
                    continue;
                };
                // A needs-review row intentionally stores preserved source text,
                // not the rejected model output. It is not a translation-quality
                // observation and must not enter the denominator.
                if stored_segment.status == "needs_review" {
                    stats.blocks_skipped_needs_review += 1;
                    continue;
                }
                let Some(translated_text) = translated_blocks.get(source_block.block_id.0.as_str())
                else {
                    stats.blocks_missing_translation += 1;
                    continue;
                };
                if translated_text.trim().is_empty() {
                    stats.blocks_empty_translation += 1;
                    continue;
                }

                stats.blocks_eligible += 1;
                blocks.push(evaluation_block(
                    &job,
                    &segment.id.0,
                    &segment.section_id.0,
                    segment.metadata.section_title.clone(),
                    section_position,
                    source_block,
                    &source_language,
                    &target_language,
                    translated_text,
                ));
            }
        }
    }

    Ok((blocks, stats))
}

#[allow(clippy::too_many_arguments)]
fn evaluation_block(
    job: &JobRecord,
    segment_id: &str,
    section_id: &str,
    section_title: Option<String>,
    section_position: usize,
    source_block: &SegmentBlock,
    source_language: &str,
    target_language: &str,
    translated_text: &str,
) -> EvaluationBlock {
    EvaluationBlock {
        job_id: job.id.clone(),
        section_id: section_id.to_string(),
        section_title,
        section_position,
        segment_id: segment_id.to_string(),
        block_id: source_block.block_id.0.clone(),
        source_language: source_language.to_string(),
        target_language: target_language.to_string(),
        source_text: source_block.text.clone(),
        translated_text: translated_text.to_string(),
    }
}

fn warn(message: &str) {
    eprintln!("warning: {message}");
}

// ---------------------------------------------------------------------------
// Sampling
// ---------------------------------------------------------------------------

fn sample_passages(passages: &[Passage], sample: usize, seed: u64) -> Vec<Passage> {
    let mut indices = (0..passages.len()).collect::<Vec<_>>();
    let mut rng = SplitMix64(seed);
    for upper in (1..indices.len()).rev() {
        let selected = (rng.next_u64() % (upper as u64 + 1)) as usize;
        indices.swap(upper, selected);
    }
    if sample != 0 {
        indices.truncate(sample.min(indices.len()));
    }
    indices
        .into_iter()
        .map(|index| passages[index].clone())
        .collect()
}

/// Small deterministic generator used only to drive Fisher-Yates sampling.
/// Keeping it local avoids adding a random-number dependency for one dev tool.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut value = self.0;
        value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        value ^ (value >> 31)
    }
}

// ---------------------------------------------------------------------------
// Scorer seam and prompt
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct RenderedPrompt {
    system: String,
    user: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CachedOutcome {
    parsed: bool,
    defects: Vec<Defect>,
    raw_defect_count: usize,
    dropped_missing_quote: usize,
    dropped_non_verbatim_quote: usize,
    #[serde(default)]
    dropped_self_refuting: usize,
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct ScoreOutcome {
    cached: CachedOutcome,
    input_tokens: u64,
    output_tokens: u64,
}

#[derive(Debug)]
struct ScoreError(String);

trait TranslationScorer {
    fn name(&self) -> &'static str;

    fn render(&self, passage: &Passage) -> RenderedPrompt;

    fn score(
        &self,
        passage: &Passage,
    ) -> impl std::future::Future<Output = Result<ScoreOutcome, ScoreError>> + Send;
}

struct TextScorer {
    provider: OpenAiCompatibleProvider,
    endpoint: Endpoint,
    temperature: f32,
    max_output_tokens: u32,
}

impl TranslationScorer for TextScorer {
    fn name(&self) -> &'static str {
        "text"
    }

    fn render(&self, passage: &Passage) -> RenderedPrompt {
        render_prompt(passage)
    }

    async fn score(&self, passage: &Passage) -> Result<ScoreOutcome, ScoreError> {
        let prompt = self.render(passage);
        let response = self
            .provider
            .complete(CompletionRequest {
                system: prompt.system,
                user: prompt.user,
                response_format: ResponseFormat::Json,
                temperature: self.temperature,
                max_output_tokens: Some(self.max_output_tokens),
                metadata: RequestMetadata {
                    segment_id: Some(passage.passage_id.clone()),
                    block_ids: passage.block_ids.clone(),
                    prompt_template: Some("judge_translation".to_string()),
                    prompt_version: Some(PROMPT_VERSION.to_string()),
                    provider: Some(self.endpoint.provider.clone()),
                    model: Some(self.endpoint.model.clone()),
                    ..RequestMetadata::default()
                },
            })
            .await
            .map_err(|error| ScoreError(error.to_string()))?;
        Ok(interpret_response(
            &response.content,
            passage,
            response.input_tokens.unwrap_or_default(),
            response.output_tokens.unwrap_or_default(),
        ))
    }
}

const JUDGE_SYSTEM_PROMPT: &str = r#"You are measuring the quality of a translation. Enumerate defects that are demonstrated by the source passage and its translation.

Use only these fixed categories:

meaning_changed - the translation asserts something the source does not, such as a reversed negation, wrong number or date, or swapped entity
content_dropped - source content is absent from the translation
content_added - the translation asserts content with no source basis
terminology_inconsistent - one source term is rendered two different ways within this passage
register_shift - formality or voice diverges from the source without cause
target_language_error - the translation is ungrammatical or unidiomatic in the target language

For every defect, quote an exact, non-empty span from BOTH the source and the translation. A finding without both verbatim spans is unverifiable and will be discarded. Do not assign a score or a severity. Do not combine defects into an overall judgment. If no defects are demonstrated, return an empty defects array.

Return ONLY one JSON object, without prose or code fences:

{"defects":[{"category":"meaning_changed","source_quote":"exact source span","translation_quote":"exact translation span","explanation":"brief explanation"}]}"#;

fn render_prompt(passage: &Passage) -> RenderedPrompt {
    RenderedPrompt {
        system: JUDGE_SYSTEM_PROMPT.to_string(),
        user: format!(
            "Source language: {}\n\
             Target language: {}\n\
             Passage: {} (job {}, section {})\n\
             \n\
             Source passage:\n\
             <<<SOURCE\n\
             {}\n\
             SOURCE\n\
             \n\
             Translation:\n\
             <<<TRANSLATION\n\
             {}\n\
             TRANSLATION\n\
             \n\
             Enumerate demonstrated defects using the required JSON object only.\n",
            passage.source_language,
            passage.target_language,
            passage.passage_id,
            passage.job_id,
            passage.section_id,
            passage.source_text,
            passage.translated_text,
        ),
    }
}

fn interpret_response(
    content: &str,
    passage: &Passage,
    input_tokens: u64,
    output_tokens: u64,
) -> ScoreOutcome {
    let unparseable = |reason: &str| ScoreOutcome {
        cached: CachedOutcome {
            parsed: false,
            defects: Vec::new(),
            raw_defect_count: 0,
            dropped_missing_quote: 0,
            dropped_non_verbatim_quote: 0,
            dropped_self_refuting: 0,
            error: Some(reason.to_string()),
        },
        input_tokens,
        output_tokens,
    };

    if content.len() > MAX_JUDGE_RESPONSE_BYTES {
        return unparseable("judge response exceeded the response-size bound");
    }
    let body = strip_json_code_fence(content.trim());
    let Ok(response) = serde_json::from_str::<JudgeResponse>(body) else {
        // Terminal by design. A second model is never used to repair output.
        return unparseable("judge response was not a valid defect object");
    };

    let raw_defect_count = response.defects.len();
    let mut defects = Vec::new();
    let mut dropped_missing_quote = 0usize;
    let mut dropped_non_verbatim_quote = 0usize;
    let mut dropped_self_refuting = 0usize;
    for raw in response.defects {
        let Some(source_quote) = nonempty_quote(raw.source_quote) else {
            dropped_missing_quote += 1;
            continue;
        };
        let Some(translation_quote) = nonempty_quote(raw.translation_quote) else {
            dropped_missing_quote += 1;
            continue;
        };
        if !passage.source_text.contains(&source_quote)
            || !passage.translated_text.contains(&translation_quote)
        {
            dropped_non_verbatim_quote += 1;
            continue;
        }
        if is_self_refuting_explanation(&raw.explanation) {
            dropped_self_refuting += 1;
            continue;
        }
        defects.push(Defect {
            category: raw.category,
            source_quote,
            translation_quote,
            explanation: raw.explanation,
        });
    }

    ScoreOutcome {
        cached: CachedOutcome {
            parsed: true,
            defects,
            raw_defect_count,
            dropped_missing_quote,
            dropped_non_verbatim_quote,
            dropped_self_refuting,
            error: None,
        },
        input_tokens,
        output_tokens,
    }
}

fn nonempty_quote(quote: Option<String>) -> Option<String> {
    let quote = quote?;
    let trimmed = quote.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Drop only explanations that unambiguously dismiss their own complaint.
///
/// This is intentionally lexical and conservative. Prompt-only attempts at
/// suppressing non-issues have increased finding volume in prior measurements,
/// while a deterministic filter is stable and auditable. Any contrast marker
/// or separate defect assertion keeps the finding, so mixed explanations such
/// as "X is correct, but Y is wrong" are never discarded.
fn is_self_refuting_explanation(explanation: &str) -> bool {
    let words = explanation
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect::<Vec<_>>();

    const DISMISSALS: &[&[&str]] = &[
        &["no", "error"],
        &["no", "errors"],
        &["no", "issue"],
        &["no", "issues"],
        &["no", "problem"],
        &["no", "problems"],
        &["nothing", "wrong"],
        &["is", "correct"],
        &["is", "accurate"],
        &["correctly", "translated"],
        &["translated", "correctly"],
        &["accurately", "translated"],
        &["translated", "accurately"],
    ];
    let has_dismissal = DISMISSALS
        .iter()
        .any(|phrase| contains_word_sequence(&words, phrase));
    if !has_dismissal {
        return false;
    }

    // Negated and hypothetical correctness claims are not dismissals. Keeping
    // the entire explanation on these markers is deliberately conservative.
    const DISMISSAL_BLOCKERS: &[&str] = &["could", "hardly", "never", "not", "should", "would"];
    if words
        .iter()
        .any(|word| DISMISSAL_BLOCKERS.contains(&word.as_str()))
    {
        return false;
    }

    const CONTRAST_MARKERS: &[&str] = &[
        "although",
        "but",
        "except",
        "however",
        "nevertheless",
        "though",
        "yet",
    ];
    if words
        .iter()
        .any(|word| CONTRAST_MARKERS.contains(&word.as_str()))
    {
        return false;
    }

    const DEFECT_ASSERTIONS: &[&str] = &[
        "added",
        "adds",
        "changed",
        "changes",
        "dropped",
        "fails",
        "incorrect",
        "inconsistent",
        "missing",
        "mistranslated",
        "omitted",
        "ungrammatical",
        "unidiomatic",
        "wrong",
    ];
    if words
        .iter()
        .any(|word| DEFECT_ASSERTIONS.contains(&word.as_str()))
    {
        return false;
    }

    !words.iter().enumerate().any(|(index, word)| {
        matches!(
            word.as_str(),
            "error" | "errors" | "issue" | "issues" | "problem" | "problems"
        ) && index
            .checked_sub(1)
            .is_none_or(|previous| words[previous] != "no")
    })
}

fn contains_word_sequence(words: &[String], phrase: &[&str]) -> bool {
    words
        .windows(phrase.len())
        .any(|window| window.iter().map(String::as_str).eq(phrase.iter().copied()))
}

/// Deterministically unwrap a whole-response JSON fence. Contents are not
/// rewritten; a still-invalid payload remains unparseable.
fn strip_json_code_fence(body: &str) -> &str {
    let Some(inner) = body.strip_prefix("```") else {
        return body;
    };
    let Some(inner) = inner.strip_suffix("```") else {
        return body;
    };
    let Some((tag, payload)) = inner.split_once('\n') else {
        return body;
    };
    let tag = tag.trim();
    if tag.is_empty() || tag.eq_ignore_ascii_case("json") {
        payload.trim()
    } else {
        body
    }
}

// ---------------------------------------------------------------------------
// Provider, cache, and pricing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Endpoint {
    provider: String,
    base_url: String,
    /// Environment-variable name only. Its value is never read by this tool.
    api_key_env: String,
    model: String,
}

fn resolve_endpoint(args: &Args) -> Result<Endpoint> {
    let defaults = match args.provider.as_str() {
        "deepseek" | "openrouter" | "openai-compatible" => {
            bookforge_core::providers::provider_defaults(&args.provider)
                .expect("allow-list above matches registry entries")
        }
        other => {
            bail!("unsupported provider '{other}'; use deepseek, openrouter, or openai-compatible")
        }
    };
    if defaults.base_url.is_none() && args.base_url.is_none() {
        bail!("--provider openai-compatible requires --base-url");
    }
    Ok(Endpoint {
        provider: args.provider.clone(),
        base_url: args
            .base_url
            .clone()
            .unwrap_or_else(|| defaults.base_url.unwrap_or_default().to_string()),
        api_key_env: args
            .api_key_env
            .clone()
            .unwrap_or_else(|| defaults.api_key_env.to_string()),
        model: args.model.clone().unwrap_or_else(|| {
            defaults
                .default_model
                .unwrap_or(bookforge_core::providers::LOCAL_MODEL_PLACEHOLDER)
                .to_string()
        }),
    })
}

fn build_text_scorer(args: &Args, endpoint: &Endpoint) -> Result<TextScorer> {
    let provider = OpenAiCompatibleProvider::new(OpenAiCompatibleConfig {
        base_url: endpoint.base_url.clone(),
        api_key_env: endpoint.api_key_env.clone(),
        model: endpoint.model.clone(),
        timeout_seconds: args.timeout_seconds,
        provider_max_attempts: 3,
        thinking_disabled: true,
        retry_after_policy: RetryAfterPolicy::JitteredExponential,
        max_backoff_seconds: 30,
        max_idle_per_host: 4,
        json_mode: JsonMode::Auto,
    })
    .map_err(|error| anyhow::anyhow!("building provider: {error}"))?;
    Ok(TextScorer {
        provider,
        endpoint: endpoint.clone(),
        temperature: args.temperature,
        max_output_tokens: args.max_output_tokens,
    })
}

struct OutcomeCache {
    dir: Option<PathBuf>,
}

impl OutcomeCache {
    fn get(&self, key: &str) -> Option<CachedOutcome> {
        let path = self.dir.as_ref()?.join(format!("{key}.json"));
        let mut outcome =
            serde_json::from_str::<CachedOutcome>(&fs::read_to_string(path).ok()?).ok()?;
        // Cache entries written before this deterministic filter existed still
        // receive the current filtering semantics without another paid call.
        let before = outcome.defects.len();
        outcome
            .defects
            .retain(|defect| !is_self_refuting_explanation(&defect.explanation));
        outcome.dropped_self_refuting = outcome
            .dropped_self_refuting
            .saturating_add(before.saturating_sub(outcome.defects.len()));
        Some(outcome)
    }

    fn put(&self, key: &str, outcome: &CachedOutcome) {
        let Some(dir) = self.dir.as_ref() else {
            return;
        };
        if let Err(error) = fs::create_dir_all(dir) {
            warn(&format!(
                "cannot create cache directory {}: {error}",
                dir.display()
            ));
            return;
        }
        let body = match serde_json::to_string(outcome) {
            Ok(body) => body,
            Err(error) => {
                warn(&format!("cannot serialize cache entry: {error}"));
                return;
            }
        };
        let path = dir.join(format!("{key}.json"));
        if let Err(error) = fs::write(&path, body) {
            warn(&format!(
                "cannot write cache entry {}: {error}",
                path.display()
            ));
        }
    }
}

fn cache_key(
    scorer: &str,
    endpoint: &Endpoint,
    passage: &Passage,
    temperature: f32,
    max_output_tokens: u32,
) -> String {
    let temperature = temperature.to_string();
    let output_tokens = max_output_tokens.to_string();
    hash_fields([
        PROMPT_VERSION,
        scorer,
        endpoint.provider.as_str(),
        endpoint.model.as_str(),
        temperature.as_str(),
        output_tokens.as_str(),
        passage.source_language.as_str(),
        passage.target_language.as_str(),
        passage.source_text.as_str(),
        passage.translated_text.as_str(),
    ])
}

fn hash_fields<'a>(fields: impl IntoIterator<Item = &'a str>) -> String {
    const SEPARATOR: &[u8] = b"\x1f";
    let mut hasher = Sha256::new();
    for field in fields {
        hasher.update(field.as_bytes());
        hasher.update(SEPARATOR);
    }
    hex_digest(hasher.finalize().as_slice())
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

// Pricing routes through the shared core catalog; the judge tools keep no
// local copy of the schema or embedded JSON.

type PricingCatalog = bookforge_core::providers::PricingCatalog;

fn load_pricing(path: Option<&Path>) -> Result<PricingCatalog> {
    bookforge_core::providers::load_pricing(path).map_err(anyhow::Error::from)
}

fn estimate_tokens(text: &str) -> u64 {
    bookforge_core::segment::estimate_tokens(text) as u64
}

// ---------------------------------------------------------------------------
// Harness and reporting
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct RunResult {
    records: Vec<PassageRecord>,
    cache_hits: usize,
    provider_calls: usize,
    request_errors: usize,
    input_tokens: u64,
    output_tokens: u64,
}

#[allow(clippy::too_many_arguments)]
async fn run_scoring<S: TranslationScorer>(
    scorer: &S,
    endpoint: &Endpoint,
    passages: &[Passage],
    cache: &OutcomeCache,
    seed: u64,
    temperature: f32,
    max_output_tokens: u32,
    sink: &mut dyn Write,
) -> Result<RunResult> {
    let mut run = RunResult::default();
    for passage in passages {
        let key = cache_key(
            scorer.name(),
            endpoint,
            passage,
            temperature,
            max_output_tokens,
        );
        let (outcome, cached) = if let Some(outcome) = cache.get(&key) {
            run.cache_hits += 1;
            (
                ScoreOutcome {
                    cached: outcome,
                    input_tokens: 0,
                    output_tokens: 0,
                },
                true,
            )
        } else {
            match scorer.score(passage).await {
                Ok(outcome) => {
                    run.provider_calls += 1;
                    cache.put(&key, &outcome.cached);
                    (outcome, false)
                }
                Err(error) => {
                    run.provider_calls += 1;
                    run.request_errors += 1;
                    warn(&format!(
                        "{}: judge request failed: {}",
                        passage.passage_id, error.0
                    ));
                    let record = passage_record(
                        passage,
                        seed,
                        RecordStatus::Error,
                        Vec::new(),
                        0,
                        0,
                        0,
                        0,
                        false,
                        0,
                        0,
                        Some(error.0),
                    );
                    write_record(sink, &record)?;
                    run.records.push(record);
                    continue;
                }
            }
        };

        run.input_tokens += outcome.input_tokens;
        run.output_tokens += outcome.output_tokens;
        let status = if outcome.cached.parsed {
            RecordStatus::Parsed
        } else {
            RecordStatus::Unparseable
        };
        let record = passage_record(
            passage,
            seed,
            status,
            outcome.cached.defects,
            outcome.cached.raw_defect_count,
            outcome.cached.dropped_missing_quote,
            outcome.cached.dropped_non_verbatim_quote,
            outcome.cached.dropped_self_refuting,
            cached,
            outcome.input_tokens,
            outcome.output_tokens,
            outcome.cached.error,
        );
        write_record(sink, &record)?;
        run.records.push(record);
    }
    Ok(run)
}

#[allow(clippy::too_many_arguments)]
fn passage_record(
    passage: &Passage,
    seed: u64,
    status: RecordStatus,
    defects: Vec<Defect>,
    raw_defect_count: usize,
    dropped_missing_quote: usize,
    dropped_non_verbatim_quote: usize,
    dropped_self_refuting: usize,
    cached: bool,
    input_tokens: u64,
    output_tokens: u64,
    error: Option<String>,
) -> PassageRecord {
    PassageRecord {
        schema_version: OUTPUT_SCHEMA_VERSION,
        prompt_version: PROMPT_VERSION.to_string(),
        seed,
        passage_id: passage.passage_id.clone(),
        content_hash: passage.content_hash.clone(),
        job_id: passage.job_id.clone(),
        section_id: passage.section_id.clone(),
        section_title: passage.section_title.clone(),
        segment_ids: passage.segment_ids.clone(),
        block_ids: passage.block_ids.clone(),
        source_language: passage.source_language.clone(),
        target_language: passage.target_language.clone(),
        source_chars: passage.source_chars,
        status,
        defects,
        raw_defect_count,
        dropped_missing_quote,
        dropped_non_verbatim_quote,
        dropped_self_refuting,
        cached,
        input_tokens,
        output_tokens,
        error,
    }
}

fn write_record(sink: &mut dyn Write, record: &PassageRecord) -> Result<()> {
    let line = serde_json::to_string(record).context("serializing passage result")?;
    writeln!(sink, "{line}").context("writing passage result")?;
    Ok(())
}

fn build_summary(
    args: &Args,
    endpoint: &Endpoint,
    passages_available: usize,
    run: &RunResult,
) -> QualitySummary {
    let parsed = run
        .records
        .iter()
        .filter(|record| record.status == RecordStatus::Parsed)
        .collect::<Vec<_>>();
    let source_chars_judged = parsed
        .iter()
        .map(|record| record.source_chars)
        .sum::<usize>();
    let mut counts = BTreeMap::<DefectCategory, usize>::new();
    for record in &parsed {
        for defect in &record.defects {
            *counts.entry(defect.category).or_default() += 1;
        }
    }
    let categories = ALL_CATEGORIES
        .iter()
        .map(|category| {
            let count = counts.get(category).copied().unwrap_or_default();
            CategoryMetric {
                category: *category,
                count,
                per_1k_source_chars: rate_per_1k(count, source_chars_judged),
            }
        })
        .collect::<Vec<_>>();
    let groups = [DefectGroup::Hard, DefectGroup::Soft]
        .iter()
        .map(|group| {
            let count = ALL_CATEGORIES
                .iter()
                .filter(|category| category.group() == *group)
                .map(|category| counts.get(category).copied().unwrap_or_default())
                .sum();
            GroupMetric {
                group: *group,
                count,
                per_1k_source_chars: rate_per_1k(count, source_chars_judged),
            }
        })
        .collect();

    let dropped_missing_quote: usize = parsed
        .iter()
        .map(|record| record.dropped_missing_quote)
        .sum();
    let dropped_non_verbatim_quote: usize = parsed
        .iter()
        .map(|record| record.dropped_non_verbatim_quote)
        .sum();
    let dropped_self_refuting: usize = parsed
        .iter()
        .map(|record| record.dropped_self_refuting)
        .sum();

    QualitySummary {
        schema_version: SUMMARY_SCHEMA_VERSION,
        prompt_version: PROMPT_VERSION.to_string(),
        provider: endpoint.provider.clone(),
        model: endpoint.model.clone(),
        seed: args.seed,
        passage_chars: args.passage_chars,
        passages_available,
        passages_sampled: run.records.len(),
        passages_judged: parsed.len(),
        source_chars_judged,
        unparseable_responses: run
            .records
            .iter()
            .filter(|record| record.status == RecordStatus::Unparseable)
            .count(),
        request_errors: run.request_errors,
        dropped_defects: dropped_missing_quote
            .saturating_add(dropped_non_verbatim_quote)
            .saturating_add(dropped_self_refuting),
        dropped_missing_quote,
        dropped_non_verbatim_quote,
        dropped_self_refuting,
        cache_hits: run.cache_hits,
        provider_calls: run.provider_calls,
        input_tokens: run.input_tokens,
        output_tokens: run.output_tokens,
        categories,
        groups,
        baseline_delta: None,
    }
}

fn rate_per_1k(count: usize, source_chars: usize) -> f64 {
    if source_chars == 0 {
        0.0
    } else {
        count as f64 * 1_000.0 / source_chars as f64
    }
}

fn compute_baseline_delta(current: &QualitySummary, baseline: &QualitySummary) -> BaselineDelta {
    let baseline_by_category = baseline
        .categories
        .iter()
        .map(|metric| (metric.category, metric))
        .collect::<BTreeMap<_, _>>();
    let categories = current
        .categories
        .iter()
        .map(|metric| {
            let baseline_metric = baseline_by_category.get(&metric.category);
            CategoryDelta {
                category: metric.category,
                count_delta: metric.count as i64
                    - baseline_metric.map_or(0, |baseline| baseline.count as i64),
                per_1k_source_chars_delta: metric.per_1k_source_chars
                    - baseline_metric.map_or(0.0, |baseline| baseline.per_1k_source_chars),
            }
        })
        .collect();
    BaselineDelta {
        baseline_seed: baseline.seed,
        baseline_passages_judged: baseline.passages_judged,
        categories,
    }
}

fn read_baseline(path: &Path) -> Result<QualitySummary> {
    let summary: QualitySummary = serde_json::from_str(
        &fs::read_to_string(path)
            .with_context(|| format!("reading baseline {}", path.display()))?,
    )
    .with_context(|| format!("parsing baseline {}", path.display()))?;
    if summary.schema_version != SUMMARY_SCHEMA_VERSION {
        bail!(
            "unsupported baseline schema_version {}; expected {}",
            summary.schema_version,
            SUMMARY_SCHEMA_VERSION
        );
    }
    Ok(summary)
}

fn print_human_report(summary: &QualitySummary, corpus: &CorpusStats, owner_db: &Path) {
    println!("=== Translation quality measurement ===");
    println!("store              : {}", owner_db.display());
    println!("store access       : throwaway copy; original never opened");
    println!(
        "judge              : {} / {}",
        summary.provider, summary.model
    );
    println!("prompt             : {}", summary.prompt_version);
    println!("sample seed        : {}", summary.seed);
    println!(
        "passage budget     : {} source chars",
        summary.passage_chars
    );
    println!(
        "passages           : {} available, {} sampled, {} judged",
        summary.passages_available, summary.passages_sampled, summary.passages_judged
    );
    println!("source chars (n)   : {}", summary.source_chars_judged);
    println!(
        "unparseable/errors : {}/{}",
        summary.unparseable_responses, summary.request_errors
    );
    println!("dropped defects    : {}", summary.dropped_defects);
    println!("  missing quote    : {}", summary.dropped_missing_quote);
    println!(
        "  non-verbatim     : {}",
        summary.dropped_non_verbatim_quote
    );
    println!("  self-refuting    : {}", summary.dropped_self_refuting);
    println!(
        "cache/calls        : {}/{}",
        summary.cache_hits, summary.provider_calls
    );
    println!(
        "provider tokens    : {} input, {} output",
        summary.input_tokens, summary.output_tokens
    );
    println!(
        "corpus jobs        : {} considered, {} loaded",
        corpus.jobs_considered, corpus.jobs_loaded
    );
    println!(
        "corpus skips       : config {}, snapshot {}, epub {}, segmentation {}",
        corpus.jobs_skipped_config,
        corpus.jobs_skipped_snapshot,
        corpus.jobs_skipped_epub,
        corpus.jobs_skipped_segmentation
    );
    println!(
        "block skips        : needs-review {}, missing {}, empty {}",
        corpus.blocks_skipped_needs_review,
        corpus.blocks_missing_translation,
        corpus.blocks_empty_translation
    );

    println!("\ncategory                    count   defects/1k source chars");
    println!("----------------------------------------------------------");
    for metric in &summary.categories {
        println!(
            "{:<27} {:>5} {:>25.3}",
            metric.category.as_str(),
            metric.count,
            metric.per_1k_source_chars
        );
    }
    println!("\ngroup                       count   defects/1k source chars");
    println!("----------------------------------------------------------");
    for metric in &summary.groups {
        println!(
            "{:<27} {:>5} {:>25.3}",
            metric.group.as_str(),
            metric.count,
            metric.per_1k_source_chars
        );
    }
    if let Some(delta) = &summary.baseline_delta {
        println!(
            "\n=== Delta from baseline (seed {}, n={}) ===",
            delta.baseline_seed, delta.baseline_passages_judged
        );
        println!("category                    count delta   rate delta");
        println!("----------------------------------------------------");
        for row in &delta.categories {
            println!(
                "{:<27} {:>+11} {:>+12.3}",
                row.category.as_str(),
                row.count_delta,
                row.per_1k_source_chars_delta
            );
        }
    }
}

fn print_dry_run(
    args: &Args,
    endpoint: &Endpoint,
    passages: &[Passage],
    available: usize,
    corpus: &CorpusStats,
) -> Result<()> {
    let mut input_tokens = 0u64;
    for passage in passages {
        let prompt = render_prompt(passage);
        input_tokens += estimate_tokens(&prompt.system) + estimate_tokens(&prompt.user);
        println!(
            "----- {} / {} [{} blocks, {} source chars] -----",
            passage.job_id,
            passage.passage_id,
            passage.block_ids.len(),
            passage.source_chars
        );
        println!("--- system prompt ---");
        println!("{}", prompt.system);
        println!("--- user prompt ---");
        println!("{}", prompt.user);
    }
    let output_cap = passages.len() as u64 * u64::from(args.max_output_tokens);
    let pricing = load_pricing(args.pricing.as_deref())?;
    let label = pricing.source_label();

    println!("\n=== Dry run estimate ===");
    println!("mode               : DRY RUN - no provider calls or output writes");
    println!("store access       : throwaway copy; original never opened");
    println!(
        "provider/model     : {} / {}",
        endpoint.provider, endpoint.model
    );
    println!("key env name       : {}", endpoint.api_key_env);
    println!("passages available : {available}");
    println!(
        "passages sampled   : {} (seed {})",
        passages.len(),
        args.seed
    );
    println!("passage budget     : {} source chars", args.passage_chars);
    println!("eligible blocks    : {}", corpus.blocks_eligible);
    println!("estimated input    : {input_tokens} tokens");
    println!("output token cap   : {output_cap} tokens");
    match pricing.token_prices(&endpoint.provider, &endpoint.model) {
        Some(model) => {
            let maximum_cost = input_tokens as f64 / 1_000_000.0 * model.input_per_million
                + output_cap as f64 / 1_000_000.0 * model.output_per_million;
            println!("maximum cost       : ${maximum_cost:.6} ({label})");
        }
        None => println!(
            "maximum cost       : unavailable; no {}/{} entry in {}",
            endpoint.provider, endpoint.model, label
        ),
    }
    Ok(())
}

fn summary_path(args: &Args) -> PathBuf {
    args.summary
        .clone()
        .unwrap_or_else(|| args.out.with_extension("summary.json"))
}

fn create_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    if args.passage_chars == 0 {
        bail!("--passage-chars must be greater than zero");
    }
    if args.max_output_tokens < 4_000 {
        warn(
            "a judge output cap below 4000 can produce HTTP 200 with empty content on reasoning models",
        );
    }
    if !args.db.exists() {
        bail!("job store not found: {}", args.db.display());
    }
    if args.dry_run && args.baseline.is_some() {
        bail!("--baseline applies to a completed measurement, not --dry-run");
    }

    let endpoint = resolve_endpoint(&args)?;
    let copied = open_copied_store(&args.db)?;
    let (blocks, corpus) = load_evaluation_blocks(&copied.store, &args.db, &args)?;
    let passages = assemble_passages(&blocks, args.passage_chars);
    let available = passages.len();
    let sampled = sample_passages(&passages, args.sample, args.seed);
    if sampled.is_empty() {
        bail!("no evaluatable translated passages matched the filters");
    }
    if args.sample == 0 {
        warn(&format!(
            "--sample 0 selects all {available} passages and removes the spend cap"
        ));
    }

    if args.dry_run {
        return print_dry_run(&args, &endpoint, &sampled, available, &corpus);
    }

    let summary_path = summary_path(&args);
    if args.out == summary_path {
        bail!("--out and --summary must name different files");
    }
    if args
        .baseline
        .as_ref()
        .is_some_and(|baseline| baseline == &args.out || baseline == &summary_path)
    {
        bail!("--baseline must not be overwritten by --out or --summary");
    }
    // Validate the baseline before any paid work. A malformed comparison file
    // must fail before provider calls, not after them.
    let baseline = args.baseline.as_deref().map(read_baseline).transpose()?;
    create_parent(&args.out)?;
    create_parent(&summary_path)?;
    let mut output = BufWriter::new(
        fs::File::create(&args.out)
            .with_context(|| format!("creating result JSONL {}", args.out.display()))?,
    );
    let scorer = build_text_scorer(&args, &endpoint)?;
    let cache = OutcomeCache {
        dir: (!args.no_cache).then(|| args.cache.clone()),
    };
    let run = run_scoring(
        &scorer,
        &endpoint,
        &sampled,
        &cache,
        args.seed,
        args.temperature,
        args.max_output_tokens,
        &mut output,
    )
    .await?;
    output.flush().context("flushing result JSONL")?;

    let mut summary = build_summary(&args, &endpoint, available, &run);
    if let Some(baseline) = baseline.as_ref() {
        summary.baseline_delta = Some(compute_baseline_delta(&summary, baseline));
    }
    fs::write(
        &summary_path,
        serde_json::to_string_pretty(&summary).context("serializing summary JSON")?,
    )
    .with_context(|| format!("writing summary {}", summary_path.display()))?;

    print_human_report(&summary, &corpus, &args.db);
    println!("\nresults JSONL       : {}", args.out.display());
    println!("summary JSON       : {}", summary_path.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// Offline tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn block(job: &str, section: &str, position: usize, id: &str, source: &str) -> EvaluationBlock {
        EvaluationBlock {
            job_id: job.to_string(),
            section_id: section.to_string(),
            section_title: Some(section.to_string()),
            section_position: position,
            segment_id: format!("segment-{section}"),
            block_id: id.to_string(),
            source_language: "English".to_string(),
            target_language: "Italian".to_string(),
            source_text: source.to_string(),
            translated_text: format!("it-{source}"),
        }
    }

    fn passage(index: usize) -> Passage {
        let source = format!("source passage number {index}");
        Passage {
            passage_id: format!("passage-{index}"),
            content_hash: format!("hash-{index}"),
            job_id: "job".to_string(),
            section_id: "section".to_string(),
            section_title: None,
            segment_ids: vec!["segment".to_string()],
            block_ids: vec![format!("block-{index}")],
            source_language: "English".to_string(),
            target_language: "Italian".to_string(),
            source_chars: source.chars().count(),
            source_text: source,
            translated_text: format!("traduzione numero {index}"),
        }
    }

    fn parsed_record(source_chars: usize, defects: Vec<Defect>) -> PassageRecord {
        PassageRecord {
            schema_version: OUTPUT_SCHEMA_VERSION,
            prompt_version: PROMPT_VERSION.to_string(),
            seed: 7,
            passage_id: "p".to_string(),
            content_hash: "h".to_string(),
            job_id: "j".to_string(),
            section_id: "s".to_string(),
            section_title: None,
            segment_ids: Vec::new(),
            block_ids: Vec::new(),
            source_language: "English".to_string(),
            target_language: "Italian".to_string(),
            source_chars,
            status: RecordStatus::Parsed,
            defects,
            raw_defect_count: 0,
            dropped_missing_quote: 0,
            dropped_non_verbatim_quote: 0,
            dropped_self_refuting: 0,
            cached: false,
            input_tokens: 0,
            output_tokens: 0,
            error: None,
        }
    }

    fn test_args() -> Args {
        Args {
            db: PathBuf::from("jobs.sqlite"),
            jobs: Vec::new(),
            target_langs: Vec::new(),
            passage_chars: 1_500,
            sample: 25,
            seed: 7,
            out: PathBuf::from("out.jsonl"),
            summary: None,
            baseline: None,
            provider: "deepseek".to_string(),
            model: None,
            base_url: None,
            api_key_env: None,
            temperature: 0.0,
            max_output_tokens: 4_096,
            timeout_seconds: 120,
            dry_run: false,
            cache: PathBuf::from("cache"),
            no_cache: true,
            pricing: None,
        }
    }

    fn test_endpoint() -> Endpoint {
        Endpoint {
            provider: "mock".to_string(),
            base_url: "http://invalid".to_string(),
            api_key_env: "MOCK_KEY".to_string(),
            model: "mock-model".to_string(),
        }
    }

    #[test]
    fn passage_assembly_respects_budget_boundaries_and_gaps() {
        let blocks = vec![
            block("job", "one", 0, "a", "abc"),
            block("job", "one", 1, "b", "def"),
            block("job", "one", 2, "c", "oversized"),
            // Position 3 is deliberately absent, so this cannot join c.
            block("job", "one", 4, "d", "xy"),
            block("job", "two", 0, "e", "zzz"),
        ];
        let passages = assemble_passages(&blocks, 6);
        let ids = passages
            .iter()
            .map(|passage| passage.block_ids.clone())
            .collect::<Vec<_>>();

        assert_eq!(
            ids,
            vec![
                vec!["a".to_string(), "b".to_string()],
                vec!["c".to_string()],
                vec!["d".to_string()],
                vec!["e".to_string()],
            ]
        );
        assert_eq!(passages[0].source_chars, 6);
        assert_eq!(passages[1].source_text, "oversized");
        assert!(passages[1].source_chars > 6);
        assert_ne!(passages[2].section_id, passages[3].section_id);
    }

    #[test]
    fn missing_or_non_verbatim_quotes_are_dropped_and_counted() {
        let passage = Passage {
            source_text: "The source has a date: December 8.".to_string(),
            translated_text: "La traduzione dice: 10 dicembre.".to_string(),
            ..passage(1)
        };
        let response = r#"{"defects":[
            {"category":"meaning_changed","source_quote":"December 8","translation_quote":"10 dicembre","explanation":"wrong date"},
            {"category":"content_dropped","translation_quote":"10 dicembre","explanation":"missing source quote"},
            {"category":"content_added","source_quote":"December 8","translation_quote":"","explanation":"empty translation quote"},
            {"category":"register_shift","source_quote":"invented source","translation_quote":"10 dicembre","explanation":"not verbatim"}
        ]}"#;
        let outcome = interpret_response(response, &passage, 11, 12);

        assert!(outcome.cached.parsed);
        assert_eq!(outcome.cached.raw_defect_count, 4);
        assert_eq!(outcome.cached.defects.len(), 1);
        assert_eq!(outcome.cached.dropped_missing_quote, 2);
        assert_eq!(outcome.cached.dropped_non_verbatim_quote, 1);
        assert_eq!(outcome.cached.dropped_self_refuting, 0);
    }

    #[test]
    fn self_refuting_findings_are_dropped_but_mixed_findings_are_kept() {
        let passage = Passage {
            source_text: "Grand Seneschal and the steward".to_string(),
            translated_text: "Gran Siniscalco e l'amministratore".to_string(),
            ..passage(1)
        };
        let response = r#"{"defects":[
            {"category":"target_language_error","source_quote":"Grand Seneschal","translation_quote":"Gran Siniscalco","explanation":"'Grand Seneschal' is correctly translated as 'Gran Siniscalco'. No error."},
            {"category":"meaning_changed","source_quote":"Grand Seneschal and the steward","translation_quote":"Gran Siniscalco e l'amministratore","explanation":"'Grand Seneschal' is correct, but 'steward' is wrong."}
        ]}"#;
        let outcome = interpret_response(response, &passage, 11, 12);

        assert!(outcome.cached.parsed);
        assert_eq!(outcome.cached.raw_defect_count, 2);
        assert_eq!(outcome.cached.defects.len(), 1);
        assert_eq!(
            outcome.cached.defects[0].category,
            DefectCategory::MeaningChanged
        );
        assert_eq!(outcome.cached.dropped_missing_quote, 0);
        assert_eq!(outcome.cached.dropped_non_verbatim_quote, 0);
        assert_eq!(outcome.cached.dropped_self_refuting, 1);
        assert!(!is_self_refuting_explanation(
            "The title is not correctly translated."
        ));
        assert!(!is_self_refuting_explanation(
            "The title would be correctly translated as Gran Siniscalco."
        ));
    }

    #[test]
    fn seeded_sampling_is_deterministic_and_seed_sensitive() {
        let passages = (0..20).map(passage).collect::<Vec<_>>();
        let first = sample_passages(&passages, 7, 42);
        let second = sample_passages(&passages, 7, 42);
        let different = sample_passages(&passages, 7, 43);

        assert_eq!(first, second);
        assert_ne!(first, different);
    }

    #[test]
    fn seed_is_frozen_into_jsonl_and_summary_schemas() {
        let mut record = parsed_record(10, Vec::new());
        record.seed = 123_456;
        let json = serde_json::to_value(&record).expect("serialize record");
        assert_eq!(json["schema_version"], 2);
        assert_eq!(json["seed"], 123_456);
        assert_eq!(json["source_chars"], 10);
        assert!(json.get("source_words").is_none());

        let mut args = test_args();
        args.seed = 123_456;
        let summary = build_summary(
            &args,
            &test_endpoint(),
            1,
            &RunResult {
                records: vec![record],
                ..RunResult::default()
            },
        );
        let json = serde_json::to_value(&summary).expect("serialize summary");
        assert_eq!(json["schema_version"], 2);
        assert_eq!(json["source_chars_judged"], 10);
        assert!(json.get("source_words_judged").is_none());
        assert_eq!(summary.seed, 123_456);
    }

    #[test]
    fn rates_use_source_chars_and_report_passage_n() {
        let hard = Defect {
            category: DefectCategory::MeaningChanged,
            source_quote: "a".to_string(),
            translation_quote: "b".to_string(),
            explanation: "hard".to_string(),
        };
        let soft = Defect {
            category: DefectCategory::RegisterShift,
            source_quote: "c".to_string(),
            translation_quote: "d".to_string(),
            explanation: "soft".to_string(),
        };
        let run = RunResult {
            records: vec![
                parsed_record(100, vec![hard.clone(), hard.clone()]),
                parsed_record(50, vec![hard, soft]),
            ],
            ..RunResult::default()
        };
        let summary = build_summary(&test_args(), &test_endpoint(), 10, &run);
        let meaning = summary
            .categories
            .iter()
            .find(|metric| metric.category == DefectCategory::MeaningChanged)
            .expect("meaning metric");
        let hard_group = summary
            .groups
            .iter()
            .find(|metric| metric.group == DefectGroup::Hard)
            .expect("hard group");

        assert_eq!(summary.passages_judged, 2);
        assert_eq!(summary.source_chars_judged, 150);
        assert_eq!(summary.dropped_missing_quote, 0);
        assert_eq!(summary.dropped_non_verbatim_quote, 0);
        assert_eq!(summary.dropped_self_refuting, 0);
        assert_eq!(meaning.count, 3);
        assert!((meaning.per_1k_source_chars - 20.0).abs() < f64::EPSILON);
        assert!((hard_group.per_1k_source_chars - 20.0).abs() < f64::EPSILON);
    }

    #[test]
    fn summary_reports_each_drop_counter_separately() {
        let mut record = parsed_record(10, Vec::new());
        record.dropped_missing_quote = 2;
        record.dropped_non_verbatim_quote = 3;
        record.dropped_self_refuting = 5;
        let run = RunResult {
            records: vec![record],
            ..RunResult::default()
        };

        let summary = build_summary(&test_args(), &test_endpoint(), 1, &run);

        assert_eq!(summary.dropped_missing_quote, 2);
        assert_eq!(summary.dropped_non_verbatim_quote, 3);
        assert_eq!(summary.dropped_self_refuting, 5);
        assert_eq!(summary.dropped_defects, 10);
    }

    #[test]
    fn baseline_delta_is_per_category() {
        let mut current = build_summary(&test_args(), &test_endpoint(), 1, &RunResult::default());
        current.passages_judged = 4;
        current
            .categories
            .iter_mut()
            .find(|metric| metric.category == DefectCategory::ContentDropped)
            .expect("current category")
            .clone_from(&CategoryMetric {
                category: DefectCategory::ContentDropped,
                count: 5,
                per_1k_source_chars: 2.5,
            });
        let mut baseline = current.clone();
        baseline.seed = 99;
        baseline.passages_judged = 3;
        baseline
            .categories
            .iter_mut()
            .find(|metric| metric.category == DefectCategory::ContentDropped)
            .expect("baseline category")
            .clone_from(&CategoryMetric {
                category: DefectCategory::ContentDropped,
                count: 2,
                per_1k_source_chars: 1.25,
            });

        let delta = compute_baseline_delta(&current, &baseline);
        let dropped = delta
            .categories
            .iter()
            .find(|row| row.category == DefectCategory::ContentDropped)
            .expect("delta category");
        assert_eq!(delta.baseline_seed, 99);
        assert_eq!(delta.baseline_passages_judged, 3);
        assert_eq!(dropped.count_delta, 3);
        assert!((dropped.per_1k_source_chars_delta - 1.25).abs() < f64::EPSILON);
    }

    #[test]
    fn copied_store_is_opened_and_owner_bytes_are_untouched() {
        let temp = tempfile::tempdir().expect("tempdir");
        let owner = temp.path().join("owner.sqlite");
        drop(JobStore::open(&owner).expect("create owner store"));
        let before = fs::read(&owner).expect("read owner before");

        let copied = open_copied_store(&owner).expect("open copy");
        copied
            .store
            .list_job_summaries()
            .expect("read copied store");
        drop(copied);

        let after = fs::read(&owner).expect("read owner after");
        assert_eq!(before, after);
    }

    struct MockScorer;

    impl TranslationScorer for MockScorer {
        fn name(&self) -> &'static str {
            "mock"
        }

        fn render(&self, passage: &Passage) -> RenderedPrompt {
            render_prompt(passage)
        }

        async fn score(&self, passage: &Passage) -> Result<ScoreOutcome, ScoreError> {
            Ok(interpret_response(r#"{"defects":[]}"#, passage, 10, 2))
        }
    }

    #[tokio::test]
    async fn scorer_seam_runs_entirely_offline() {
        let passages = vec![passage(1), passage(2)];
        let mut output = Vec::new();
        let run = run_scoring(
            &MockScorer,
            &test_endpoint(),
            &passages,
            &OutcomeCache { dir: None },
            17,
            0.0,
            4_096,
            &mut output,
        )
        .await
        .expect("offline run");

        assert_eq!(run.provider_calls, 2);
        assert_eq!(run.request_errors, 0);
        assert_eq!(run.records.len(), 2);
        assert_eq!(output.split(|byte| *byte == b'\n').count(), 3);
        assert!(run.records.iter().all(|record| record.seed == 17));
    }
}
