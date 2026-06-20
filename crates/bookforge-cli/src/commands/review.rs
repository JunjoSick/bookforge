use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use bookforge_core::{
    GlossaryTerm, RunConfigSnapshot, segment::build_segments, target_matches, term_matches,
};
use bookforge_epub::read_epub;
use bookforge_store::{JobRecord, JobStore};
use clap::Args;
use serde::Serialize;
#[cfg(test)]
use serde_json::json;

#[derive(Debug, Args)]
pub struct ReviewArgs {
    pub job_id: String,

    #[arg(long)]
    pub open: bool,

    #[arg(long)]
    pub out: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
struct ReviewDocument {
    schema_version: u32,
    job_id: String,
    source_language: Option<String>,
    target_language: String,
    provider: String,
    model: String,
    generated_at: String,
    source_book_title: Option<String>,
    source_book_author: Option<String>,
    source_input_snapshot: Option<String>,
    totals: ReviewTotals,
    segments: Vec<ReviewSegment>,
}

#[derive(Debug, Serialize)]
struct ReviewTotals {
    segments: usize,
    tokens_input: u64,
    tokens_input_cached: u64,
    tokens_output: u64,
    estimated_cost_usd: Option<f64>,
}

#[derive(Debug, Serialize)]
struct ReviewSegment {
    segment_id: String,
    chapter_id: String,
    chapter_title: Option<String>,
    ordinal: usize,
    source_text: String,
    target_text: String,
    soft_warnings: Vec<ReviewWarning>,
    tokens: ReviewTokens,
    status: String,
}

#[derive(Debug, Serialize)]
struct ReviewWarning {
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    term_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    found_target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    threshold: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

#[derive(Debug, Serialize)]
struct ReviewTokens {
    input: u64,
    input_cached: u64,
    output: u64,
    estimated: bool,
}

pub async fn run(args: ReviewArgs) -> Result<()> {
    let store = JobStore::open_default()?;
    let Some(job) = store.get_job(&args.job_id)? else {
        anyhow::bail!("job '{}' was not found", args.job_id);
    };
    let Some(snapshot) = store.load_job_config_snapshot(&args.job_id)? else {
        anyhow::bail!(
            "job '{}' does not have a run configuration snapshot; review cannot be regenerated",
            args.job_id
        );
    };

    let input = resolve_review_input(&job, &snapshot)?;
    let book = read_epub(&input)?;
    let settings = snapshot.settings.to_settings();
    let segments = build_segments(&book, &settings.segmentation)?;
    let summary = store
        .summary(&job.id)?
        .ok_or_else(|| anyhow::anyhow!("job '{}' summary unavailable", job.id))?;
    let records = store.segment_records(&job.id)?;
    let translations = store.load_terminal_segment_translations(&job.id)?;
    let glossary_terms = if snapshot.glossary_fingerprint.is_empty() {
        store.load_active_glossary_terms(
            job.source_lang.as_deref().unwrap_or("auto"),
            &job.target_lang,
            job.book_id.as_deref(),
            job.series_id.as_deref(),
        )?
    } else {
        snapshot.glossary_terms.clone()
    };

    let document = build_review_document(
        &job,
        &snapshot,
        &book,
        &segments,
        &records,
        &translations,
        &summary,
        &glossary_terms,
    );

    let out_dir = args.out.unwrap_or_else(|| {
        PathBuf::from(".bookforge/runs")
            .join(&job.id)
            .join("review")
    });
    fs::create_dir_all(&out_dir)?;
    let json_path = out_dir.join("review.json");
    let html_path = out_dir.join("index.html");
    let css_path = out_dir.join("style.css");
    let json_text = serde_json::to_string_pretty(&document)?;
    fs::write(&json_path, &json_text)?;
    fs::write(&css_path, REVIEW_CSS)?;
    fs::write(&html_path, render_html(&json_text))?;

    println!("Review JSON: {}", json_path.display());
    println!("Review HTML: {}", html_path.display());

    if args.open {
        open_in_browser(&html_path)?;
    }

    Ok(())
}

fn resolve_review_input(job: &JobRecord, snapshot: &RunConfigSnapshot) -> Result<PathBuf> {
    if let Some(path) = snapshot
        .input_snapshot_path
        .as_ref()
        .or(job.input_snapshot_path.as_ref())
        && path.exists()
    {
        return Ok(path.clone());
    }

    if snapshot.input_snapshot_path.is_none() && job.input_snapshot_path.is_none() {
        tracing::warn!(
            "job '{}' predates input EPUB snapshots; falling back to original input path",
            job.id
        );
        if snapshot.input_path.exists() {
            return Ok(snapshot.input_path.clone());
        }
        anyhow::bail!(
            "job '{}' does not have an input snapshot and the original input path no longer exists: {}",
            job.id,
            snapshot.input_path.display()
        );
    }

    let snapshot_path = snapshot
        .input_snapshot_path
        .as_ref()
        .or(job.input_snapshot_path.as_ref())
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "<missing>".to_string());
    anyhow::bail!(
        "job '{}' input snapshot is missing: {}",
        job.id,
        snapshot_path
    )
}

fn build_review_document(
    job: &JobRecord,
    snapshot: &RunConfigSnapshot,
    book: &bookforge_core::ir::Book,
    segments: &[bookforge_core::segment::Segment],
    records: &[bookforge_store::SegmentRecord],
    translations: &[bookforge_store::StoredSegmentTranslation],
    summary: &bookforge_store::JobSummary,
    glossary_terms: &[GlossaryTerm],
) -> ReviewDocument {
    let records_by_id = records
        .iter()
        .map(|record| (record.id.as_str(), record))
        .collect::<HashMap<_, _>>();
    let translations_by_id = translations
        .iter()
        .map(|translation| (translation.segment_id.as_str(), translation))
        .collect::<HashMap<_, _>>();
    let chapter_titles = book
        .sections
        .iter()
        .map(|section| (section.id.0.as_str(), section.title.clone()))
        .collect::<HashMap<_, _>>();

    let review_segments = segments
        .iter()
        .filter_map(|segment| {
            let record = records_by_id.get(segment.id.0.as_str())?;
            let target_text = translations_by_id
                .get(segment.id.0.as_str())
                .map(|translation| translation.translated_text.clone())
                .unwrap_or_default();
            Some(ReviewSegment {
                segment_id: segment.id.0.clone(),
                chapter_id: segment.section_id.0.clone(),
                chapter_title: chapter_titles
                    .get(segment.section_id.0.as_str())
                    .cloned()
                    .flatten(),
                ordinal: segment.ordinal + 1,
                source_text: segment.source.text.clone(),
                soft_warnings: soft_warnings(
                    record,
                    &segment.source.text,
                    &target_text,
                    glossary_terms,
                ),
                target_text,
                tokens: ReviewTokens {
                    input: record.input_tokens.unwrap_or(0),
                    input_cached: record.input_cached_tokens.unwrap_or(0),
                    output: record.output_tokens.unwrap_or(0),
                    estimated: record.tokens_estimated,
                },
                status: record.status.clone(),
            })
        })
        .collect::<Vec<_>>();

    ReviewDocument {
        schema_version: 1,
        job_id: job.id.clone(),
        source_language: job.source_lang.clone(),
        target_language: job.target_lang.clone(),
        provider: job.provider.clone(),
        model: job.model.clone(),
        generated_at: iso_timestamp(SystemTime::now()),
        source_book_title: book.metadata.title.clone(),
        source_book_author: book.metadata.creators.first().cloned(),
        source_input_snapshot: snapshot
            .input_snapshot_path
            .as_ref()
            .or(job.input_snapshot_path.as_ref())
            .map(|path| path.display().to_string()),
        totals: ReviewTotals {
            segments: review_segments.len(),
            tokens_input: summary.input_tokens,
            tokens_input_cached: summary.input_cached_tokens,
            tokens_output: summary.output_tokens,
            estimated_cost_usd: crate::cost::estimate_cost_usd_with_cached(
                &job.provider,
                &job.model,
                summary.input_tokens,
                summary.input_cached_tokens,
                summary.output_tokens,
            ),
        },
        segments: review_segments,
    }
}

fn soft_warnings(
    record: &bookforge_store::SegmentRecord,
    source: &str,
    target: &str,
    glossary_terms: &[GlossaryTerm],
) -> Vec<ReviewWarning> {
    let mut warnings = Vec::new();
    match record.status.as_str() {
        "failed" => warnings.push(warning_message(
            "failed_segment",
            record
                .error
                .as_deref()
                .unwrap_or("segment failed without a stored error"),
        )),
        "needs_review" => warnings.push(warning_message(
            "needs_review",
            record.error.as_deref().unwrap_or("segment requires review"),
        )),
        "retry_pending" => warnings.push(warning_message(
            "retry_pending",
            "segment is still pending retry",
        )),
        _ => {}
    }

    let source_len = source.chars().count().max(1);
    let target_len = target.chars().count();
    if source_len >= 40 {
        let ratio = target_len as f64 / source_len as f64;
        if !(0.33..=3.0).contains(&ratio) {
            warnings.push(ReviewWarning {
                kind: "length_ratio".to_string(),
                term_id: None,
                source: None,
                expected_target: None,
                found_target: None,
                value: Some((ratio * 100.0).round() / 100.0),
                threshold: Some(3.0),
                from: None,
                to: None,
                message: Some(format!(
                    "{source_len} source chars, {target_len} target chars"
                )),
            });
        }
    }

    for missing in missing_tokens("url_changed", &urls(source), &urls(target)) {
        warnings.push(missing);
    }
    for missing in missing_tokens("number_changed", &numbers(source), &numbers(target)) {
        warnings.push(missing);
    }
    if source_len >= 40 && source.trim() == target.trim() {
        warnings.push(warning_message(
            "untranslated",
            "translation matches source",
        ));
    }
    if looks_like_model_commentary(target) {
        warnings.push(warning_message(
            "model_commentary",
            "translation appears to include model commentary",
        ));
    }
    warnings.extend(glossary_warnings(source, target, glossary_terms));
    warnings
}

fn glossary_warnings(
    source: &str,
    target: &str,
    glossary_terms: &[GlossaryTerm],
) -> Vec<ReviewWarning> {
    glossary_terms
        .iter()
        .filter(|term| term_matches(source, term))
        .filter(|term| !target_matches(target, term))
        .map(|term| ReviewWarning {
            kind: "glossary_mismatch".to_string(),
            term_id: term.id,
            source: Some(term.source_text.clone()),
            expected_target: Some(term.target_text.clone()),
            found_target: None,
            value: None,
            threshold: None,
            from: None,
            to: None,
            message: Some(format!(
                "expected glossary target '{}' for source '{}'",
                term.target_text, term.source_text
            )),
        })
        .collect()
}

fn warning_message(kind: &str, message: &str) -> ReviewWarning {
    ReviewWarning {
        kind: kind.to_string(),
        term_id: None,
        source: None,
        expected_target: None,
        found_target: None,
        value: None,
        threshold: None,
        from: None,
        to: None,
        message: Some(message.to_string()),
    }
}

fn missing_tokens(kind: &str, source: &[String], target: &[String]) -> Vec<ReviewWarning> {
    source
        .iter()
        .filter(|token| !target.contains(token))
        .map(|token| ReviewWarning {
            kind: kind.to_string(),
            term_id: None,
            source: None,
            expected_target: None,
            found_target: None,
            value: None,
            threshold: None,
            from: Some(token.clone()),
            to: None,
            message: Some(format!("preserved token missing: {token}")),
        })
        .collect()
}

fn urls(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter_map(|token| {
            let value = token.trim_matches(|ch: char| {
                matches!(
                    ch,
                    ',' | ';' | ':' | '.' | '!' | '?' | ')' | ']' | '"' | '\''
                )
            });
            (value.starts_with("http://") || value.starts_with("https://"))
                .then(|| value.to_string())
        })
        .collect()
}

fn numbers(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter_map(|token| {
            let value = token.trim_matches(|ch: char| {
                matches!(
                    ch,
                    ',' | ';' | ':' | '.' | '!' | '?' | '(' | ')' | '[' | ']' | '"' | '\''
                )
            });
            let digits = value.chars().filter(|ch| ch.is_ascii_digit()).count();
            (digits >= 2
                && value.chars().all(|ch| {
                    ch.is_ascii_digit()
                        || matches!(ch, '.' | ',' | ':' | '/' | '-' | '+' | '%' | '$')
                }))
            .then(|| value.to_string())
        })
        .collect()
}

fn looks_like_model_commentary(text: &str) -> bool {
    let lower = text.trim_start().to_ascii_lowercase();
    lower.starts_with("here is ")
        || lower.starts_with("here's ")
        || lower.starts_with("certainly")
        || lower.starts_with("translation:")
        || lower.contains("as an ai")
}

fn render_html(review_json: &str) -> String {
    let embedded = review_json
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026");
    let mut html = String::new();
    html.push_str(REVIEW_HTML_PREFIX);
    html.push_str(REVIEW_CSS);
    html.push_str(REVIEW_HTML_MIDDLE);
    html.push_str(&embedded);
    html.push_str(REVIEW_HTML_SUFFIX);
    html
}

fn open_in_browser(path: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    let status = Command::new("open").arg(path).status()?;

    #[cfg(target_os = "windows")]
    let status = Command::new("cmd")
        .args(["/C", "start", "", &path.display().to_string()])
        .status()?;

    #[cfg(all(unix, not(target_os = "macos")))]
    let status = Command::new("xdg-open").arg(path).status()?;

    if !status.success() {
        anyhow::bail!("failed to open review HTML in default browser");
    }
    Ok(())
}

fn iso_timestamp(time: SystemTime) -> String {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let days = seconds.div_euclid(86_400);
    let seconds_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

const REVIEW_CSS: &str = r#"
:root {
  color-scheme: light;
  --bg: #f7f8fa;
  --panel: #ffffff;
  --text: #1d2329;
  --muted: #66717d;
  --line: #d9dee5;
  --accent: #0f766e;
  --warn: #b45309;
  --bad: #b91c1c;
  --shadow: 0 1px 2px rgba(21, 31, 43, 0.08);
}
* { box-sizing: border-box; }
body {
  margin: 0;
  background: var(--bg);
  color: var(--text);
  font: 14px/1.5 system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
}
header {
  position: sticky;
  top: 0;
  z-index: 10;
  display: grid;
  grid-template-columns: 1fr auto;
  gap: 12px;
  align-items: center;
  padding: 10px 16px;
  background: var(--panel);
  border-bottom: 1px solid var(--line);
  box-shadow: var(--shadow);
}
h1 { margin: 0; font-size: 16px; font-weight: 650; letter-spacing: 0; }
.meta { color: var(--muted); font-size: 12px; display: flex; flex-wrap: wrap; gap: 10px; }
.toolbar { display: flex; flex-wrap: wrap; gap: 8px; align-items: center; justify-content: flex-end; }
button, input, textarea { font: inherit; }
button {
  border: 1px solid var(--line);
  background: var(--panel);
  color: var(--text);
  border-radius: 6px;
  padding: 6px 10px;
  cursor: pointer;
}
button.active, button.primary { border-color: var(--accent); color: #075e57; background: #e7f5f2; }
input[type="search"] {
  min-width: 240px;
  border: 1px solid var(--line);
  border-radius: 6px;
  padding: 7px 9px;
  background: white;
}
.review-shell {
  display: grid;
  grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
  height: calc(100vh - 64px);
}
.pane {
  overflow: auto;
  padding: 12px;
  border-right: 1px solid var(--line);
}
.pane:last-child { border-right: 0; }
.pane-title {
  position: sticky;
  top: 0;
  z-index: 2;
  padding: 8px 4px;
  background: var(--bg);
  color: var(--muted);
  font-size: 12px;
  text-transform: uppercase;
}
.segment {
  min-height: 116px;
  margin-bottom: 10px;
  padding: 10px;
  background: var(--panel);
  border: 1px solid var(--line);
  border-radius: 8px;
  box-shadow: var(--shadow);
  white-space: pre-wrap;
}
.segment.hidden { display: none; }
.segment-head {
  display: flex;
  justify-content: space-between;
  gap: 8px;
  align-items: flex-start;
  margin-bottom: 7px;
  color: var(--muted);
  font-size: 12px;
}
.badges { display: flex; gap: 5px; flex-wrap: wrap; justify-content: flex-end; }
.badge {
  border: 1px solid #f0d5a8;
  color: var(--warn);
  background: #fff7ed;
  border-radius: 999px;
  padding: 1px 7px;
  font-size: 11px;
}
.badge.status-needs_review, .badge.status-failed, .badge.glossary-mismatch {
  border-color: #fecaca;
  background: #fef2f2;
  color: var(--bad);
}
.flag-panel {
  display: none;
  margin-top: 10px;
  border-top: 1px solid var(--line);
  padding-top: 10px;
}
.segment.flag-open .flag-panel { display: grid; gap: 8px; }
.kind-grid { display: flex; flex-wrap: wrap; gap: 8px 12px; font-size: 12px; }
textarea {
  width: 100%;
  min-height: 64px;
  resize: vertical;
  border: 1px solid var(--line);
  border-radius: 6px;
  padding: 7px;
}
.suggestions { display: grid; grid-template-columns: 1fr 1fr; gap: 8px; }
footer { padding: 10px 16px; color: var(--muted); font-size: 12px; border-top: 1px solid var(--line); }
@media (max-width: 840px) {
  header { grid-template-columns: 1fr; }
  .toolbar { justify-content: flex-start; }
  .review-shell { grid-template-columns: 1fr; height: auto; }
  .pane { height: 50vh; border-right: 0; border-bottom: 1px solid var(--line); }
  input[type="search"] { min-width: 0; width: 100%; }
  .suggestions { grid-template-columns: 1fr; }
}
"#;

const REVIEW_HTML_PREFIX: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>BookForge Review</title>
<style>
"#;

const REVIEW_HTML_MIDDLE: &str = r#"
</style>
</head>
<body>
<header>
  <div>
    <h1 id="title">BookForge Review</h1>
    <div class="meta" id="meta"></div>
  </div>
  <div class="toolbar">
    <input id="search" type="search" placeholder="Search">
    <button data-filter="all" class="active">all</button>
    <button data-filter="flagged">flagged</button>
    <button data-filter="warnings">warnings</button>
    <button data-filter="needs_review">needs review</button>
    <button data-filter="completed">completed</button>
    <button id="export" class="primary">Export flags</button>
  </div>
</header>
<main class="review-shell">
  <section class="pane" id="sourcePane"><div class="pane-title">Source</div><div id="sourceList"></div></section>
  <section class="pane" id="targetPane"><div class="pane-title">Translation</div><div id="targetList"></div></section>
</main>
<footer>This page contains the full text of your book. Treat as private.</footer>
<script id="embedded-review-json" type="application/json">
"#;

const REVIEW_HTML_SUFFIX: &str = r#"
</script>
<script>
let data;
let flags = {};
let filter = "all";
let syncing = false;

const kinds = ["name", "register", "wrong_translation", "formatting", "tone", "other"];

async function loadReview() {
  try {
    const response = await fetch("./review.json", {cache: "no-store"});
    if (!response.ok) throw new Error("fetch failed");
    return await response.json();
  } catch (_) {
    return JSON.parse(document.getElementById("embedded-review-json").textContent);
  }
}

function flagKey() {
  return `bookforge.review.flags.${data.job_id}`;
}

function loadFlags() {
  try { flags = JSON.parse(localStorage.getItem(flagKey()) || "{}"); }
  catch (_) { flags = {}; }
}

function saveFlags() {
  localStorage.setItem(flagKey(), JSON.stringify(flags));
  updateMeta();
}

function updateMeta() {
  const flagged = Object.keys(flags).length;
  const totals = data.totals;
  document.getElementById("meta").textContent =
    `${data.provider}/${data.model} | ${totals.segments} segments | ${flagged} flagged | ` +
    `${totals.tokens_input} input, ${totals.tokens_input_cached} cached, ${totals.tokens_output} output tokens`;
}

function warningLabel(w) {
  if (w.kind === "length_ratio" && w.value) return `length ${w.value}x`;
  if (w.kind === "url_changed") return "url";
  if (w.kind === "number_changed") return "number";
  if (w.kind === "glossary_mismatch") return `glossary: ${w.source || ""}`;
  return w.kind.replaceAll("_", " ");
}

function renderSegment(segment, side) {
  const flagged = flags[segment.segment_id];
  const el = document.createElement("article");
  el.className = "segment";
  el.dataset.segmentId = segment.segment_id;
  el.dataset.status = segment.status;
  el.dataset.warnings = String(segment.soft_warnings.length > 0);
  el.dataset.flagged = String(Boolean(flagged));
  el.dataset.text = `${segment.source_text} ${segment.target_text}`.toLowerCase();

  const head = document.createElement("div");
  head.className = "segment-head";
  const left = document.createElement("span");
  const approx = segment.tokens.estimated ? " ≈" : "";
  left.textContent = `${segment.ordinal}. ${segment.chapter_title || segment.chapter_id} | ${segment.status} | ${approx}${segment.tokens.input}/${segment.tokens.input_cached}/${segment.tokens.output}`;
  head.appendChild(left);

  const badges = document.createElement("div");
  badges.className = "badges";
  if (flagged) {
    const b = document.createElement("span");
    b.className = "badge";
    b.textContent = flagged.kind;
    badges.appendChild(b);
  }
  if (segment.status !== "succeeded" && segment.status !== "skipped_cached") {
    const b = document.createElement("span");
    b.className = `badge status-${segment.status}`;
    b.textContent = segment.status.replaceAll("_", " ");
    badges.appendChild(b);
  }
  for (const warning of segment.soft_warnings) {
    const b = document.createElement("span");
    b.className = warning.kind === "glossary_mismatch" ? "badge glossary-mismatch" : "badge";
    b.textContent = warningLabel(warning);
    b.title = warning.message || warning.kind;
    badges.appendChild(b);
  }
  head.appendChild(badges);
  el.appendChild(head);

  const text = document.createElement("div");
  text.textContent = side === "source" ? segment.source_text : segment.target_text;
  el.appendChild(text);

  if (side === "target") {
    const btn = document.createElement("button");
    btn.type = "button";
    btn.textContent = flagged ? "Edit flag" : "Flag";
    btn.addEventListener("click", () => el.classList.toggle("flag-open"));
    el.appendChild(btn);
    el.appendChild(renderFlagPanel(segment));
  }
  return el;
}

function renderFlagPanel(segment) {
  const current = flags[segment.segment_id] || {kind: "wrong_translation", note: "", suggested_source: "", suggested_target: ""};
  const panel = document.createElement("div");
  panel.className = "flag-panel";

  const kindGrid = document.createElement("div");
  kindGrid.className = "kind-grid";
  for (const kind of kinds) {
    const label = document.createElement("label");
    const radio = document.createElement("input");
    radio.type = "radio";
    radio.name = `kind-${segment.segment_id}`;
    radio.value = kind;
    radio.checked = current.kind === kind;
    label.appendChild(radio);
    label.append(` ${kind.replaceAll("_", " ")}`);
    kindGrid.appendChild(label);
  }
  panel.appendChild(kindGrid);

  const note = document.createElement("textarea");
  note.placeholder = "Note";
  note.value = current.note || "";
  panel.appendChild(note);

  const suggestions = document.createElement("div");
  suggestions.className = "suggestions";
  const source = document.createElement("textarea");
  source.placeholder = "Suggested source";
  source.value = current.suggested_source || "";
  const target = document.createElement("textarea");
  target.placeholder = "Suggested target";
  target.value = current.suggested_target || "";
  suggestions.append(source, target);
  panel.appendChild(suggestions);

  const actions = document.createElement("div");
  const save = document.createElement("button");
  save.type = "button";
  save.className = "primary";
  save.textContent = "Save flag";
  save.addEventListener("click", () => {
    const kind = panel.querySelector("input[type=radio]:checked").value;
    flags[segment.segment_id] = {
      segment_id: segment.segment_id,
      kind,
      note: note.value,
      suggested_source: source.value || null,
      suggested_target: target.value || null
    };
    saveFlags();
    render();
  });
  const clear = document.createElement("button");
  clear.type = "button";
  clear.textContent = "Clear";
  clear.addEventListener("click", () => {
    delete flags[segment.segment_id];
    saveFlags();
    render();
  });
  actions.append(save, clear);
  panel.appendChild(actions);
  return panel;
}

function render() {
  document.getElementById("title").textContent = data.source_book_title || "BookForge Review";
  updateMeta();
  const query = document.getElementById("search").value.trim().toLowerCase();
  const sourceList = document.getElementById("sourceList");
  const targetList = document.getElementById("targetList");
  sourceList.textContent = "";
  targetList.textContent = "";
  for (const segment of data.segments) {
    const hasFlag = Boolean(flags[segment.segment_id]);
    const visible =
      (!query || `${segment.source_text} ${segment.target_text}`.toLowerCase().includes(query)) &&
      (filter === "all" ||
       (filter === "flagged" && hasFlag) ||
       (filter === "warnings" && segment.soft_warnings.length > 0) ||
       (filter === "needs_review" && segment.status === "needs_review") ||
       (filter === "completed" && (segment.status === "succeeded" || segment.status === "skipped_cached")));
    if (!visible) continue;
    sourceList.appendChild(renderSegment(segment, "source"));
    targetList.appendChild(renderSegment(segment, "target"));
  }
}

function exportFlags() {
  const payload = {
    schema_version: 1,
    job_id: data.job_id,
    exported_at: new Date().toISOString(),
    flags: Object.values(flags).map(f => ({
      segment_id: f.segment_id,
      kind: f.kind,
      note: f.note || "",
      suggested_source: f.suggested_source || null,
      suggested_target: f.suggested_target || null
    }))
  };
  const blob = new Blob([JSON.stringify(payload, null, 2)], {type: "application/json"});
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = "flags.json";
  a.click();
  URL.revokeObjectURL(url);
}

function bindSync(a, b) {
  a.addEventListener("scroll", () => {
    if (syncing) return;
    syncing = true;
    const maxA = Math.max(1, a.scrollHeight - a.clientHeight);
    const maxB = Math.max(1, b.scrollHeight - b.clientHeight);
    b.scrollTop = (a.scrollTop / maxA) * maxB;
    syncing = false;
  });
}

document.addEventListener("DOMContentLoaded", async () => {
  data = await loadReview();
  loadFlags();
  render();
  document.getElementById("search").addEventListener("input", render);
  document.querySelectorAll("[data-filter]").forEach(button => {
    button.addEventListener("click", () => {
      filter = button.dataset.filter;
      document.querySelectorAll("[data-filter]").forEach(b => b.classList.toggle("active", b === button));
      render();
    });
  });
  document.getElementById("export").addEventListener("click", exportFlags);
  bindSync(document.getElementById("sourcePane"), document.getElementById("targetPane"));
  bindSync(document.getElementById("targetPane"), document.getElementById("sourcePane"));
});
</script>
</body>
</html>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_timestamp_formats_unix_epoch() {
        assert_eq!(
            iso_timestamp(UNIX_EPOCH),
            "1970-01-01T00:00:00Z".to_string()
        );
    }

    #[test]
    fn render_html_embeds_json_fallback() {
        let html = render_html(&json!({"schema_version": 1, "x": "<tag>"}).to_string());
        assert!(html.contains("embedded-review-json"));
        assert!(html.contains("\\u003ctag\\u003e"));
    }

    #[test]
    fn glossary_warning_reports_expected_target() {
        let warnings = glossary_warnings(
            "Aragorn arrived.",
            "Granpasso arrivo.",
            &[GlossaryTerm {
                id: Some(42),
                scope_kind: bookforge_core::GlossaryScopeKind::Book,
                scope_id: Some("fellowship".to_string()),
                source_text: "Aragorn".to_string(),
                target_text: "Aragorn".to_string(),
                category: bookforge_core::GlossaryCategory::Person,
                notes: None,
                case_sensitive: true,
                always_active: false,
                status: bookforge_core::GlossaryStatus::UserSeeded,
                source_language: "English".to_string(),
                target_language: "Italian".to_string(),
                source_count: 0,
            }],
        );

        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].kind, "glossary_mismatch");
        assert_eq!(warnings[0].term_id, Some(42));
        assert_eq!(warnings[0].expected_target.as_deref(), Some("Aragorn"));
    }
}
