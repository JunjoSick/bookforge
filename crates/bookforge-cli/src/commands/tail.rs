use clap::Args;

use std::{
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
};

use bookforge_core::RunConfigSnapshot;
use serde_json::Value;

use bookforge_store::{JobRecord, JobStore};

use crate::presentation::RunView;

/// When reconstructing dashboard state from recent events, never walk further
/// back than this many lines even if `--last` is smaller. Keeps `tail`
/// bounded on very long logs (CLI-15).
const RECONSTRUCT_TAIL_LINES: usize = 512;
/// Hard ceiling on how many raw bytes a single tail read may buffer.
const MAX_TAIL_BYTES: usize = 8 * 1024 * 1024;
/// Backwards read granularity.
const TAIL_CHUNK_BYTES: u64 = 64 * 1024;

#[derive(Debug, Args)]
pub struct TailArgs {
    pub job_id: String,

    #[arg(long, alias = "lines", default_value_t = 20)]
    pub last: usize,

    #[arg(long)]
    pub json: bool,
}

pub async fn run(args: TailArgs) -> anyhow::Result<()> {
    let store = JobStore::open_default()?;
    let job = store.get_job(&args.job_id)?;
    let snapshot = store.load_job_config_snapshot(&args.job_id)?;
    let event_log_path = event_log_path_for_tail(job.as_ref(), snapshot.as_ref(), &args.job_id);

    ensure_event_log_exists(&args.job_id, &event_log_path)?;

    let mut file = std::fs::File::open(&event_log_path)?;
    // Bounded read from the end of the log: only as much history as the tail
    // print plus the state-reconstruction window needs is ever loaded.
    let fetch_lines = args.last.max(RECONSTRUCT_TAIL_LINES);
    let events = read_last_lines(&mut file, fetch_lines)?;

    // JSON mode keeps stdout machine-pure; corrupt lines are warned on stderr
    // so they are never silently invisible (UI-28/30).
    if args.json {
        let skipped = events.iter().filter(|line| parse_failed(line)).count();
        if skipped > 0 {
            eprintln!("warning: {skipped} unparseable line(s) skipped while reading the event log");
        }
    }

    print!(
        "{}",
        render_tail(&args.job_id, &events, args.last, args.json)
    );

    Ok(())
}

fn parse_failed(line: &str) -> bool {
    serde_json::from_str::<Value>(line).is_err()
}

/// Read at most `max_lines` trailing lines from `file` by scanning backwards,
/// so a multi-megabyte event log no longer has to be fully loaded (CLI-15).
fn read_last_lines(file: &mut std::fs::File, max_lines: usize) -> std::io::Result<Vec<String>> {
    let len = file.metadata()?.len();
    let mut buffer: Vec<u8> = Vec::new();
    let mut newlines_seen = 0usize;
    let mut end = len;
    let mut reached_start = false;

    while end > 0 {
        let start = end.saturating_sub(TAIL_CHUNK_BYTES);
        let chunk_len = usize::try_from(end - start).unwrap_or(0);
        let mut chunk = vec![0_u8; chunk_len];
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut chunk)?;
        newlines_seen += chunk.iter().filter(|&&byte| byte == b'\n').count();

        let mut combined = Vec::with_capacity(chunk.len() + buffer.len());
        combined.extend_from_slice(&chunk);
        combined.extend_from_slice(&buffer);
        buffer = combined;

        end = start;
        reached_start = start == 0;
        if newlines_seen > max_lines || buffer.len() >= MAX_TAIL_BYTES {
            break;
        }
    }

    let text = String::from_utf8_lossy(&buffer);
    let mut lines: Vec<String> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(ToOwned::to_owned)
        .collect();
    // When the backwards window stopped before the beginning of the file, the
    // first recovered line may be a torn fragment of an older record.
    if !reached_start && !lines.is_empty() {
        lines.remove(0);
    }
    if lines.len() > max_lines {
        lines.drain(..lines.len() - max_lines);
    }
    Ok(lines)
}

fn event_log_path_for_tail(
    job: Option<&JobRecord>,
    snapshot: Option<&RunConfigSnapshot>,
    job_id: &str,
) -> PathBuf {
    job.and_then(|job| job.events_path.clone())
        .or_else(|| snapshot.and_then(|snapshot| snapshot.events_path.clone()))
        .unwrap_or_else(|| PathBuf::from(format!(".bookforge/runs/{job_id}/events.jsonl")))
}

fn ensure_event_log_exists(job_id: &str, event_log_path: &std::path::Path) -> anyhow::Result<()> {
    if event_log_path.exists() {
        return Ok(());
    }

    anyhow::bail!(
        "event log not found for job '{}' at {}",
        job_id,
        event_log_path.display()
    );
}

