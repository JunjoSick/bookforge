//! `adjudicate_translation` — measure the precision of `judge_translation`.
//!
//! This is a read-only, dev-time second pass over an existing
//! `judge_translation` JSONL file. Each enumerated finding becomes one narrow
//! adjudication task: is this category-specific complaint supported by its
//! exact source and translation spans?
//!
//! Start with a dry run. It renders every selected prompt and estimates the
//! maximum cost without calling a provider or writing output:
//!
//! ```text
//! cargo run --release --example adjudicate_translation -- \
//!   --results judge-translation.jsonl --dry-run
//! ```
//!
//! A paid run writes frozen verdict JSONL and a summary containing a
//! true-positive rate for every category. The API key is never a CLI value:
//! `--api-key-env` accepts only the environment-variable name read by the
//! provider at request time.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use bookforge_core::{JsonMode, RetryAfterPolicy};
use bookforge_llm::{
    CompletionRequest, LlmProvider, OpenAiCompatibleConfig, OpenAiCompatibleProvider,
    RequestMetadata, ResponseFormat,
};
use clap::Parser;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Prompt changes invalidate every cached adjudication.
const PROMPT_VERSION: &str = "adjudicate_translation/v1";
const OUTPUT_SCHEMA_VERSION: u32 = 1;
const SUMMARY_SCHEMA_VERSION: u32 = 1;
const SUPPORTED_RESULTS_SCHEMA_VERSION: u32 = 1;
const MAX_JUDGE_RESPONSE_BYTES: usize = 8 * 1024;
const MAX_RATIONALE_CHARS: usize = 300;
const EMBEDDED_PRICING: &str = include_str!("../pricing/providers.json");

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
    name = "adjudicate_translation",
    about = "Measure judge_translation precision per defect category (dev-time only)"
)]
struct Args {
    /// Existing judge_translation passage-result JSONL.
    #[arg(long)]
    results: PathBuf,

    /// Frozen per-finding adjudication JSONL.
    #[arg(long, default_value = "translation-adjudication.jsonl")]
    out: PathBuf,

    /// Machine-readable precision summary. Defaults beside --out.
    #[arg(long)]
    summary: Option<PathBuf>,

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

    /// Adjudication sampling temperature.
    #[arg(long, default_value_t = 0.0)]
    temperature: f32,

    /// Hard judge-output cap per finding.
    #[arg(long, default_value_t = 1_024)]
    max_output_tokens: u32,

    /// Per-request timeout in seconds.
    #[arg(long, default_value_t = 120)]
    timeout_seconds: u64,

    /// Cap on findings in input order. 0 selects all and removes the spend cap.
    #[arg(long, default_value_t = 25)]
    limit: usize,

    /// Render prompts and estimate maximum cost without calls or writes.
    #[arg(long)]
    dry_run: bool,

    /// Directory for content-addressed adjudication results.
    #[arg(long, default_value = ".bookforge/translation-adjudication-cache")]
    cache: PathBuf,

    /// Disable the on-disk adjudication cache.
    #[arg(long)]
    no_cache: bool,

    /// Pricing catalog override. The embedded CLI catalog is used by default.
    #[arg(long)]
    pricing: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// Input and task schema
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
}

