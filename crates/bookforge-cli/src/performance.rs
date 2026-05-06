use std::{fs::File, io::BufRead, path::Path};

use anyhow::Result;
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct RunPerformanceSummary {
    pub request_count: usize,
    pub p50_latency_ms: Option<u64>,
    pub p95_latency_ms: Option<u64>,
    pub rate_limited: usize,
    pub timeouts: usize,
    pub server_errors: usize,
    pub invalid_responses: usize,
    pub truncations: usize,
    pub retries: usize,
    pub batch_splits: usize,
    pub repair_batches: usize,
    pub repair_failures: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub blocks_per_minute: Option<f64>,
    pub checkpoint_flushes: usize,
    pub elapsed_ms: Option<u64>,
}

pub(crate) fn performance_summary_from_events(
    path: &Path,
) -> Result<Option<RunPerformanceSummary>> {
    if !path.exists() {
        return Ok(None);
    }
    let file = File::open(path)?;
    let reader = std::io::BufReader::new(file);
    let mut summary = RunPerformanceSummary::default();
    let mut latencies = Vec::<u64>::new();
    let mut first_ts = None::<u64>;
    let mut last_ts = None::<u64>;
    let mut finished_segments = 0usize;
    let mut request_input_tokens = 0u64;
    let mut request_output_tokens = 0u64;
    let mut request_events_with_tokens = 0usize;
    let mut segment_input_tokens = 0u64;
    let mut segment_output_tokens = 0u64;

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some((kind, payload)) = value.as_object().and_then(|object| object.iter().next())
        else {
            continue;
        };
        if let Some(ts) = payload.get("timestamp_ms").and_then(Value::as_u64) {
            first_ts = Some(first_ts.map_or(ts, |current| current.min(ts)));
            last_ts = Some(last_ts.map_or(ts, |current| current.max(ts)));
        }
        match kind.as_str() {
            "RequestFinished" => {
                summary.request_count += 1;
                if let Some(latency) = payload.get("latency_ms").and_then(Value::as_u64) {
                    latencies.push(latency);
                }
                let status = payload.get("status").and_then(Value::as_str).unwrap_or("");
                let code = payload.get("status_code").and_then(Value::as_u64);
                let error_kind = payload
                    .get("error_kind")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if code == Some(429) || status.contains("rate") {
                    summary.rate_limited += 1;
                }
                if status.contains("timeout") || error_kind.contains("timeout") {
                    summary.timeouts += 1;
                }
                if code.is_some_and(|code| (500..600).contains(&code)) {
                    summary.server_errors += 1;
                }
                if error_kind.contains("json")
                    || error_kind.contains("invalid")
                    || status.contains("invalid")
                {
                    summary.invalid_responses += 1;
                }
                if payload
                    .get("finish_reason")
                    .and_then(Value::as_str)
                    .is_some_and(|reason| reason.eq_ignore_ascii_case("length"))
                {
                    summary.truncations += 1;
                }
                summary.retries += payload
                    .get("retry_count")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;
                let input_tokens = payload.get("input_tokens").and_then(Value::as_u64);
                let output_tokens = payload.get("output_tokens").and_then(Value::as_u64);
                if input_tokens.is_some() || output_tokens.is_some() {
                    request_events_with_tokens += 1;
                }
                request_input_tokens += input_tokens.unwrap_or(0);
                request_output_tokens += output_tokens.unwrap_or(0);
            }
            "SegmentFinished" => {
                finished_segments += 1;
                segment_input_tokens += payload
                    .get("input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                segment_output_tokens += payload
                    .get("output_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
            }
            "BatchSplit" => summary.batch_splits += 1,
            "BatchRepairStarted" => summary.repair_batches += 1,
            "BatchRepairFinished"
                if payload
                    .get("still_failed_items")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    > 0 =>
            {
                summary.repair_failures += 1;
            }
            "BatchRepairFinished" => {}
            "CheckpointFlushed" => summary.checkpoint_flushes += 1,
            "TranslationFinished" => {
                summary.elapsed_ms = payload.get("elapsed_ms").and_then(Value::as_u64);
            }
            _ => {}
        }
    }

    latencies.sort_unstable();
    summary.p50_latency_ms = percentile(&latencies, 0.50);
    summary.p95_latency_ms = percentile(&latencies, 0.95);
    if request_events_with_tokens > 0 {
        summary.input_tokens = request_input_tokens;
        summary.output_tokens = request_output_tokens;
    } else {
        summary.input_tokens = segment_input_tokens;
        summary.output_tokens = segment_output_tokens;
    }
    let elapsed_ms = summary.elapsed_ms.or_else(|| {
        first_ts
            .zip(last_ts)
            .map(|(first, last)| last.saturating_sub(first))
    });
    summary.elapsed_ms = elapsed_ms;
    if let Some(elapsed_ms) = elapsed_ms.filter(|elapsed| *elapsed > 0) {
        summary.blocks_per_minute = Some(finished_segments as f64 / (elapsed_ms as f64 / 60_000.0));
    }

    Ok(Some(summary))
}

fn percentile(values: &[u64], percentile: f64) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    let index = ((values.len() - 1) as f64 * percentile).round() as usize;
    values.get(index).copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn request_and_segment_tokens_are_not_double_counted() {
        let path = temp_path("request-segment-tokens.jsonl");
        fs::write(
            &path,
            [
                r#"{"RequestFinished":{"request_id":"r1","batch_id":null,"segment_id":"s1","status":"ok","latency_ms":10,"status_code":null,"finish_reason":null,"retry_count":0,"input_tokens":11,"output_tokens":7,"error_kind":null,"timestamp_ms":1}}"#,
                r#"{"SegmentFinished":{"segment_id":"s1","status":"succeeded","input_tokens":11,"output_tokens":7,"timestamp_ms":2}}"#,
            ]
            .join("\n"),
        )
        .expect("fixture should be written");

        let summary = performance_summary_from_events(&path)
            .expect("summary should parse")
            .expect("summary should exist");

        assert_eq!(summary.input_tokens, 11);
        assert_eq!(summary.output_tokens, 7);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn segment_tokens_are_used_when_request_tokens_are_absent() {
        let path = temp_path("segment-only-tokens.jsonl");
        fs::write(
            &path,
            [
                r#"{"RequestFinished":{"request_id":"r1","batch_id":null,"segment_id":"s1","status":"ok","latency_ms":10,"status_code":null,"finish_reason":null,"retry_count":0,"input_tokens":null,"output_tokens":null,"error_kind":null,"timestamp_ms":1}}"#,
                r#"{"SegmentFinished":{"segment_id":"s1","status":"succeeded","input_tokens":13,"output_tokens":5,"timestamp_ms":2}}"#,
            ]
            .join("\n"),
        )
        .expect("fixture should be written");

        let summary = performance_summary_from_events(&path)
            .expect("summary should parse")
            .expect("summary should exist");

        assert_eq!(summary.input_tokens, 13);
        assert_eq!(summary.output_tokens, 5);
        let _ = fs::remove_file(path);
    }

    fn temp_path(name: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "bookforge-performance-test-{}-{nanos}-{name}",
            std::process::id()
        ))
    }
}
