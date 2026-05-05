use clap::Args;

use std::io::BufRead;

use serde_json::Value;

#[derive(Debug, Args)]
pub struct TailArgs {
    pub job_id: String,

    #[arg(long, default_value_t = 20)]
    pub lines: usize,
}

pub async fn run(args: TailArgs) -> anyhow::Result<()> {
    let event_log_path =
        std::path::PathBuf::from(format!(".bookforge/runs/{}/events.jsonl", args.job_id));

    if !event_log_path.exists() {
        anyhow::bail!(
            "event log not found for job '{}' at {}",
            args.job_id,
            event_log_path.display()
        );
    }

    let file = std::fs::File::open(&event_log_path)?;
    let reader = std::io::BufReader::new(file);

    let mut events: Vec<String> = Vec::new();
    for line in reader.lines() {
        match line {
            Ok(l) if !l.trim().is_empty() => events.push(l),
            _ => {}
        }
    }

    let start = events.len().saturating_sub(args.lines);
    let recent: Vec<&String> = events.iter().skip(start).collect();

    if recent.is_empty() {
        println!("(no events)");
        return Ok(());
    }

    println!("Last {} events for job {}:", recent.len(), args.job_id);
    println!();

    for line in &recent {
        // Pretty-print each JSON event with a type header
        if let Ok(parsed) = serde_json::from_str::<Value>(line) {
            let event_type = parsed
                .as_object()
                .and_then(|o| o.keys().next())
                .map(|k| k.as_str())
                .unwrap_or("?");
            let compact = serde_json::to_string(&parsed).unwrap_or_else(|_| line.to_string());
            println!("[{event_type}] {compact}");
        } else {
            println!("{line}");
        }
    }

    println!();

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

    println!("Reconstructed state:");
    println!("  stage:        {stage}");
    println!("  segments:     {segments_done}/{segments_total}");
    println!(
        "  cache:        {} hits, {} misses",
        cache_hits, cache_misses
    );
    println!("  input tokens:  {input_tokens}");
    println!("  output tokens: {output_tokens}");
    println!("  checkpoints:   {checkpoint_flushed}");

    Ok(())
}
