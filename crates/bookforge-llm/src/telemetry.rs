use std::sync::Mutex;
use bookforge_core::config::ProviderRequestMetric;

pub struct TelemetryLog {
    entries: Mutex<Vec<ProviderRequestMetric>>,
}

impl TelemetryLog {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
        }
    }
}

impl Default for TelemetryLog {
    fn default() -> Self {
        Self::new()
    }
}

impl TelemetryLog {
    pub fn record(&self, metric: ProviderRequestMetric) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.push(metric);
        }
    }

    pub fn snapshot(&self) -> Vec<ProviderRequestMetric> {
        self.entries.lock().map(|e| e.clone()).unwrap_or_default()
    }
}

pub fn telemetry_summary(entries: &[ProviderRequestMetric]) -> String {
    if entries.is_empty() {
        return "no requests recorded".to_string();
    }

    let total = entries.len();
    let succeeded = entries.iter().filter(|e| e.status == "ok").count();
    let failed = total - succeeded;
    let rate_limited = entries
        .iter()
        .filter(|e| e.status_code == Some(429))
        .count();
    let timed_out = entries
        .iter()
        .filter(|e| e.status == "timeout")
        .count();
    let total_input_tokens: u64 = entries.iter().filter_map(|e| e.input_tokens).sum();
    let total_output_tokens: u64 = entries.iter().filter_map(|e| e.output_tokens).sum();

    let mut latencies: Vec<u64> = entries.iter().map(|e| e.latency_ms).collect();
    latencies.sort_unstable();

    let avg_latency = if total > 0 {
        latencies.iter().sum::<u64>() / total as u64
    } else {
        0
    };
    let p50 = percentile(&latencies, 50.0);
    let p95 = percentile(&latencies, 95.0);

    format!(
        "requests total={total} ok={succeeded} fail={failed} | p50={p50}ms p95={p95}ms avg={avg_latency}ms | 429s={rate_limited} timeouts={timed_out} | input_tokens={total_input_tokens} output_tokens={total_output_tokens}"
    )
}

fn percentile(sorted: &[u64], pct: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((pct / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[idx]
}
