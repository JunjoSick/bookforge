use clap::Args;

use std::{io::BufRead, path::PathBuf};

use bookforge_core::RunConfigSnapshot;
use serde_json::Value;

use bookforge_store::{JobRecord, JobStore};

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

    let file = std::fs::File::open(&event_log_path)?;
    let reader = std::io::BufReader::new(file);

    let mut events: Vec<String> = Vec::new();
    for line in reader.lines() {
        match line {
            Ok(l) if !l.trim().is_empty() => events.push(l),
            _ => {}
        }
    }

    print!(
        "{}",
        render_tail(&args.job_id, &events, args.last, args.json)
    );

    Ok(())
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

    for line in &recent {
        if let Ok(parsed) = serde_json::from_str::<Value>(line) {
            let event_type = parsed
                .as_object()
                .and_then(|o| o.keys().next())
                .map(|k| k.as_str())
                .unwrap_or("?");
            let compact = serde_json::to_string(&parsed).unwrap_or_else(|_| line.to_string());
            output.push_str(&format!("[{event_type}] {compact}\n"));
        } else {
            output.push_str(line);
            output.push('\n');
        }
    }

    output.push('\n');

    // Reconstruct a simple dashboard from recent events
    let mut stage = String::new();
    let mut segments_total = 0usize;
    let mut segments_done = 0usize;
    let mut cache_hits = 0usize;
    let mut cache_misses = 0usize;
    let mut input_tokens = 0u64;
    let mut output_tokens = 0u64;
    let mut checkpoint_flushed = 0usize;

    for line in events.iter().rev() {
        if let Ok(parsed) = serde_json::from_str::<Value>(line) {
            if let Some(v) = parsed.get("StageStarted")
                && let Some(s) = v.get("stage").and_then(|s| s.as_str())
                && stage.is_empty()
            {
                stage = s.to_string();
            }
            if let Some(v) = parsed.get("SegmentationFinished") {
                segments_total =
                    v.get("segment_count").and_then(|s| s.as_u64()).unwrap_or(0) as usize;
            }
            if let Some(v) = parsed.get("CacheScanFinished") {
                cache_hits = v.get("hits").and_then(|s| s.as_u64()).unwrap_or(0) as usize;
                cache_misses = v.get("misses").and_then(|s| s.as_u64()).unwrap_or(0) as usize;
            }
            if let Some(v) = parsed.get("SegmentFinished") {
                segments_done += 1;
                input_tokens += v.get("input_tokens").and_then(|s| s.as_u64()).unwrap_or(0);
                output_tokens += v.get("output_tokens").and_then(|s| s.as_u64()).unwrap_or(0);
            }
            if let Some(_v) = parsed.get("CheckpointFlushed") {
                checkpoint_flushed += 1;
            }
        }
    }

    output.push_str("Reconstructed state:\n");
    output.push_str(&format!("  stage:        {stage}\n"));
    output.push_str(&format!(
        "  segments:     {segments_done}/{segments_total}\n"
    ));
    output.push_str(&format!(
        "  cache:        {} hits, {} misses",
        cache_hits, cache_misses
    ));
    output.push('\n');
    output.push_str(&format!("  input tokens:  {input_tokens}\n"));
    output.push_str(&format!("  output tokens: {output_tokens}\n"));
    output.push_str(&format!("  checkpoints:   {checkpoint_flushed}\n"));

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
}