#[derive(Debug, Clone, Deserialize)]
struct InputDefect {
    category: DefectCategory,
    source_quote: String,
    translation_quote: String,
    explanation: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum InputRecordStatus {
    Parsed,
    Unparseable,
    Error,
}

/// Unknown fields are intentionally tolerated: this consumer needs only the
/// stable identity, language, status, and defect columns from passage JSONL.
#[derive(Debug, Deserialize)]
struct InputPassageRecord {
    schema_version: u32,
    passage_id: String,
    source_language: String,
    target_language: String,
    status: InputRecordStatus,
    defects: Vec<InputDefect>,
}

#[derive(Debug, Clone)]
struct AdjudicationTask {
    finding_id: String,
    passage_id: String,
    finding_index: usize,
    source_language: String,
    target_language: String,
    category: DefectCategory,
    source_quote: String,
    translation_quote: String,
    explanation: String,
}

#[derive(Debug, Default)]
struct InputStats {
    records_read: usize,
    parsed_records: usize,
    skipped_unparseable_lines: usize,
    skipped_unsupported_schema: usize,
    skipped_nonparsed_records: usize,
    dropped_self_refuting: usize,
}

fn read_tasks(path: &Path) -> Result<(Vec<AdjudicationTask>, InputStats)> {
    let file = fs::File::open(path)
        .with_context(|| format!("opening translation results {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut tasks = Vec::new();
    let mut stats = InputStats::default();

    for (line_index, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("reading {}", path.display()))?;
        if line.trim().is_empty() {
            continue;
        }
        stats.records_read += 1;
        let record = match serde_json::from_str::<InputPassageRecord>(&line) {
            Ok(record) => record,
            Err(error) => {
                stats.skipped_unparseable_lines += 1;
                warn(&format!(
                    "{}:{}: skipping unparseable result: {error}",
                    path.display(),
                    line_index + 1
                ));
                continue;
            }
        };
        if record.schema_version != SUPPORTED_RESULTS_SCHEMA_VERSION {
            stats.skipped_unsupported_schema += 1;
            warn(&format!(
                "{}:{}: skipping schema_version {}; expected {}",
                path.display(),
                line_index + 1,
                record.schema_version,
                SUPPORTED_RESULTS_SCHEMA_VERSION
            ));
            continue;
        }
        if record.status != InputRecordStatus::Parsed {
            stats.skipped_nonparsed_records += 1;
            continue;
        }
        stats.parsed_records += 1;

        for (finding_index, defect) in record.defects.into_iter().enumerate() {
            // This makes old result files safe to adjudicate. New result files
            // have already applied the same conservative deterministic filter.
            if is_self_refuting_explanation(&defect.explanation) {
                stats.dropped_self_refuting += 1;
                continue;
            }
            let finding_index_text = finding_index.to_string();
            let finding_id = format!(
                "finding_{}",
                &hash_fields([
                    record.passage_id.as_str(),
                    finding_index_text.as_str(),
                    defect.category.as_str(),
                    defect.source_quote.as_str(),
                    defect.translation_quote.as_str(),
                    defect.explanation.as_str(),
                ])[..20]
            );
            tasks.push(AdjudicationTask {
                finding_id,
                passage_id: record.passage_id.clone(),
                finding_index,
                source_language: record.source_language.clone(),
                target_language: record.target_language.clone(),
                category: defect.category,
                source_quote: defect.source_quote,
                translation_quote: defect.translation_quote,
                explanation: defect.explanation,
            });
        }
    }

    Ok((tasks, stats))
}

// Keep this rule in lockstep with judge_translation. It is duplicated because
// examples are standalone Cargo targets and the permitted edit surface does not
// include a shared library module.
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
    if !DISMISSALS
        .iter()
        .any(|phrase| contains_word_sequence(&words, phrase))
    {
        return false;
    }
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

// ---------------------------------------------------------------------------
// Frozen output schemas
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum VerdictLabel {
    TruePositive,
    FalsePositive,
    Unclear,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AdjudicationStatus {
    Parsed,
    Unparseable,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AdjudicationRecord {
    schema_version: u32,
    prompt_version: String,
    finding_id: String,
    passage_id: String,
    finding_index: usize,
    category: DefectCategory,
    source_quote: String,
    translation_quote: String,
    explanation: String,
    status: AdjudicationStatus,
    verdict: Option<VerdictLabel>,
    confidence: Option<f64>,
    rationale: Option<String>,
    cached: bool,
    input_tokens: u64,
    output_tokens: u64,
    error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CategoryPrecision {
    category: DefectCategory,
    findings: usize,
    adjudicated: usize,
    true_positive: usize,
    false_positive: usize,
    unclear: usize,
    unparseable: usize,
    request_errors: usize,
    /// `None` is serialized as JSON null: 0/0 is unknown, not 0% precision.
    true_positive_rate: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PrecisionSummary {
    schema_version: u32,
    prompt_version: String,
    source_results: String,
    provider: String,
    model: String,
    input_records: usize,
    parsed_input_records: usize,
    skipped_input_lines: usize,
    skipped_unsupported_schema: usize,
    skipped_nonparsed_records: usize,
    dropped_input_self_refuting: usize,
    findings_available: usize,
    findings_selected: usize,
    limit: usize,
    cache_hits: usize,
    provider_calls: usize,
    request_errors: usize,
    input_tokens: u64,
    output_tokens: u64,
    categories: Vec<CategoryPrecision>,
}

// ---------------------------------------------------------------------------
// Prompt and scorer seam
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct RenderedPrompt {
    system: String,
    user: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JudgeResponse {
    verdict: VerdictLabel,
    confidence: f64,
    rationale: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CachedVerdict {
    parsed: bool,
    verdict: Option<VerdictLabel>,
    confidence: Option<f64>,
    rationale: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct ScoreOutcome {
    cached: CachedVerdict,
    input_tokens: u64,
    output_tokens: u64,
}

#[derive(Debug)]
struct ScoreError(String);

trait AdjudicationScorer {
    fn name(&self) -> &'static str;

    fn render(&self, task: &AdjudicationTask) -> RenderedPrompt;

    fn score(
        &self,
        task: &AdjudicationTask,
    ) -> impl std::future::Future<Output = Result<ScoreOutcome, ScoreError>> + Send;
}

struct TextScorer {
    provider: OpenAiCompatibleProvider,
    endpoint: Endpoint,
    temperature: f32,
    max_output_tokens: u32,
}

impl AdjudicationScorer for TextScorer {
    fn name(&self) -> &'static str {
        "text"
    }

    fn render(&self, task: &AdjudicationTask) -> RenderedPrompt {
        render_prompt(task)
    }

    async fn score(&self, task: &AdjudicationTask) -> Result<ScoreOutcome, ScoreError> {
        let prompt = self.render(task);
        let response = self
            .provider
            .complete(CompletionRequest {
                system: prompt.system,
                user: prompt.user,
                response_format: ResponseFormat::Json,
                temperature: self.temperature,
                max_output_tokens: Some(self.max_output_tokens),
                metadata: RequestMetadata {
                    segment_id: Some(task.finding_id.clone()),
                    prompt_template: Some("adjudicate_translation".to_string()),
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
            response.input_tokens.unwrap_or_default(),
            response.output_tokens.unwrap_or_default(),
        ))
    }
}

const JUDGE_SYSTEM_PROMPT: &str = r#"You are measuring the precision of another translation-quality judge.

Your only job is to decide whether ONE specific, category-labelled complaint is correct about the exact source and translation spans supplied.

Use these verdicts:

true_positive - the claimed category-specific defect is demonstrated by the spans
false_positive - the spans are fine with respect to this exact complaint
unclear - the claim cannot be decided from the supplied spans

Category meanings:

meaning_changed - the translation asserts something the source does not
content_dropped - source content is absent from the translation
content_added - the translation asserts content with no source basis
terminology_inconsistent - one source term is rendered inconsistently
register_shift - formality or voice diverges without cause
target_language_error - the translation is ungrammatical or unidiomatic

Judge only the stated complaint. Do not substitute a different defect and do not reward plausible explanations unsupported by the quoted spans. If deciding the complaint requires omitted context, use unclear.

Return ONLY one JSON object, without prose or code fences:

{"verdict":"true_positive"|"false_positive"|"unclear","confidence":0.0,"rationale":"one sentence, at most 200 characters"}"#;

fn render_prompt(task: &AdjudicationTask) -> RenderedPrompt {
    RenderedPrompt {
        system: JUDGE_SYSTEM_PROMPT.to_string(),
        user: format!(
            "Source language: {}\n\
             Target language: {}\n\
             Passage: {}\n\
             Finding: {} (index {})\n\
             Claimed category: {}\n\
             Exact complaint: {}\n\
             \n\
             Exact source span:\n\
             <<<SOURCE\n\
             {}\n\
             SOURCE\n\
             \n\
             Exact translation span:\n\
             <<<TRANSLATION\n\
             {}\n\
             TRANSLATION\n\
             \n\
             Is this specific complaint correct? Return the JSON object only.\n",
            task.source_language,
            task.target_language,
            task.passage_id,
            task.finding_id,
            task.finding_index,
            task.category.as_str(),
            task.explanation,
            task.source_quote,
            task.translation_quote,
        ),
    }
}

fn interpret_response(content: &str, input_tokens: u64, output_tokens: u64) -> ScoreOutcome {
    let unparseable = |reason: &str| ScoreOutcome {
        cached: CachedVerdict {
            parsed: false,
            verdict: None,
            confidence: None,
            rationale: None,
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
        // Terminal by design: malformed output is recorded and never repaired
        // or sent to a second prompt.
        return unparseable("judge response was not a valid adjudication object");
    };
    if !(0.0..=1.0).contains(&response.confidence) {
        return unparseable("judge returned confidence outside 0.0..=1.0");
    }
    let rationale = response.rationale.trim();
    if rationale.is_empty() {
        return unparseable("judge returned an empty rationale");
    }

    ScoreOutcome {
        cached: CachedVerdict {
            parsed: true,
            verdict: Some(response.verdict),
            confidence: Some(response.confidence),
            rationale: Some(truncate_chars(rationale, MAX_RATIONALE_CHARS)),
            error: None,
        },
        input_tokens,
        output_tokens,
    }
}

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

fn truncate_chars(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

// ---------------------------------------------------------------------------
// Provider and content-addressed cache
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Endpoint {
    provider: String,
    base_url: String,
    api_key_env: String,
    model: String,
}

fn resolve_endpoint(args: &Args) -> Result<Endpoint> {
    let (default_url, default_key_env, default_model) = match args.provider.as_str() {
        "deepseek" => (
            "https://api.deepseek.com/v1",
            "DEEPSEEK_API_KEY",
            "deepseek-v4-flash",
        ),
        "openrouter" => (
            "https://openrouter.ai/api/v1",
            "OPENROUTER_API_KEY",
            "openrouter/auto",
        ),
        "openai-compatible" => {
            if args.base_url.is_none() {
                bail!("--provider openai-compatible requires --base-url");
            }
            ("", "OPENAI_API_KEY", "local-model")
        }
        other => {
            bail!("unsupported provider '{other}'; use deepseek, openrouter, or openai-compatible")
        }
    };
    Ok(Endpoint {
        provider: args.provider.clone(),
        base_url: args
            .base_url
            .clone()
            .unwrap_or_else(|| default_url.to_string()),
        api_key_env: args
            .api_key_env
            .clone()
            .unwrap_or_else(|| default_key_env.to_string()),
        model: args
            .model
            .clone()
            .unwrap_or_else(|| default_model.to_string()),
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

struct VerdictCache {
    dir: Option<PathBuf>,
}

impl VerdictCache {
    fn get(&self, key: &str) -> Option<CachedVerdict> {
        let path = self.dir.as_ref()?.join(format!("{key}.json"));
        serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
    }

    fn put(&self, key: &str, verdict: &CachedVerdict) {
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
        let body = match serde_json::to_string(verdict) {
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
    task: &AdjudicationTask,
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
        task.source_language.as_str(),
        task.target_language.as_str(),
        task.category.as_str(),
        task.source_quote.as_str(),
        task.translation_quote.as_str(),
        task.explanation.as_str(),
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

// ---------------------------------------------------------------------------
// Harness and precision reporting
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct RunResult {
    records: Vec<AdjudicationRecord>,
    cache_hits: usize,
    provider_calls: usize,
    request_errors: usize,
    input_tokens: u64,
    output_tokens: u64,
}

async fn run_adjudication<S: AdjudicationScorer>(
    scorer: &S,
    endpoint: &Endpoint,
    tasks: &[AdjudicationTask],
    cache: &VerdictCache,
    temperature: f32,
    max_output_tokens: u32,
    sink: &mut dyn Write,
) -> Result<RunResult> {
    let mut run = RunResult::default();
    for task in tasks {
        let key = cache_key(
            scorer.name(),
            endpoint,
            task,
            temperature,
            max_output_tokens,
        );
        let (outcome, cached) = if let Some(verdict) = cache.get(&key) {
            run.cache_hits += 1;
            (
                ScoreOutcome {
                    cached: verdict,
                    input_tokens: 0,
                    output_tokens: 0,
                },
                true,
            )
        } else {
            match scorer.score(task).await {
                Ok(outcome) => {
                    run.provider_calls += 1;
                    // Parsed and unparseable responses are both terminal and
                    // cached. A re-run never re-prompts malformed output.
                    cache.put(&key, &outcome.cached);
                    (outcome, false)
                }
                Err(error) => {
                    run.provider_calls += 1;
                    run.request_errors += 1;
                    warn(&format!(
                        "{} [{}]: adjudication request failed: {}",
                        task.finding_id,
                        task.category.as_str(),
                        error.0
                    ));
                    let record = make_record(
                        task,
                        AdjudicationStatus::Error,
                        None,
                        None,
                        None,
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
            AdjudicationStatus::Parsed
        } else {
            AdjudicationStatus::Unparseable
        };
        let record = make_record(
            task,
            status,
            outcome.cached.verdict,
            outcome.cached.confidence,
            outcome.cached.rationale,
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
fn make_record(
    task: &AdjudicationTask,
    status: AdjudicationStatus,
    verdict: Option<VerdictLabel>,
    confidence: Option<f64>,
    rationale: Option<String>,
    cached: bool,
    input_tokens: u64,
    output_tokens: u64,
    error: Option<String>,
) -> AdjudicationRecord {
    AdjudicationRecord {
        schema_version: OUTPUT_SCHEMA_VERSION,
        prompt_version: PROMPT_VERSION.to_string(),
        finding_id: task.finding_id.clone(),
        passage_id: task.passage_id.clone(),
        finding_index: task.finding_index,
        category: task.category,
        source_quote: task.source_quote.clone(),
        translation_quote: task.translation_quote.clone(),
        explanation: task.explanation.clone(),
        status,
        verdict,
        confidence,
        rationale,
        cached,
        input_tokens,
        output_tokens,
        error,
    }
}

fn write_record(sink: &mut dyn Write, record: &AdjudicationRecord) -> Result<()> {
    let line = serde_json::to_string(record).context("serializing adjudication record")?;
    writeln!(sink, "{line}").context("writing adjudication record")?;
    Ok(())
}

fn build_summary(
    args: &Args,
    endpoint: &Endpoint,
    input: &InputStats,
    findings_available: usize,
    tasks: &[AdjudicationTask],
    run: &RunResult,
) -> PrecisionSummary {
    let categories = ALL_CATEGORIES
        .iter()
        .map(|category| {
            let findings = tasks
                .iter()
                .filter(|task| task.category == *category)
                .count();
            let records = run
                .records
                .iter()
                .filter(|record| record.category == *category)
                .collect::<Vec<_>>();
            let adjudicated = records
                .iter()
                .filter(|record| record.status == AdjudicationStatus::Parsed)
                .count();
            let count_verdict = |label| {
                records
                    .iter()
                    .filter(|record| {
                        record.status == AdjudicationStatus::Parsed && record.verdict == Some(label)
                    })
                    .count()
            };
            let true_positive = count_verdict(VerdictLabel::TruePositive);
            CategoryPrecision {
                category: *category,
                findings,
                adjudicated,
                true_positive,
                false_positive: count_verdict(VerdictLabel::FalsePositive),
                unclear: count_verdict(VerdictLabel::Unclear),
                unparseable: records
                    .iter()
                    .filter(|record| record.status == AdjudicationStatus::Unparseable)
                    .count(),
                request_errors: records
                    .iter()
                    .filter(|record| record.status == AdjudicationStatus::Error)
                    .count(),
                true_positive_rate: (adjudicated != 0)
                    .then_some(true_positive as f64 / adjudicated as f64),
            }
        })
        .collect();

    PrecisionSummary {
        schema_version: SUMMARY_SCHEMA_VERSION,
        prompt_version: PROMPT_VERSION.to_string(),
        source_results: args.results.display().to_string(),
        provider: endpoint.provider.clone(),
        model: endpoint.model.clone(),
        input_records: input.records_read,
        parsed_input_records: input.parsed_records,
        skipped_input_lines: input.skipped_unparseable_lines,
        skipped_unsupported_schema: input.skipped_unsupported_schema,
        skipped_nonparsed_records: input.skipped_nonparsed_records,
        dropped_input_self_refuting: input.dropped_self_refuting,
        findings_available,
        findings_selected: tasks.len(),
        limit: args.limit,
        cache_hits: run.cache_hits,
        provider_calls: run.provider_calls,
        request_errors: run.request_errors,
        input_tokens: run.input_tokens,
        output_tokens: run.output_tokens,
        categories,
    }
}

fn print_human_report(summary: &PrecisionSummary) {
    println!("=== Translation finding precision ===");
    println!("source results     : {}", summary.source_results);
    println!(
        "judge              : {} / {}",
        summary.provider, summary.model
    );
    println!("prompt             : {}", summary.prompt_version);
    println!(
        "input records      : {} read, {} parsed",
        summary.input_records, summary.parsed_input_records
    );
    println!(
        "input skips        : {} malformed, {} schema, {} non-parsed",
        summary.skipped_input_lines,
        summary.skipped_unsupported_schema,
        summary.skipped_nonparsed_records
    );
    println!(
        "self-refuting      : {} dropped from input",
        summary.dropped_input_self_refuting
    );
    println!(
        "findings           : {} available, {} selected",
        summary.findings_available, summary.findings_selected
    );
    println!(
        "cache/calls/errors : {}/{}/{}",
        summary.cache_hits, summary.provider_calls, summary.request_errors
    );
    println!(
        "provider tokens    : {} input, {} output",
        summary.input_tokens, summary.output_tokens
    );
    println!(
        "\ncategory                    found judged    true+   false+  unclear unparsed errors"
    );
    println!("--------------------------------------------------------------------------------");
    for metric in &summary.categories {
        println!(
            "{:<27} {:>5} {:>6} {:>8} {:>8} {:>8} {:>8} {:>6}",
            metric.category.as_str(),
            metric.findings,
            metric.adjudicated,
            percent(metric.true_positive, metric.adjudicated),
            percent(metric.false_positive, metric.adjudicated),
            percent(metric.unclear, metric.adjudicated),
            metric.unparseable,
            metric.request_errors,
        );
    }
}

fn percent(part: usize, whole: usize) -> String {
    if whole == 0 {
        "-".to_string()
    } else {
        format!("{:.1}%", part as f64 * 100.0 / whole as f64)
    }
}

// ---------------------------------------------------------------------------
// Pricing and dry run
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct PricingFile {
    schema_version: u32,
    providers: BTreeMap<String, ProviderPricing>,
}

#[derive(Debug, Deserialize)]
struct ProviderPricing {
    models: BTreeMap<String, ModelPricing>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
struct ModelPricing {
    input_per_million_usd: f64,
    output_per_million_usd: f64,
}

struct PricingCatalog {
    file: PricingFile,
    label: String,
}

impl PricingCatalog {
    fn model_pricing(&self, provider: &str, model: &str) -> Option<ModelPricing> {
        self.file
            .providers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(provider))
            .and_then(|(_, pricing)| {
                pricing
                    .models
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(model))
                    .map(|(_, pricing)| *pricing)
            })
    }
}

fn load_pricing(path: Option<&Path>) -> Result<PricingCatalog> {
    let (body, label) = match path {
        Some(path) => (
            fs::read_to_string(path)
                .with_context(|| format!("reading pricing catalog {}", path.display()))?,
            path.display().to_string(),
        ),
        None => (EMBEDDED_PRICING.to_string(), "embedded catalog".to_string()),
    };
    let file: PricingFile =
        serde_json::from_str(&body).with_context(|| format!("parsing {label}"))?;
    if file.schema_version != 1 {
        bail!(
            "unsupported pricing schema_version {}; expected 1",
            file.schema_version
        );
    }
    Ok(PricingCatalog { file, label })
}

fn estimate_tokens(text: &str) -> u64 {
    bookforge_core::segment::estimate_tokens(text) as u64
}

fn print_dry_run(
    args: &Args,
    endpoint: &Endpoint,
    tasks: &[AdjudicationTask],
    findings_available: usize,
    input: &InputStats,
) -> Result<()> {
    let mut input_tokens = 0u64;
    for task in tasks {
        let prompt = render_prompt(task);
        input_tokens += estimate_tokens(&prompt.system) + estimate_tokens(&prompt.user);
        println!(
            "----- {} / {} [{}] -----",
            task.passage_id,
            task.finding_id,
            task.category.as_str()
        );
        println!("--- system prompt ---");
        println!("{}", prompt.system);
        println!("--- user prompt ---");
        println!("{}", prompt.user);
    }
    let output_cap = tasks.len() as u64 * u64::from(args.max_output_tokens);
    let pricing = load_pricing(args.pricing.as_deref())?;

    println!("\n=== Dry run estimate ===");
    println!("mode               : DRY RUN - no provider calls or output writes");
    println!(
        "provider/model     : {} / {}",
        endpoint.provider, endpoint.model
    );
    println!("key env name       : {}", endpoint.api_key_env);
    println!("input records      : {}", input.records_read);
    println!(
        "self-refuting      : {} dropped from input",
        input.dropped_self_refuting
    );
    println!("findings available : {findings_available}");
    println!("findings selected  : {}", tasks.len());
    println!("estimated input    : {input_tokens} tokens");
    println!("output token cap   : {output_cap} tokens");
    match pricing.model_pricing(&endpoint.provider, &endpoint.model) {
        Some(model) => {
            let maximum_cost = input_tokens as f64 / 1_000_000.0 * model.input_per_million_usd
                + output_cap as f64 / 1_000_000.0 * model.output_per_million_usd;
            println!(
                "maximum cost       : ${maximum_cost:.6} ({})",
                pricing.label
            );
        }
        None => println!(
            "maximum cost       : unavailable; no {}/{} entry in {}",
            endpoint.provider, endpoint.model, pricing.label
        ),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

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

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if let Ok(path) = fs::canonicalize(path) {
        return Ok(path);
    }
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()
            .context("resolving the current directory")?
            .join(path))
    }
}

fn warn(message: &str) {
    eprintln!("warning: {message}");
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    if !args.results.exists() {
        bail!(
            "translation result JSONL not found: {}",
            args.results.display()
        );
    }
    let endpoint = resolve_endpoint(&args)?;
    let (mut tasks, input) = read_tasks(&args.results)?;
    let findings_available = tasks.len();
    if args.limit == 0 {
        warn(&format!(
            "--limit 0 selects all {findings_available} findings and removes the spend cap"
        ));
    } else if tasks.len() > args.limit {
        tasks.truncate(args.limit);
    }

    if args.dry_run {
        return print_dry_run(&args, &endpoint, &tasks, findings_available, &input);
    }

    let summary_path = summary_path(&args);
    let input_path = absolute_path(&args.results)?;
    let output_path = absolute_path(&args.out)?;
    let machine_summary_path = absolute_path(&summary_path)?;
    if input_path == output_path || input_path == machine_summary_path {
        bail!("--out and --summary must not overwrite --results");
    }
    if output_path == machine_summary_path {
        bail!("--out and --summary must name different files");
    }

    create_parent(&args.out)?;
    create_parent(&summary_path)?;
    let mut output = BufWriter::new(
        fs::File::create(&args.out)
            .with_context(|| format!("creating adjudication JSONL {}", args.out.display()))?,
    );
    let scorer = build_text_scorer(&args, &endpoint)?;
    let cache = VerdictCache {
        dir: (!args.no_cache).then(|| args.cache.clone()),
    };
    let run = run_adjudication(
        &scorer,
        &endpoint,
        &tasks,
        &cache,
        args.temperature,
        args.max_output_tokens,
        &mut output,
    )
    .await?;
    output.flush().context("flushing adjudication JSONL")?;

    let summary = build_summary(&args, &endpoint, &input, findings_available, &tasks, &run);
    fs::write(
        &summary_path,
        serde_json::to_string_pretty(&summary).context("serializing precision summary")?,
    )
    .with_context(|| format!("writing precision summary {}", summary_path.display()))?;

    print_human_report(&summary);
    println!("\nadjudication JSONL : {}", args.out.display());
    println!("precision summary  : {}", summary_path.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// Offline tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn task(index: usize, category: DefectCategory) -> AdjudicationTask {
        AdjudicationTask {
            finding_id: format!("finding-{index}"),
            passage_id: "passage".to_string(),
            finding_index: index,
            source_language: "English".to_string(),
            target_language: "Italian".to_string(),
            category,
            source_quote: format!("source-{index}"),
            translation_quote: format!("translation-{index}"),
            explanation: format!("complaint-{index}"),
        }
    }

    fn test_args() -> Args {
        Args {
            results: PathBuf::from("results.jsonl"),
            out: PathBuf::from("out.jsonl"),
            summary: None,
            provider: "mock".to_string(),
            model: Some("mock-model".to_string()),
            base_url: Some("http://invalid".to_string()),
            api_key_env: Some("MOCK_KEY".to_string()),
            temperature: 0.0,
            max_output_tokens: 1_024,
            timeout_seconds: 1,
            limit: 25,
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

    struct StaticScorer;

    impl AdjudicationScorer for StaticScorer {
        fn name(&self) -> &'static str {
            "static"
        }

        fn render(&self, task: &AdjudicationTask) -> RenderedPrompt {
            render_prompt(task)
        }

        async fn score(&self, task: &AdjudicationTask) -> Result<ScoreOutcome, ScoreError> {
            let response = match task.finding_index {
                0 => {
                    r#"{"verdict":"true_positive","confidence":0.9,"rationale":"The complaint is demonstrated."}"#
                }
                1 => {
                    r#"{"verdict":"false_positive","confidence":0.8,"rationale":"The spans do not support it."}"#
                }
                _ => "not json",
            };
            Ok(interpret_response(response, 10, 2))
        }
    }

    #[tokio::test]
    async fn static_provider_runs_offline_and_records_unparseable_output() {
        let tasks = vec![
            task(0, DefectCategory::MeaningChanged),
            task(1, DefectCategory::MeaningChanged),
            task(2, DefectCategory::RegisterShift),
        ];
        let mut output = Vec::new();
        let run = run_adjudication(
            &StaticScorer,
            &test_endpoint(),
            &tasks,
            &VerdictCache { dir: None },
            0.0,
            1_024,
            &mut output,
        )
        .await
        .expect("offline adjudication");

        assert_eq!(run.provider_calls, 3);
        assert_eq!(run.request_errors, 0);
        assert_eq!(run.records.len(), 3);
        assert_eq!(run.records[2].status, AdjudicationStatus::Unparseable);
        assert!(run.records[2].verdict.is_none());
        assert_eq!(output.iter().filter(|byte| **byte == b'\n').count(), 3);
    }

    #[test]
    fn per_category_rates_exclude_unparseable_and_are_null_for_zero_findings() {
        let tasks = vec![
            task(0, DefectCategory::MeaningChanged),
            task(1, DefectCategory::MeaningChanged),
            task(2, DefectCategory::RegisterShift),
        ];
        let run = RunResult {
            records: vec![
                make_record(
                    &tasks[0],
                    AdjudicationStatus::Parsed,
                    Some(VerdictLabel::TruePositive),
                    Some(0.9),
                    Some("yes".to_string()),
                    false,
                    1,
                    1,
                    None,
                ),
                make_record(
                    &tasks[1],
                    AdjudicationStatus::Parsed,
                    Some(VerdictLabel::FalsePositive),
                    Some(0.9),
                    Some("no".to_string()),
                    false,
                    1,
                    1,
                    None,
                ),
                make_record(
                    &tasks[2],
                    AdjudicationStatus::Unparseable,
                    None,
                    None,
                    None,
                    false,
                    1,
                    1,
                    Some("bad json".to_string()),
                ),
            ],
            ..RunResult::default()
        };
        let summary = build_summary(
            &test_args(),
            &test_endpoint(),
            &InputStats::default(),
            tasks.len(),
            &tasks,
            &run,
        );
        let meaning = summary
            .categories
            .iter()
            .find(|metric| metric.category == DefectCategory::MeaningChanged)
            .expect("meaning_changed metric");
        let added = summary
            .categories
            .iter()
            .find(|metric| metric.category == DefectCategory::ContentAdded)
            .expect("content_added metric");
        let register = summary
            .categories
            .iter()
            .find(|metric| metric.category == DefectCategory::RegisterShift)
            .expect("register_shift metric");

        assert_eq!(meaning.findings, 2);
        assert_eq!(meaning.adjudicated, 2);
        assert_eq!(meaning.true_positive_rate, Some(0.5));
        assert_eq!(register.findings, 1);
        assert_eq!(register.adjudicated, 0);
        assert_eq!(register.unparseable, 1);
        assert_eq!(register.true_positive_rate, None);
        assert_eq!(added.findings, 0);
        assert_eq!(added.true_positive_rate, None);
    }

    #[test]
    fn old_results_drop_self_refuting_but_keep_mixed_findings() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("results.jsonl");
        let input = r#"{"schema_version":1,"passage_id":"p","source_language":"English","target_language":"Italian","status":"parsed","defects":[{"category":"target_language_error","source_quote":"Grand Seneschal","translation_quote":"Gran Siniscalco","explanation":"It is correctly translated. No error."},{"category":"meaning_changed","source_quote":"X and Y","translation_quote":"X e Z","explanation":"X is correct, but Y is wrong."}]}
"#;
        fs::write(&path, input).expect("write input");

        let (tasks, stats) = read_tasks(&path).expect("read tasks");
        assert_eq!(stats.dropped_self_refuting, 1);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].category, DefectCategory::MeaningChanged);
    }
}
