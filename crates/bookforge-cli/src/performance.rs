use std::{collections::HashSet, fs::File, io::BufRead, path::Path};

use anyhow::Result;
use serde::Serialize;
use serde_json::Value;

/// Which part of a job's event log a [`RunPerformanceSummary`] describes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PerformanceScope {
    /// The most recent run: everything after the log's last `JobCreated`.
    #[default]
    LatestRun,
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct RunPerformanceSummary {
    pub scope: PerformanceScope,
    pub request_count: usize,
    pub p50_latency_ms: Option<u64>,
    pub p95_latency_ms: Option<u64>,
    pub rate_limited: usize,
    pub timeouts: usize,
    pub server_errors: usize,
    /// Requests that never reached the provider (connect/TLS failures).
    pub connection_errors: usize,
    pub invalid_responses: usize,
    pub truncations: usize,
    pub retries: usize,
    pub batch_splits: usize,
    pub repair_batches: usize,
    pub repair_failures: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Unique fresh terminal segment completions per minute, excluding cache
    /// hits. The JSON key keeps its historical name so existing report
    /// consumers keep the throughput field.
    #[serde(rename = "blocks_per_minute")]
    pub segments_per_minute: Option<f64>,
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
    let mut finished_segments = HashSet::<String>::new();
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
        // `resume` appends a fresh JobCreated to the same log, the run boundary
        // RunState also uses. Counters and finish times before it belong to an
        // earlier run. A log without any JobCreated keeps the whole stream.
        if kind == "JobCreated" {
            summary = RunPerformanceSummary::default();
            latencies.clear();
            first_ts = None;
            last_ts = None;
            finished_segments.clear();
            request_input_tokens = 0;
            request_output_tokens = 0;
            request_events_with_tokens = 0;
            segment_input_tokens = 0;
            segment_output_tokens = 0;
        }
        if let Some(ts) = payload
            .get("timestamp_ms")
            .and_then(Value::as_u64)
            .filter(|ts| *ts > 0)
        {
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
                let status_lower = status.to_ascii_lowercase();
                let code = payload.get("status_code").and_then(Value::as_u64);
                let error_kind = payload
                    .get("error_kind")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let error_kind_lower = error_kind.to_ascii_lowercase();
                if code == Some(429) || status_lower.contains("rate") {
                    summary.rate_limited += 1;
                }
                if status_lower.contains("timeout") || error_kind_lower.contains("timeout") {
                    summary.timeouts += 1;
                }
                if code.is_some_and(|code| (500..600).contains(&code))
                    || status_lower.contains("server")
                {
                    summary.server_errors += 1;
                }
                if status_lower == "connect_error" {
                    summary.connection_errors += 1;
                }
                // Transport diagnostics can mention terms such as InvalidData
                // or InvalidContentType without any model response existing,
                // so only response-level statuses count as invalid output.
                let invalid_response = match status_lower.as_str() {
                    "invalid_response" | "json_error" | "truncated" | "incomplete" => true,
                    status if status.starts_with("incomplete:") => true,
                    "" | "error" if code.is_none() => {
                        matches!(error_kind_lower.as_str(), "invalid_response" | "json")
                            || error_kind_lower.starts_with("invalidresponse(")
                            || error_kind_lower.starts_with("json(")
                    }
                    _ => false,
                };
                if invalid_response {
                    summary.invalid_responses += 1;
                }
                if payload
                    .get("finish_reason")
                    .and_then(Value::as_str)
                    .is_some_and(|reason| reason.eq_ignore_ascii_case("length"))
                    || status_lower.contains("truncated")
                    || error_kind_lower.contains("truncated")
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
                // Throughput counts each segment's fresh terminal outcome once,
                // like live progress: cache hits, pending states and repeated
                // outcomes for the same segment do not add work.
                if matches!(
                    payload.get("status").and_then(Value::as_str),
                    Some("succeeded" | "needs_review" | "failed")
                ) && let Some(id) = payload
                    .get("segment_id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                {
                    finished_segments.insert(id.to_string());
                }
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
        summary.segments_per_minute =
            Some(finished_segments.len() as f64 / (elapsed_ms as f64 / 60_000.0));
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
    use bookforge_core::ProgressEvent;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn performance_summary_from_events_counts_requests() {
        let path = temp_path("requests.jsonl");
        write_events(
            &path,
            &[
                ProgressEvent::RequestFinished {
                    request_id: "r1".to_string(),
                    batch_id: None,
                    segment_id: Some("s1".to_string()),
                    status: "ok".to_string(),
                    latency_ms: 10,
                    status_code: None,
                    finish_reason: None,
                    retry_count: 0,
                    input_tokens: Some(11),
                    output_tokens: Some(7),
                    error_kind: None,
                    timestamp_ms: 1,
                },
                ProgressEvent::RequestFinished {
                    request_id: "r2".to_string(),
                    batch_id: None,
                    segment_id: Some("s2".to_string()),
                    status: "ok".to_string(),
                    latency_ms: 20,
                    status_code: None,
                    finish_reason: None,
                    retry_count: 1,
                    input_tokens: Some(13),
                    output_tokens: Some(5),
                    error_kind: None,
                    timestamp_ms: 2,
                },
            ],
        );

        let summary = performance_summary_from_events(&path)
            .expect("summary should parse")
            .expect("summary should exist");

        assert_eq!(summary.request_count, 2);
        assert_eq!(summary.retries, 1);
        assert_eq!(summary.input_tokens, 24);
        assert_eq!(summary.output_tokens, 12);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn performance_summary_from_events_computes_latency_percentiles() {
        let path = temp_path("latencies.jsonl");
        write_events(
            &path,
            &[10, 20, 30, 40, 50]
                .into_iter()
                .enumerate()
                .map(|(index, latency_ms)| ProgressEvent::RequestFinished {
                    request_id: format!("r{index}"),
                    batch_id: None,
                    segment_id: Some(format!("s{index}")),
                    status: "ok".to_string(),
                    latency_ms,
                    status_code: None,
                    finish_reason: None,
                    retry_count: 0,
                    input_tokens: None,
                    output_tokens: None,
                    error_kind: None,
                    timestamp_ms: index as u64,
                })
                .collect::<Vec<_>>(),
        );

        let summary = performance_summary_from_events(&path)
            .expect("summary should parse")
            .expect("summary should exist");

        assert_eq!(summary.p50_latency_ms, Some(30));
        assert_eq!(summary.p95_latency_ms, Some(50));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn performance_summary_from_events_counts_rate_limits_timeouts_server_errors() {
        let path = temp_path("errors.jsonl");
        write_events(
            &path,
            &[
                request_finished("rate", "rate_limited", Some(429), None),
                request_finished("timeout", "timeout", None, Some("Http timeout")),
                request_finished("server", "server_error", None, None),
            ],
        );

        let summary = performance_summary_from_events(&path)
            .expect("summary should parse")
            .expect("summary should exist");

        assert_eq!(summary.rate_limited, 1);
        assert_eq!(summary.timeouts, 1);
        assert_eq!(summary.server_errors, 1);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn performance_summary_from_events_counts_invalid_responses_and_truncations() {
        let path = temp_path("invalid-truncated.jsonl");
        write_events(
            &path,
            &[
                request_finished(
                    "invalid",
                    "invalid_response",
                    None,
                    Some("InvalidResponse(\"bad json\")"),
                ),
                request_finished(
                    "truncated",
                    "truncated",
                    None,
                    Some("InvalidResponse(\"truncated response\")"),
                ),
                ProgressEvent::RequestFinished {
                    request_id: "length".to_string(),
                    batch_id: None,
                    segment_id: Some("s_length".to_string()),
                    status: "ok".to_string(),
                    latency_ms: 5,
                    status_code: None,
                    finish_reason: Some("length".to_string()),
                    retry_count: 0,
                    input_tokens: None,
                    output_tokens: None,
                    error_kind: None,
                    timestamp_ms: 3,
                },
            ],
        );

        let summary = performance_summary_from_events(&path)
            .expect("summary should parse")
            .expect("summary should exist");

        assert_eq!(summary.invalid_responses, 2);
        assert_eq!(summary.truncations, 2);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn performance_summary_from_events_counts_checkpoint_flushes() {
        let path = temp_path("checkpoint-flushes.jsonl");
        write_events(
            &path,
            &[
                ProgressEvent::CheckpointFlushed {
                    segment_id: Some("s1".to_string()),
                    flushed_count: 1,
                    latency_ms: Some(2),
                    timestamp_ms: 1,
                },
                ProgressEvent::CheckpointFlushed {
                    segment_id: Some("s2".to_string()),
                    flushed_count: 2,
                    latency_ms: Some(3),
                    timestamp_ms: 2,
                },
            ],
        );

        let summary = performance_summary_from_events(&path)
            .expect("summary should parse")
            .expect("summary should exist");

        assert_eq!(summary.checkpoint_flushes, 2);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn performance_summary_from_events_returns_none_for_missing_file() {
        let path = temp_path("missing.jsonl");

        let summary = performance_summary_from_events(&path).expect("missing file should not fail");

        assert!(summary.is_none());
    }

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

    #[test]
    fn transport_diagnostics_do_not_count_as_invalid_model_responses() {
        let path = temp_path("transport-diagnostics.jsonl");
        write_events(
            &path,
            &[
                request_finished(
                    "tls",
                    "connect_error",
                    None,
                    Some("Http(Error(Connect, InvalidData, InvalidMessage(InvalidContentType)))"),
                ),
                request_finished(
                    "timeout",
                    "timeout",
                    None,
                    Some("Http(timeout while reading json response)"),
                ),
                request_finished(
                    "http",
                    "error",
                    Some(400),
                    Some("HttpStatus(400, invalid JSON request)"),
                ),
                request_finished(
                    "legacy",
                    "error",
                    None,
                    Some("InvalidResponse(\"malformed JSON\")"),
                ),
                request_finished("partial", "incomplete:1/2", None, None),
                request_finished("review-invalid", "error", None, Some("invalid_response")),
                request_finished("review-json", "error", None, Some("json")),
            ],
        );
        let summary = performance_summary_from_events(&path).unwrap().unwrap();
        assert_eq!(summary.connection_errors, 1);
        assert_eq!(summary.timeouts, 1);
        assert_eq!(summary.invalid_responses, 4);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn resumed_performance_summary_contains_only_latest_run() {
        let summary = summary_from_values(&[
            serde_json::json!({"JobCreated":{"timestamp_ms":1000}}),
            serde_json::json!({"RequestFinished":{"status":"rate_limited","status_code":429,"latency_ms":900,"retry_count":2,"input_tokens":100,"output_tokens":40,"timestamp_ms":2000}}),
            serde_json::json!({"RequestFinished":{"status":"timeout","timestamp_ms":3000}}),
            serde_json::json!({"RequestFinished":{"status":"server_error","timestamp_ms":4000}}),
            serde_json::json!({"RequestFinished":{"status":"connect_error","timestamp_ms":5000}}),
            serde_json::json!({"RequestFinished":{"status":"truncated","timestamp_ms":6000}}),
            serde_json::json!({"SegmentFinished":{"segment_id":"old","status":"succeeded","input_tokens":100,"output_tokens":40,"timestamp_ms":7000}}),
            serde_json::json!({"BatchSplit":{}}),
            serde_json::json!({"BatchRepairStarted":{}}),
            serde_json::json!({"BatchRepairFinished":{"still_failed_items":1}}),
            serde_json::json!({"CheckpointFlushed":{}}),
            serde_json::json!({"TranslationFinished":{"elapsed_ms":120000,"timestamp_ms":121000}}),
            serde_json::json!({"JobCreated":{"timestamp_ms":1000000}}),
            serde_json::json!({"RequestFinished":{"status":"ok","latency_ms":50,"input_tokens":30,"output_tokens":10,"timestamp_ms":1020000}}),
            serde_json::json!({"SegmentFinished":{"segment_id":"new","status":"succeeded","input_tokens":30,"output_tokens":10,"timestamp_ms":1029000}}),
            serde_json::json!({"TranslationFinished":{"elapsed_ms":30000,"timestamp_ms":1030000}}),
        ]);

        assert_eq!(summary.request_count, 1);
        assert_eq!(
            serde_json::to_value(&summary).unwrap(),
            serde_json::json!({
                "scope":"latest_run", "request_count":1, "p50_latency_ms":50,
                "p95_latency_ms":50, "rate_limited":0, "timeouts":0, "server_errors":0,
                "connection_errors":0, "invalid_responses":0, "truncations":0,
                "retries":0, "batch_splits":0, "repair_batches":0, "repair_failures":0,
                "input_tokens":30, "output_tokens":10, "blocks_per_minute":2.0,
                "checkpoint_flushes":0, "elapsed_ms":30000
            }),
            "all accumulators reset, while the legacy throughput JSON key remains stable"
        );
    }

    #[test]
    fn unfinished_resume_discards_previous_elapsed_and_usage_fallback() {
        let summary = summary_from_values(&[
            serde_json::json!({"JobCreated":{"timestamp_ms":1000}}),
            serde_json::json!({"RequestFinished":{"status":"ok","latency_ms":900,"input_tokens":100,"output_tokens":40,"timestamp_ms":2000}}),
            serde_json::json!({"SegmentFinished":{"segment_id":"s1","status":"succeeded","input_tokens":100,"output_tokens":40,"timestamp_ms":3000}}),
            serde_json::json!({"TranslationFinished":{"elapsed_ms":120000,"timestamp_ms":121000}}),
            serde_json::json!({"JobCreated":{"timestamp_ms":1000000}}),
            serde_json::json!({"SegmentFinished":{"segment_id":"s1","status":"succeeded","input_tokens":13,"output_tokens":5,"timestamp_ms":1020000}}),
        ]);

        assert_eq!(summary.elapsed_ms, Some(20000));
        assert_eq!(summary.request_count, 0);
        assert_eq!(summary.p50_latency_ms, None);
        assert_eq!(summary.p95_latency_ms, None);
        assert_eq!(summary.input_tokens, 13);
        assert_eq!(summary.output_tokens, 5);
        assert_eq!(
            serde_json::to_value(summary).unwrap()["blocks_per_minute"],
            3.0
        );
    }

    #[test]
    fn throughput_counts_unique_work_completions_like_live_progress() {
        let mut events = vec![serde_json::json!({"JobCreated":{"timestamp_ms":1000}})];
        for (id, status) in [
            ("s1", "retry_pending"),
            ("s1", "succeeded"),
            ("s1", "succeeded"),
            ("s2", "retry_pending"),
            ("s3", "needs_review"),
            ("s4", "failed"),
            ("s5", "skipped_cached"),
            ("s6", "succeeded"),
            ("s6", "retry_pending"),
            ("s7", "queued"),
            ("s8", "unknown_status"),
        ] {
            events.push(serde_json::json!({"SegmentFinished":{"segment_id":id,"status":status,"timestamp_ms":2000}}));
        }
        events.push(
            serde_json::json!({"SegmentFinished":{"status":"succeeded","timestamp_ms":3000}}),
        );
        events.push(
            serde_json::json!({"TranslationFinished":{"elapsed_ms":60000,"timestamp_ms":61000}}),
        );

        let summary = summary_from_values(&events);
        assert_eq!(
            serde_json::to_value(summary).unwrap()["blocks_per_minute"],
            4.0
        );
    }

    #[test]
    fn cached_only_run_has_zero_work_throughput() {
        let summary = summary_from_values(&[
            serde_json::json!({"JobCreated":{"timestamp_ms":1000}}),
            serde_json::json!({"SegmentFinished":{"segment_id":"cached","status":"skipped_cached","timestamp_ms":2000}}),
            serde_json::json!({"TranslationFinished":{"elapsed_ms":1000,"timestamp_ms":2000}}),
        ]);

        assert_eq!(summary.segments_per_minute, Some(0.0));
    }

    #[test]
    fn legacy_log_without_job_boundary_keeps_whole_stream() {
        let summary = summary_from_values(&[
            serde_json::json!({"SegmentFinished":{"segment_id":"s1","status":"succeeded","input_tokens":11,"output_tokens":3,"timestamp_ms":1000}}),
            serde_json::json!({"SegmentFinished":{"segment_id":"s2","status":"succeeded","input_tokens":13,"output_tokens":5,"timestamp_ms":121000}}),
        ]);

        assert_eq!(summary.elapsed_ms, Some(120000));
        assert_eq!(summary.input_tokens, 24);
        assert_eq!(summary.output_tokens, 8);
        assert_eq!(
            serde_json::to_value(summary).unwrap()["blocks_per_minute"],
            1.0
        );
    }

    fn summary_from_values(events: &[serde_json::Value]) -> RunPerformanceSummary {
        let path = temp_path("run-performance.jsonl");
        let jsonl = events
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&path, jsonl).unwrap();
        let summary = performance_summary_from_events(&path).unwrap().unwrap();
        let _ = fs::remove_file(path);
        summary
    }

    fn temp_path(name: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        // Counter disambiguates calls that share a timestamp (the clock is
        // coarse on some platforms), keeping parallel tests off each other's
        // temp files.
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "bookforge-performance-test-{}-{nanos}-{seq}-{name}",
            std::process::id()
        ))
    }

    fn write_events(path: &std::path::Path, events: &[ProgressEvent]) {
        let jsonl = events
            .iter()
            .map(|event| serde_json::to_string(event).expect("event should serialize"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(path, jsonl).expect("fixture should be written");
    }

    fn request_finished(
        request_id: &str,
        status: &str,
        status_code: Option<u16>,
        error_kind: Option<&str>,
    ) -> ProgressEvent {
        ProgressEvent::RequestFinished {
            request_id: request_id.to_string(),
            batch_id: None,
            segment_id: Some(format!("segment_{request_id}")),
            status: status.to_string(),
            latency_ms: 5,
            status_code,
            finish_reason: None,
            retry_count: 0,
            input_tokens: None,
            output_tokens: None,
            error_kind: error_kind.map(ToOwned::to_owned),
            timestamp_ms: 1,
        }
    }
}