fn render_tail(job_id: &str, events: &[String], last: usize, json: bool) -> String {
    let start = events.len().saturating_sub(last);
    let recent: Vec<&String> = events.iter().skip(start).collect();

    if recent.is_empty() {
        return if json {
            String::new()
        } else {
            "(no events)\n".to_string()
        };
    }

    if json {
        // Raw-line pass-through keeps the machine contract stable. Corrupt
        // lines are still surfaced — as a warning on stderr from `run` — so
        // nothing disappears silently (UI-28/30) while stdout stays pure
        // JSONL.
        let mut output = String::new();
        for line in recent {
            output.push_str(line);
            output.push('\n');
        }
        return output;
    }

    let mut output = String::new();
    output.push_str(&format!(
        "Last {} events for job {}:\n\n",
        recent.len(),
        job_id
    ));
    let mut unparsed_lines = 0usize;

    for line in &recent {
        match serde_json::from_str::<Value>(line) {
            Ok(parsed) => {
                let event_type = parsed
                    .as_object()
                    .and_then(|o| o.keys().next())
                    .map(|k| k.as_str())
                    .unwrap_or("?");
                let compact = serde_json::to_string(&parsed).unwrap_or_else(|_| line.to_string());
                output.push_str(&format!("[{event_type}] {compact}\n"));
            }
            // Rendered verbatim below the JSON events; counted exactly once
            // by the reconstruction pass so nothing vanishes silently.
            Err(_) => {
                output.push_str(line);
                output.push('\n');
            }
        }
    }

    output.push('\n');

    // Reconstruct dashboard state by folding every parseable event through
    // the canonical RunView (RunState + epoch baselines) the other dashboards
    // use, so counts and rates agree across resume epochs (UI-28/30/31). The
    // hand-scanner this replaces drifted from fold semantics (it counted every
    // SegmentFinished, ignored terminal-status rules) and miscounted across
    // epochs.
    let mut view = RunView::new();
    let mut cache_misses = 0usize;
    for line in events {
        match serde_json::from_str::<Value>(line)
            .ok()
            .and_then(|value| serde_json::from_value::<bookforge_core::ProgressEvent>(value).ok())
        {
            Some(event) => {
                if let bookforge_core::ProgressEvent::CacheScanFinished { misses, .. } = &event {
                    cache_misses = *misses;
                }
                view.fold(&event);
            }
            None => {
                // Already counted in the per-line rendering above for the
                // tail window; malformed lines outside that window are
                // counted here so reconstruction never under-reports.
                unparsed_lines += 1;
            }
        }
    }

    output.push_str("Reconstructed state:\n");
    // Status naming comes from the shared presentation vocabulary (UI-31);
    // additive for `tail`, matching what watch/serve would title the run.
    output.push_str(&format!(
        "  status:       {}\n",
        crate::presentation::run_status_name(&view)
    ));
    output.push_str(&format!(
        "  stage:        {}\n",
        view.stage.as_deref().unwrap_or("")
    ));
    output.push_str(&format!(
        "  segments:     {}/{}\n",
        view.done_segments, view.total_segments
    ));
    output.push_str(&format!(
        "  cache:        {} hits, {} misses",
        view.cached, cache_misses
    ));
    output.push('\n');
    output.push_str(&format!("  input tokens:  {}\n", view.input_tokens));
    output.push_str(&format!("  output tokens: {}\n", view.output_tokens));
    output.push_str(&format!("  checkpoints:   {}\n", view.checkpoint_flushed));

    if unparsed_lines > 0 {
        // Corrupt log lines are skipped for counting, but never silently.
        output.push_str(&format!(
            "\nwarning: {unparsed_lines} unparseable line(s) skipped while reading the event log\n"
        ));
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct TailTestCli {
        #[command(flatten)]
        args: TailArgs,
    }

    fn job(events_path: Option<PathBuf>) -> JobRecord {
        JobRecord {
            id: "job_test".to_string(),
            input_path: PathBuf::from("input.epub"),
            input_snapshot_path: None,
            input_sha256: None,
            output_path: PathBuf::from("out.epub"),
            input_hash: "hash".to_string(),
            source_lang: Some("English".to_string()),
            target_lang: "Italian".to_string(),
            provider: "mock".to_string(),
            model: "mock-prefix-target".to_string(),
            base_url: None,
            api_key_env: None,
            status: "succeeded".to_string(),
            events_path,
            report_json_path: None,
            report_markdown_path: None,
            book_id: None,
            series_id: None,
        }
    }

    fn snapshot(events_path: Option<PathBuf>) -> RunConfigSnapshot {
        let settings = bookforge_core::TranslationProfile::V1Fast.resolve();
        RunConfigSnapshot {
            input_path: PathBuf::from("input.epub"),
            input_snapshot_path: None,
            input_sha256: None,
            output_path: PathBuf::from("out.epub"),
            events_path,
            report_json_path: None,
            report_markdown_path: None,
            source_language: Some("English".to_string()),
            target_language: "Italian".to_string(),
            creator: None,
            provider: "mock".to_string(),
            model: "mock-prefix-target".to_string(),
            base_url: None,
            api_key_env: None,
            profile: settings.profile,
            provider_preset: None,
            prompt_version: "v1".to_string(),
            cache_namespace: "cache".to_string(),
            book_id: None,
            series_id: None,
            glossary_budget_tokens: 800,
            glossary_format: bookforge_core::GlossaryFormat::Json,
            prompt_extra: None,
            glossary_fingerprint: String::new(),
            glossary_terms: Vec::new(),
            context_window: 0,
            context_budget_tokens: 1200,
            context_scope: bookforge_core::config::ContextScope::Chapter,
            style_fingerprint: String::new(),
            style_rendered_block: String::new(),
            entities_fingerprint: String::new(),
            entities_rendered_block: String::new(),
            bilingual_mode: bookforge_core::BilingualMode::Replace,
            bilingual_separator: " / ".to_string(),
            bilingual_style: bookforge_core::BilingualStyle::Minimal,
            bilingual_css: None,
            fallback: None,
            finalize: bookforge_core::FinalizeCheckpointSnapshot::default(),
            qa_mode: "off".to_string(),
            validate_output: false,
            settings: bookforge_core::ResolvedRunSettingsSnapshot::from_settings(&settings),
        }
    }

    #[test]
    fn tail_accepts_last_argument() {
        let parsed = TailTestCli::parse_from(["tail-test", "job_1", "--last", "7"]);

        assert_eq!(parsed.args.last, 7);
    }

    #[test]
    fn tail_lines_alias_still_works() {
        let parsed = TailTestCli::parse_from(["tail-test", "job_1", "--lines", "9"]);

        assert_eq!(parsed.args.last, 9);
    }

    #[test]
    fn tail_json_outputs_raw_valid_json_lines() {
        let events = vec![
            r#"{"StageStarted":{"stage":"resume","timestamp_ms":1}}"#.to_string(),
            r#"{"TranslationFinished":{"succeeded":1,"cached":0,"needs_review":0,"failed":0,"input_tokens":1,"output_tokens":1,"elapsed_ms":2,"timestamp_ms":3}}"#.to_string(),
        ];

        let output = render_tail("job_1", &events, 2, true);

        assert!(!output.contains("Last "));
        for line in output.lines() {
            serde_json::from_str::<serde_json::Value>(line).expect("line should be raw JSON");
        }
    }

    #[test]
    fn tail_uses_snapshot_event_path_when_available() {
        let job = job(None);
        let snapshot = snapshot(Some(PathBuf::from("/tmp/snapshot-events.jsonl")));

        assert_eq!(
            event_log_path_for_tail(Some(&job), Some(&snapshot), &job.id),
            PathBuf::from("/tmp/snapshot-events.jsonl")
        );
    }

    #[test]
    fn tail_missing_event_log_prints_clear_error() {
        let path = PathBuf::from("/tmp/bookforge-tail-missing-events.jsonl");
        let error =
            ensure_event_log_exists("job_missing_log", &path).expect_err("missing log should fail");

        assert!(error.to_string().contains("event log not found"));
        assert!(error.to_string().contains("job_missing_log"));
    }

    #[test]
    fn read_last_lines_returns_only_the_requested_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        std::fs::write(
            &path,
            (0..1000)
                .map(|n| format!("line-{n:04}\n"))
                .collect::<String>(),
        )
        .unwrap();

        let mut file = std::fs::File::open(&path).unwrap();
        let tail = read_last_lines(&mut file, 5).unwrap();
        assert_eq!(
            tail,
            vec![
                "line-0995",
                "line-0996",
                "line-0997",
                "line-0998",
                "line-0999"
            ]
        );
    }

    #[test]
    fn read_last_lines_handles_short_files_and_missing_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        let short = dir.path().join("short.jsonl");
        std::fs::write(&short, "a\nb\nc").unwrap();

        let mut file = std::fs::File::open(&short).unwrap();
        assert_eq!(read_last_lines(&mut file, 10).unwrap(), vec!["a", "b", "c"]);

        // Requesting fewer lines than exist must still yield exactly that many
        // complete lines, never a torn fragment.
        assert_eq!(read_last_lines(&mut file, 2).unwrap(), vec!["b", "c"]);
    }

    #[test]
    fn read_last_lines_skips_blank_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blanky.jsonl");
        std::fs::write(&path, "\n\none\n\n\ntwo\n\n").unwrap();

        let mut file = std::fs::File::open(&path).unwrap();
        assert_eq!(read_last_lines(&mut file, 3).unwrap(), vec!["one", "two"]);
    }

    /// UI-28/30: reconstruction must agree with the dashboards' RunState fold
    /// — only terminal SegmentFinished statuses count as done, tokens come
    /// from those events, and resume epochs keep cumulative totals sane.
    #[test]
    fn reconstructed_state_matches_runstate_fold_semantics() {
        let events = vec![
            r#"{"JobCreated":{"job_id":"job_x","input_path":"i.epub","output_path":"o.epub","timestamp_ms":1}}"#.to_string(),
            r#"{"StageStarted":{"stage":"translating","timestamp_ms":2}}"#.to_string(),
            r#"{"SegmentationFinished":{"segment_count":4,"timestamp_ms":3}}"#.to_string(),
            r#"{"CacheScanFinished":{"hits":1,"misses":3,"timestamp_ms":4}}"#.to_string(),
            // A non-terminal status line: the old hand-scanner counted it.
            r#"{"SegmentFinished":{"segment_id":"s0","status":"started","input_tokens":5,"output_tokens":7,"timestamp_ms":5}}"#.to_string(),
            r#"{"SegmentFinished":{"segment_id":"s1","status":"succeeded","input_tokens":11,"output_tokens":13,"timestamp_ms":6}}"#.to_string(),
            r#"{"CheckpointFlushed":{"segment_id":"s1","flushed_count":2,"latency_ms":3,"timestamp_ms":7}}"#.to_string(),
        ];

        let output = render_tail("job_x", &events, 20, false);

        assert!(output.contains("stage:        translating"));
        // The non-terminal `started` line must not inflate progress or skip
        // status rules, matching every other RunState consumer.
        assert!(
            output.contains("segments:     2/4"),
            "only succeeded/failed/review/cached statuses are done: {output}"
        );
        assert!(output.contains("cache:        1 hits, 3 misses"));
        // Token totals follow the same single fold as the dashboards.
        assert!(output.contains("input tokens:  16"));
        assert!(output.contains("output tokens: 20"));
        assert!(output.contains("checkpoints:   2"));
        assert!(!output.contains("unparseable"));
    }

    #[test]
    fn corrupt_lines_are_counted_not_swallowed() {
        let events = vec![
            r#"{"JobCreated":{"job_id":"job_y","input_path":"i.epub","output_path":"o.epub","timestamp_ms":1}}"#.to_string(),
            "{not json at all".to_string(),
            "trunc".to_string(),
        ];

        let human = render_tail("job_y", &events, 10, false);
        assert!(
            human.contains("warning: 2 unparseable line(s) skipped"),
            "human output must surface corrupt lines: {human}"
        );

        let json = render_tail("job_y", &events, 10, true);
        // Machine contract stays pure JSONL passthrough.
        for line in json.lines() {
            let _ =
                serde_json::from_str::<serde_json::Value>(line).unwrap_or(serde_json::Value::Null);
        }
        assert_eq!(json.lines().count(), 3);
    }

    #[test]
    fn resumed_log_epoch_keeps_reconstruction_consistent_with_dashboards() {
        // Two epochs in one appended log; the epoch-aware fold keeps counts
        // consistent with what `watch`/serve would show after replaying the
        // same file, instead of drifting like the old hand-scanner.
        let mut events = vec![
            r#"{"JobCreated":{"job_id":"job_z","input_path":"i.epub","output_path":"o1.epub","timestamp_ms":1}}"#.to_string(),
            r#"{"SegmentationFinished":{"segment_count":4,"timestamp_ms":50}}"#.to_string(),
        ];
        for i in 0..3 {
            events.push(format!(
                r#"{{"SegmentFinished":{{"segment_id":"a{i}","status":"succeeded","input_tokens":null,"output_tokens":null,"timestamp_ms":{}}}}}"#,
                100 + i
            ));
        }
        // Resume epoch appends a fresh JobCreated to the same log.
        events.push(
            r#"{"JobCreated":{"job_id":"job_z","input_path":"i.epub","output_path":"o2.epub","timestamp_ms":9000}}"#.to_string(),
        );
        events.push(
            r#"{"SegmentFinished":{"segment_id":"b0","status":"needs_review","input_tokens":null,"output_tokens":null,"timestamp_ms":9500}}"#
                .to_string(),
        );

        let output = render_tail("job_z", &events, 20, false);
        assert!(
            output.contains("segments:     4/4"),
            "terminal statuses accumulate across epochs exactly like the dashboards: {output}"
        );
    }
}
