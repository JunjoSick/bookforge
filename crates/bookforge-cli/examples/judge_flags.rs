//! `judge_flags` — adjudicate BookForge validator flags with an LLM judge.
//!
//! BookForge's deterministic validators flag translated segments as
//! `needs_review`. On real books they flag hundreds of segments, and a large
//! fraction of those flags are false positives from a single heuristic
//! misfiring. Hand-adjudicating them takes hours. This example asks a judge
//! model one narrow question per flag — *is this validator complaint real?* —
//! and reports, per validator kind, what fraction of its flags are true
//! positives. That aggregate is the point: it tells the maintainer which
//! validators to fix and which to delete.
//!
//! # This is dev-time test tooling only
//!
//! BookForge's load-bearing architectural invariant is that models never repair
//! structure and never gate correctness. This example therefore:
//!
//! * must never be wired into the `translate` runtime path,
//! * never accepts, rewrites, or repairs a translation,
//! * never feeds a malformed model response to a second "repair" model —
//!   unparseable output is recorded as `unclear` and left alone.
//!
//! It reads data and writes a report. Nothing else.
//!
//! # Usage
//!
//! ```text
//! # write the built-in sample pairs file, then render prompts and price the run
//! cargo run --example judge_flags -- --write-fixture pairs.jsonl
//! cargo run --example judge_flags -- --pairs pairs.jsonl --dry-run
//!
//! # actually judge (costs money; capped at --limit units, cached on disk)
//! cargo run --example judge_flags -- --pairs pairs.jsonl --out verdicts.jsonl
//! ```
//!
//! The API key is never a CLI argument. The provider reads it itself from the
//! environment variable named by `--api-key-env` (or the provider preset's
//! default), exactly as the rest of BookForge does.

mod support;
use bookforge_core::providers::load_pricing;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use support::{Endpoint, resolve_endpoint, strip_json_code_fence, truncate_chars};

use anyhow::{Context, Result};
use bookforge_core::{JsonMode, RetryAfterPolicy};
use bookforge_llm::{
    CompletionRequest, LlmProvider, OpenAiCompatibleConfig, OpenAiCompatibleProvider,
    RequestMetadata, ResponseFormat,
};
use clap::Parser;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Bumped whenever the prompt changes. It is part of the cache key, so an
/// edited prompt invalidates every cached verdict instead of silently mixing
/// verdicts from two different questions.
const PROMPT_VERSION: &str = "judge_flags/v1";

/// A judge verdict is a handful of tokens. Anything larger than this is not a
/// verdict, so it is refused before parsing rather than after.
const MAX_JUDGE_RESPONSE_BYTES: usize = 8 * 1024;

/// Rationales are for a human skimming a report, not for storage.
const MAX_RATIONALE_CHARS: usize = 300;

/// Rough per-unit output budget used only by `--dry-run` pricing.
const ESTIMATED_OUTPUT_TOKENS_PER_UNIT: u64 = 80;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Debug, Parser)]
#[command(
    name = "judge_flags",
    about = "Adjudicate BookForge validator flags with an LLM judge (dev-time tooling only)."
)]
struct Args {
    /// Pairs JSONL: one flagged (source, translation, validator_flags) object per line.
    #[arg(long, required_unless_present = "write_fixture")]
    pairs: Option<PathBuf>,

    /// Write verdict JSONL here. Defaults to stdout (the report then goes to stderr).
    #[arg(long)]
    out: Option<PathBuf>,

    /// Provider preset: deepseek, openrouter, or openai-compatible.
    #[arg(long, default_value = "deepseek")]
    provider: String,

    /// Model override. Defaults to the provider preset's model.
    #[arg(long)]
    model: Option<String>,

    /// Base URL override. Required for `--provider openai-compatible`.
    #[arg(long)]
    base_url: Option<String>,

    /// NAME of the environment variable holding the API key. Never a key value.
    #[arg(long)]
    api_key_env: Option<String>,

    /// Sampling temperature. Adjudication wants determinism, so this defaults to 0.
    #[arg(long, default_value_t = 0.0)]
    temperature: f32,

    /// Hard cap on judge output tokens per unit.
    #[arg(long, default_value_t = 400)]
    max_output_tokens: u32,

    /// Per-request timeout.
    #[arg(long, default_value_t = 120)]
    timeout_seconds: u64,

    /// Cap on how many flags are judged. 0 removes the cap and spends real money.
    #[arg(long, default_value_t = 25)]
    limit: usize,

    /// Only judge flags of this validator kind. Repeatable.
    #[arg(long = "kind")]
    kinds: Vec<String>,

    /// Render prompts and estimate cost without calling the provider at all.
    #[arg(long)]
    dry_run: bool,

    /// Directory for the on-disk verdict cache.
    #[arg(long, default_value = ".bookforge/judge-cache")]
    cache: PathBuf,

    /// Disable the verdict cache.
    #[arg(long)]
    no_cache: bool,

    /// Pricing catalog override. Falls back to the catalog bundled with the CLI.
    #[arg(long)]
    pricing: Option<PathBuf>,

    /// Write the built-in sample pairs JSONL to this path and exit.
    #[arg(long)]
    write_fixture: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// Frozen input schema
// ---------------------------------------------------------------------------

/// One flagged segment as exported by the pairs producer. Unknown fields are
/// tolerated on purpose: the exporter may grow columns this tool ignores.
#[derive(Debug, Deserialize)]
struct JudgePair {
    job_id: String,
    segment_id: String,
    block_id: String,
    source_language: String,
    target_language: String,
    source_text: String,
    translated_text: String,
    validator_flags: Vec<ValidatorFlag>,
}

#[derive(Debug, Clone, Deserialize)]
struct ValidatorFlag {
    kind: String,
    message: String,
}

/// One unit of work: a single flag on a single segment. A segment carrying two
/// flags is judged twice, because the two complaints can have different answers.
#[derive(Debug, Clone)]
struct JudgeTask {
    job_id: String,
    segment_id: String,
    block_id: String,
    source_language: String,
    target_language: String,
    source_text: String,
    translated_text: String,
    kind: String,
    message: String,
}

// ---------------------------------------------------------------------------
// Frozen output schema
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum VerdictLabel {
    TruePositive,
    FalsePositive,
    Unclear,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VerdictRecord {
    segment_id: String,
    kind: String,
    verdict: VerdictLabel,
    confidence: f64,
    rationale: String,
}

/// The shape the judge model is required to return.
#[derive(Debug, Deserialize)]
struct JudgeResponse {
    verdict: VerdictLabel,
    confidence: f64,
    rationale: String,
}

// ---------------------------------------------------------------------------
// Scorer seam
// ---------------------------------------------------------------------------

/// A rendered judge prompt. `--dry-run` prints these instead of sending them.
#[derive(Debug, Clone)]
struct RenderedPrompt {
    system: String,
    user: String,
}

/// One scored flag.
#[derive(Debug, Clone)]
struct ScoreOutcome {
    verdict: VerdictLabel,
    confidence: f64,
    rationale: String,
    /// `false` when the response could not be parsed strictly and was forced to
    /// `Unclear`. Reported separately so a flood of unparseable output is
    /// visible rather than hiding inside the `unclear` rate.
    parsed: bool,
    input_tokens: u64,
    output_tokens: u64,
}

/// A per-item scoring failure (network, HTTP, auth). Not a verdict: it is
/// counted in the report's `errors` column and never written to the verdict
/// stream, so a flaky provider cannot skew the true/false-positive rates.
#[derive(Debug)]
struct ScoreError(String);

// SEAM: the owner intends to add a vision-backed scorer later — render a
// rebuilt EPUB page and check *visually* that footnote markers and layout
// survived. That scorer is a second `impl FlagScorer` and nothing else: the
// harness below (task expansion, `--limit`, caching, verdict JSONL, the
// aggregate report) is generic over `S: FlagScorer` and does not change.
// `render` is on the trait rather than free-standing precisely because a vision
// scorer renders something different — an image reference plus a shorter text
// prompt — and `--dry-run` must still be able to show it without calling out.
//
// The async-fn-in-trait shape deliberately mirrors `bookforge_llm::LlmProvider`.
// It is not dyn-compatible, which is intended: dispatch stays static and there
// is no `Box<dyn>` in the hot path.
trait FlagScorer {
    fn name(&self) -> &'static str;

    fn render(&self, task: &JudgeTask) -> RenderedPrompt;

    fn score(
        &self,
        task: &JudgeTask,
    ) -> impl std::future::Future<Output = Result<ScoreOutcome, ScoreError>> + Send;
}

/// The only scorer implemented today: text-in, JSON-verdict-out, over
/// BookForge's existing OpenAI-compatible provider.
struct TextScorer {
    provider: OpenAiCompatibleProvider,
    endpoint: Endpoint,
    temperature: f32,
    max_output_tokens: u32,
}

impl FlagScorer for TextScorer {
    fn name(&self) -> &'static str {
        "text"
    }

    fn render(&self, task: &JudgeTask) -> RenderedPrompt {
        RenderedPrompt {
            system: JUDGE_SYSTEM_PROMPT.to_string(),
            user: render_user_prompt(task),
        }
    }

    async fn score(&self, task: &JudgeTask) -> Result<ScoreOutcome, ScoreError> {
        let prompt = self.render(task);
        let request = CompletionRequest {
            system: prompt.system,
            user: prompt.user,
            response_format: ResponseFormat::Json,
            temperature: self.temperature,
            max_output_tokens: Some(self.max_output_tokens),
            metadata: RequestMetadata {
                segment_id: Some(task.segment_id.clone()),
                block_ids: vec![task.block_id.clone()],
                prompt_template: Some("judge_flag".to_string()),
                prompt_version: Some(PROMPT_VERSION.to_string()),
                provider: Some(self.endpoint.provider.clone()),
                model: Some(self.endpoint.model.clone()),
                ..RequestMetadata::default()
            },
        };

        let response = self
            .provider
            .complete(request)
            .await
            .map_err(|error| ScoreError(error.to_string()))?;

        Ok(interpret_response(
            &response.content,
            response.input_tokens.unwrap_or_default(),
            response.output_tokens.unwrap_or_default(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Prompt
// ---------------------------------------------------------------------------

const JUDGE_SYSTEM_PROMPT: &str = r#"You are auditing a deterministic validator, not a translation.

BookForge translates EPUBs one segment at a time and then runs mechanical,
string-level checks over each translation. When a check fires, the segment is
flagged for human review. These checks have no model of meaning, so they
produce false positives.

Your only job is to decide whether ONE specific validator complaint is correct
about ONE specific translation.

  true_positive  - the defect the validator describes is really present in the
                   translated text.
  false_positive - the translated text is fine with respect to this complaint;
                   the validator is wrong.
  unclear        - you genuinely cannot tell from what you were given.

Rules:

1. Judge ONLY the stated complaint. Do not grade translation quality, style,
   register, fluency, or accuracy. A mediocre translation that does not have the
   specific defect described is a false_positive for this complaint.

2. A protected span is content that must survive translation AS DATA: numbers,
   dates, measurements, identifiers, code, citations, URLs. Ordinary
   source-language words are not data. If the complaint demands that a
   source-language WORD appear verbatim in the target text, and the translation
   renders that word correctly in the target language, the complaint is a
   false_positive. The data part of the span - digits, punctuation, symbols -
   must still be present and unaltered for the complaint to be a false_positive.

3. Inline markers look like <m1>...</m1> or <m4/>. They carry EPUB structure:
   footnote anchors, links, emphasis. Every marker id in the source must appear
   exactly once in the translation, and no marker id may be invented. Moving a
   marker to a different position to suit target-language word order is allowed.
   Dropping, duplicating, or inventing one is not.

4. Prefer unclear over a guess. Do not invent context you were not given.

Return ONLY a JSON object, with no prose, no commentary, and no code fences:

{"verdict":"true_positive"|"false_positive"|"unclear","confidence":0.0,"rationale":"one sentence, at most 200 characters"}"#;

/// The source and translation are delimited with `<<<SOURCE` / `SOURCE` rather
/// than markdown fences so that book text containing backticks cannot break out
/// of its block.
fn render_user_prompt(task: &JudgeTask) -> String {
    let JudgeTask {
        job_id,
        segment_id,
        block_id,
        source_language,
        target_language,
        source_text,
        translated_text,
        kind,
        message,
    } = task;
    let explanation = validator_kind_explanation(kind);

    format!(
        "Source language: {source_language}\n\
         Target language: {target_language}\n\
         Segment: {segment_id} (block {block_id}, job {job_id})\n\
         \n\
         Validator kind: {kind}\n\
         What this validator checks: {explanation}\n\
         Exact complaint: {message}\n\
         \n\
         Source text:\n\
         <<<SOURCE\n\
         {source_text}\n\
         SOURCE\n\
         \n\
         Translated text:\n\
         <<<TRANSLATION\n\
         {translated_text}\n\
         TRANSLATION\n\
         \n\
         Is this complaint correct about this translation? \
         Answer with the JSON object only.\n"
    )
}

/// Plain-language description of what each validator actually asserts. These
/// track the real implementations in `bookforge-llm` (`batch/rendering.rs`
/// `batch_item_validation_error`, and `validation.rs`), and they matter: the
/// judge cannot tell a misfire from a defect without knowing what the check was
/// trying to do.
fn validator_kind_explanation(kind: &str) -> &'static str {
    match kind {
        "protected_span_missing" => {
            "the listed span must appear in the translation. It exists to protect data that must \
             survive translation unchanged - numbers, dates, citations, identifiers - and it \
             matches literally, so it misfires when the span also contains ordinary \
             source-language words that were correctly translated."
        }
        "inline_marker_missing" => {
            "every inline marker id present in the source must also appear in the translation; \
             this one did not appear at all."
        }
        "inline_marker_duplicated" => {
            "each inline marker id may appear at most once in the translation; this one appears \
             more than once."
        }
        "unknown_inline_marker" => {
            "the translation contains an inline marker id that does not exist in the source, so \
             the model invented structure."
        }
        "marker_structure" => {
            "paired inline markers must be balanced and properly nested; this translation has an \
             unclosed, mis-nested, or wrongly closed marker tag."
        }
        "source_copy" => {
            "the translation still reproduces the source-language prose - either identical to the \
             source, or overlapping it by a very high fraction of words - which usually means the \
             model echoed the input instead of translating it."
        }
        "target_language" => {
            "the translation violates a hard constraint of the configured target language, such as \
             source-language leakage or invented vocabulary in a constrained language like Toki \
             Pona."
        }
        _ => {
            "this validator kind is not known to the judging harness. Reason from the complaint \
             text itself and prefer 'unclear' if the complaint is not self-explanatory."
        }
    }
}

// ---------------------------------------------------------------------------
// Response handling
// ---------------------------------------------------------------------------

fn interpret_response(content: &str, input_tokens: u64, output_tokens: u64) -> ScoreOutcome {
    let unusable = |reason: &str| ScoreOutcome {
        verdict: VerdictLabel::Unclear,
        confidence: 0.0,
        rationale: reason.to_string(),
        parsed: false,
        input_tokens,
        output_tokens,
    };

    if content.len() > MAX_JUDGE_RESPONSE_BYTES {
        return unusable("judge response exceeded the response-size bound");
    }

    let body = strip_json_code_fence(content.trim());
    let Ok(parsed) = serde_json::from_str::<JudgeResponse>(body) else {
        // Deliberately terminal. Sending malformed output to a second "repair"
        // model is the pattern this project rejects, so an unparseable verdict
        // stays unparseable and is reported as such.
        return unusable("judge response was not a valid verdict object");
    };

    if !(0.0..=1.0).contains(&parsed.confidence) {
        return unusable("judge returned a confidence outside 0.0..=1.0");
    }

    ScoreOutcome {
        verdict: parsed.verdict,
        confidence: parsed.confidence,
        rationale: truncate_chars(&parsed.rationale, MAX_RATIONALE_CHARS),
        parsed: true,
        input_tokens,
        output_tokens,
    }
}

// ---------------------------------------------------------------------------
// Provider configuration
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Verdict cache
// ---------------------------------------------------------------------------

/// Content-addressed verdict cache. Re-running the same pairs against the same
/// model costs nothing. Cache problems are warnings, never failures: a miss is
/// always a legal answer.
struct VerdictCache {
    dir: Option<PathBuf>,
}

impl VerdictCache {
    fn get(&self, key: &str) -> Option<VerdictRecord> {
        let path = self.dir.as_ref()?.join(format!("{key}.json"));
        let body = fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    fn put(&self, key: &str, record: &VerdictRecord) {
        let Some(dir) = self.dir.as_ref() else {
            return;
        };
        if let Err(error) = fs::create_dir_all(dir) {
            eprintln!(
                "warning: cannot create cache directory {}: {error}",
                dir.display()
            );
            return;
        }
        let path = dir.join(format!("{key}.json"));
        let body = match serde_json::to_string(record) {
            Ok(body) => body,
            Err(error) => {
                eprintln!("warning: cannot serialize cache entry: {error}");
                return;
            }
        };
        if let Err(error) = fs::write(&path, body) {
            eprintln!(
                "warning: cannot write cache entry {}: {error}",
                path.display()
            );
        }
    }
}

fn cache_key(scorer: &str, endpoint: &Endpoint, task: &JudgeTask) -> String {
    const SEPARATOR: &[u8] = b"\x1f";

    let mut hasher = Sha256::new();
    for field in [
        PROMPT_VERSION,
        scorer,
        endpoint.provider.as_str(),
        endpoint.model.as_str(),
        task.source_language.as_str(),
        task.target_language.as_str(),
        task.kind.as_str(),
        task.message.as_str(),
        task.source_text.as_str(),
        task.translated_text.as_str(),
    ] {
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
// Pricing (dry-run estimate only)
// ---------------------------------------------------------------------------

// Pricing routes through the shared core catalog; the judge tools keep no
// local copy of the schema or embedded JSON.

/// The canonical script-aware estimator shared by every BookForge
/// subsystem for pre-send token estimates. It is an estimate, and the
/// report says so.
fn estimate_tokens(text: &str) -> u64 {
    bookforge_core::segment::estimate_tokens(text) as u64
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, Copy)]
struct KindStats {
    judged: usize,
    true_positive: usize,
    false_positive: usize,
    unclear: usize,
    unparsed: usize,
    errors: usize,
}

impl KindStats {
    fn record(&mut self, verdict: VerdictLabel, parsed: bool) {
        self.judged += 1;
        if !parsed {
            self.unparsed += 1;
        }
        match verdict {
            VerdictLabel::TruePositive => self.true_positive += 1,
            VerdictLabel::FalsePositive => self.false_positive += 1,
            VerdictLabel::Unclear => self.unclear += 1,
        }
    }

    fn merge(&mut self, other: &KindStats) {
        self.judged += other.judged;
        self.true_positive += other.true_positive;
        self.false_positive += other.false_positive;
        self.unclear += other.unclear;
        self.unparsed += other.unparsed;
        self.errors += other.errors;
    }

    fn false_positive_rate(&self) -> f64 {
        if self.judged == 0 {
            0.0
        } else {
            self.false_positive as f64 / self.judged as f64
        }
    }
}

/// Human-readable output goes to stdout normally, but must move to stderr when
/// the verdict JSONL is being streamed to stdout, so the two stay separable.
struct Human {
    to_stderr: bool,
}

impl Human {
    fn say(&self, line: impl AsRef<str>) {
        if self.to_stderr {
            eprintln!("{}", line.as_ref());
        } else {
            println!("{}", line.as_ref());
        }
    }
}

fn percent(part: usize, whole: usize) -> String {
    if whole == 0 {
        "-".to_string()
    } else {
        format!("{:.1}%", part as f64 * 100.0 / whole as f64)
    }
}

fn print_aggregate(human: &Human, stats: &BTreeMap<String, KindStats>) {
    let mut rows = stats.iter().collect::<Vec<_>>();
    rows.sort_by(|(left_kind, left), (right_kind, right)| {
        right
            .false_positive_rate()
            .total_cmp(&left.false_positive_rate())
            .then_with(|| left_kind.cmp(right_kind))
    });

    let width = rows
        .iter()
        .map(|(kind, _)| kind.len())
        .chain(["kind".len(), "ALL".len()])
        .max()
        .unwrap_or(4);

    human.say(format!(
        "{:<width$} {:>7} {:>8} {:>8} {:>8} {:>7}",
        "kind", "judged", "true+", "false+", "unclear", "errors"
    ));
    let rule = "-".repeat(width + 42);
    human.say(&rule);

    let mut total = KindStats::default();
    for (kind, entry) in &rows {
        total.merge(entry);
        human.say(format!(
            "{:<width$} {:>7} {:>8} {:>8} {:>8} {:>7}",
            kind,
            entry.judged,
            percent(entry.true_positive, entry.judged),
            percent(entry.false_positive, entry.judged),
            percent(entry.unclear, entry.judged),
            entry.errors,
        ));
    }

    human.say(&rule);
    human.say(format!(
        "{:<width$} {:>7} {:>8} {:>8} {:>8} {:>7}",
        "ALL",
        total.judged,
        percent(total.true_positive, total.judged),
        percent(total.false_positive, total.judged),
        percent(total.unclear, total.judged),
        total.errors,
    ));

    if total.unparsed > 0 {
        human.say(format!(
            "\n{} verdict(s) were unparseable and counted as unclear.",
            total.unparsed
        ));
    }

    if let Some((kind, entry)) =
        rows.iter()
            .filter(|(_, entry)| entry.judged > 0)
            .max_by(|(_, left), (_, right)| {
                left.false_positive_rate()
                    .total_cmp(&right.false_positive_rate())
            })
        && entry.false_positive > 0
    {
        human.say(format!(
            "\nHighest false-positive rate: {kind} ({} of {} flags)",
            percent(entry.false_positive, entry.judged),
            entry.judged
        ));
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct RunSummary {
    stats: BTreeMap<String, KindStats>,
    cached: usize,
    called: usize,
    errors: usize,
    /// Provider-reported usage for the calls that actually happened.
    input_tokens: u64,
    output_tokens: u64,
    /// `--dry-run` only: what the run *would* have cost.
    estimated_input_tokens: u64,
    estimated_output_tokens: u64,
}

/// Judged sequentially on purpose. `--limit` keeps runs small, and one request
/// at a time makes provider rate-limit behaviour predictable and the log
/// readable; there is nothing here worth a concurrency budget.
async fn run_judgement<S: FlagScorer>(
    scorer: &S,
    endpoint: &Endpoint,
    tasks: &[JudgeTask],
    cache: &VerdictCache,
    dry_run: bool,
    human: &Human,
    verdicts: &mut dyn Write,
) -> Result<RunSummary> {
    let mut summary = RunSummary::default();

    for task in tasks {
        if dry_run {
            let prompt = scorer.render(task);
            summary.estimated_input_tokens +=
                estimate_tokens(&prompt.system) + estimate_tokens(&prompt.user);
            summary.estimated_output_tokens += ESTIMATED_OUTPUT_TOKENS_PER_UNIT;
            human.say(format!(
                "----- {} / {} [{}] -----",
                task.segment_id, task.block_id, task.kind
            ));
            human.say("--- system prompt ---");
            human.say(&prompt.system);
            human.say("--- user prompt ---");
            human.say(&prompt.user);
            continue;
        }

        let key = cache_key(scorer.name(), endpoint, task);
        if let Some(record) = cache.get(&key) {
            summary.cached += 1;
            summary
                .stats
                .entry(task.kind.clone())
                .or_default()
                .record(record.verdict, true);
            write_verdict(verdicts, &record)?;
            continue;
        }

        match scorer.score(task).await {
            Ok(outcome) => {
                summary.called += 1;
                summary.input_tokens += outcome.input_tokens;
                summary.output_tokens += outcome.output_tokens;
                let record = VerdictRecord {
                    segment_id: task.segment_id.clone(),
                    kind: task.kind.clone(),
                    verdict: outcome.verdict,
                    confidence: outcome.confidence,
                    rationale: outcome.rationale,
                };
                summary
                    .stats
                    .entry(task.kind.clone())
                    .or_default()
                    .record(record.verdict, outcome.parsed);
                write_verdict(verdicts, &record)?;
                cache.put(&key, &record);
            }
            Err(error) => {
                // Per-item failure: counted, reported, and skipped. One bad
                // request must not abort a paid run, and an error is not a
                // verdict, so it never reaches the verdict stream.
                summary.errors += 1;
                summary.stats.entry(task.kind.clone()).or_default().errors += 1;
                eprintln!(
                    "warning: {} [{}]: judge call failed: {}",
                    task.segment_id, task.kind, error.0
                );
            }
        }
    }

    Ok(summary)
}

fn write_verdict(sink: &mut dyn Write, record: &VerdictRecord) -> Result<()> {
    let line = serde_json::to_string(record).context("serializing verdict")?;
    writeln!(sink, "{line}").context("writing verdict")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// A tiny hand-written pairs file so the harness is runnable and reviewable
/// before the real exporter lands. It is embedded rather than shipped as a
/// separate data file, and written out on demand with `--write-fixture`.
///
/// Line 1 is the known false positive that motivated this tool: the validator
/// demanded the English word `vaccine` survive into Italian. Line 2 is a real
/// defect of the same kind, so the two are not distinguishable by kind alone —
/// which is exactly the discrimination the judge has to make.
const FIXTURE_JSONL: &str = r#"{"job_id":"job-demo","segment_id":"seg-0142","block_id":"b17","source_language":"English","target_language":"Italian","source_text":"Parents were advised to delay the MMR vaccine.*2","translated_text":"Ai genitori fu consigliato di rimandare il vaccino.*2","validator_flags":[{"kind":"protected_span_missing","message":"protected span missing: vaccine.*2"}]}
{"job_id":"job-demo","segment_id":"seg-0311","block_id":"b42","source_language":"English","target_language":"Italian","source_text":"The trial enrolled 1,247 children between 1998 and 2001.","translated_text":"Lo studio ha arruolato dei bambini tra il 1998 e il 2001.","validator_flags":[{"kind":"protected_span_missing","message":"protected span missing: 1,247"}]}
{"job_id":"job-demo","segment_id":"seg-0523","block_id":"b8","source_language":"English","target_language":"Italian","source_text":"He cited the Lancet paper<m4/> throughout his testimony.","translated_text":"Ha citato lo studio del Lancet in tutta la sua testimonianza.","validator_flags":[{"kind":"inline_marker_missing","message":"inline marker missing: m4"}]}
{"job_id":"job-demo","segment_id":"seg-0704","block_id":"b3","source_language":"English","target_language":"Italian","source_text":"See Appendix B for the full list of excluded studies and the reason for each exclusion.","translated_text":"See Appendix B for the full list of excluded studies and the reason for each exclusion.","validator_flags":[{"kind":"source_copy","message":"translation is unchanged from the source-language prose"}]}
"#;

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    if let Some(path) = args.write_fixture.as_deref() {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        fs::write(path, FIXTURE_JSONL)
            .with_context(|| format!("writing fixture to {}", path.display()))?;
        println!(
            "wrote {} fixture pairs to {}",
            FIXTURE_JSONL
                .lines()
                .filter(|l| !l.trim().is_empty())
                .count(),
            path.display()
        );
        return Ok(());
    }

    let pairs_path = args
        .pairs
        .clone()
        .context("--pairs is required unless --write-fixture is used")?;
    let endpoint = resolve_endpoint(
        &args.provider,
        &args.base_url,
        &args.api_key_env,
        &args.model,
    )?;

    // Verdicts stream to stdout when no --out is given, so the human-readable
    // report has to move aside. A dry run writes no verdicts at all, so it can
    // keep stdout.
    let human = Human {
        to_stderr: args.out.is_none() && !args.dry_run,
    };

    let (pairs, skipped) = read_pairs(&pairs_path)?;
    let mut tasks = expand_tasks(&pairs, &args.kinds);
    let flags_found = tasks.len();

    if args.limit == 0 {
        eprintln!("warning: --limit 0 removes the spend cap; {flags_found} flag(s) will be judged");
    } else if tasks.len() > args.limit {
        tasks.truncate(args.limit);
    }

    let cache = VerdictCache {
        dir: if args.no_cache {
            None
        } else {
            Some(args.cache.clone())
        },
    };

    let mut verdict_sink: Box<dyn Write> = match args.out.as_deref() {
        Some(path) => {
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
            {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            Box::new(std::io::BufWriter::new(
                fs::File::create(path).with_context(|| format!("creating {}", path.display()))?,
            ))
        }
        None => Box::new(std::io::stdout().lock()),
    };

    let scorer = build_text_scorer(&args, &endpoint)?;
    let summary = run_judgement(
        &scorer,
        &endpoint,
        &tasks,
        &cache,
        args.dry_run,
        &human,
        verdict_sink.as_mut(),
    )
    .await?;
    verdict_sink.flush().context("flushing verdict output")?;

    human.say("");
    human.say("=== Validator flag adjudication ===");
    human.say(format!(
        "scorer   : {} ({PROMPT_VERSION})",
        FlagScorer::name(&scorer)
    ));
    human.say(format!("provider : {}", endpoint.provider));
    human.say(format!("model    : {}", endpoint.model));
    human.say(format!(
        "key env  : {} (value never read here)",
        endpoint.api_key_env
    ));
    human.say(format!(
        "pairs read: {}    flags found: {}    units judged: {}",
        pairs.len(),
        flags_found,
        tasks.len()
    ));
    if skipped > 0 {
        human.say(format!("skipped {skipped} unparsable input line(s)"));
    }
    human.say("");

    if args.dry_run {
        print_dry_run_estimate(&human, &args, &endpoint, &tasks, &summary)?;
    } else {
        human.say(format!(
            "cached {}    called {}    errors {}",
            summary.cached, summary.called, summary.errors
        ));
        human.say("");
        print_aggregate(&human, &summary.stats);
    }

    Ok(())
}

fn build_text_scorer(args: &Args, endpoint: &Endpoint) -> Result<TextScorer> {
    let config = OpenAiCompatibleConfig {
        base_url: endpoint.base_url.clone(),
        api_key_env: endpoint.api_key_env.clone(),
        model: endpoint.model.clone(),
        timeout_seconds: args.timeout_seconds,
        // Adjudication is a side quest: fail the item fast rather than paying
        // for long retry storms.
        provider_max_attempts: 3,
        thinking_disabled: true,
        retry_after_policy: RetryAfterPolicy::JitteredExponential,
        max_backoff_seconds: 30,
        max_idle_per_host: 4,
        json_mode: JsonMode::Auto,
    };
    let provider = OpenAiCompatibleProvider::new(config)
        .map_err(|error| anyhow::anyhow!("building provider: {error}"))?;

    Ok(TextScorer {
        provider,
        endpoint: endpoint.clone(),
        temperature: args.temperature,
        max_output_tokens: args.max_output_tokens,
    })
}

fn print_dry_run_estimate(
    human: &Human,
    args: &Args,
    endpoint: &Endpoint,
    tasks: &[JudgeTask],
    summary: &RunSummary,
) -> Result<()> {
    let pricing = load_pricing(args.pricing.as_deref())?;
    let label = pricing.source_label();

    human.say("=== Dry run estimate ===");
    human.say("mode       : DRY RUN - no provider calls were made");
    human.say(format!("units      : {}", tasks.len()));
    human.say(format!(
        "est. input : {} tokens",
        summary.estimated_input_tokens
    ));
    human.say(format!(
        "est. output: {} tokens",
        summary.estimated_output_tokens
    ));

    match pricing.token_prices(&endpoint.provider, &endpoint.model) {
        Some(prices) => {
            let cost = summary.estimated_input_tokens as f64 / 1_000_000.0
                * prices.input_per_million
                + summary.estimated_output_tokens as f64 / 1_000_000.0 * prices.output_per_million;
            human.say(format!(
                "est. cost  : ${cost:.6} ({} / {}, {})",
                endpoint.provider, endpoint.model, label
            ));
        }
        None => human.say(format!(
            "est. cost  : no pricing entry for {} / {} in {}; cannot estimate cost",
            endpoint.provider, endpoint.model, label
        )),
    }

    Ok(())
}

fn read_pairs(path: &Path) -> Result<(Vec<JudgePair>, usize)> {
    let file =
        fs::File::open(path).with_context(|| format!("opening pairs file {}", path.display()))?;
    let reader = BufReader::new(file);

    let mut pairs = Vec::new();
    let mut skipped = 0usize;
    for (index, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("reading {}", path.display()))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<JudgePair>(trimmed) {
            Ok(pair) => pairs.push(pair),
            Err(error) => {
                skipped += 1;
                eprintln!(
                    "warning: {}:{}: skipping unparsable pair: {error}",
                    path.display(),
                    index + 1
                );
            }
        }
    }

    Ok((pairs, skipped))
}

fn expand_tasks(pairs: &[JudgePair], kinds: &[String]) -> Vec<JudgeTask> {
    pairs
        .iter()
        .flat_map(|pair| {
            pair.validator_flags
                .iter()
                .filter(|flag| kinds.is_empty() || kinds.iter().any(|kind| kind == &flag.kind))
                .map(|flag| JudgeTask {
                    job_id: pair.job_id.clone(),
                    segment_id: pair.segment_id.clone(),
                    block_id: pair.block_id.clone(),
                    source_language: pair.source_language.clone(),
                    target_language: pair.target_language.clone(),
                    source_text: pair.source_text.clone(),
                    translated_text: pair.translated_text.clone(),
                    kind: flag.kind.clone(),
                    message: flag.message.clone(),
                })
        })
        .collect()
}
