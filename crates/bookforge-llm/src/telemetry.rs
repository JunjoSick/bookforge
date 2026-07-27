use bookforge_core::config::ProviderRequestMetric;
use bookforge_core::glossary::GlossarySelectionRule;
use std::{collections::HashMap, sync::Mutex};

pub struct TelemetryLog {
    entries: Mutex<Vec<ProviderRequestMetric>>,
    glossary_rules: Mutex<HashMap<GlossarySelectionRule, GlossaryRuleCounters>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct GlossaryRuleCounters {
    injected: usize,
    honored: usize,
}

impl TelemetryLog {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
            glossary_rules: Mutex::new(HashMap::new()),
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

    pub fn record_glossary_entry(&self, rule: GlossarySelectionRule, honored: bool) {
        if let Ok(mut rules) = self.glossary_rules.lock() {
            let counters = rules.entry(rule).or_default();
            counters.injected += 1;
            counters.honored += usize::from(honored);
        }
    }

    pub fn glossary_rule_counts(&self, rule: GlossarySelectionRule) -> (usize, usize) {
        self.glossary_rules
            .lock()
            .ok()
            .and_then(|rules| rules.get(&rule).copied())
            .map(|counters| (counters.injected, counters.honored))
            .unwrap_or_default()
    }

    pub fn has_glossary_entries(&self) -> bool {
        self.glossary_rules
            .lock()
            .is_ok_and(|rules| rules.values().any(|counters| counters.injected > 0))
    }

    pub fn glossary_summary(&self) -> String {
        const RULES: [GlossarySelectionRule; 4] = [
            GlossarySelectionRule::SegmentMatched,
            GlossarySelectionRule::AlwaysActive,
            GlossarySelectionRule::RecentlyActive,
            GlossarySelectionRule::HighFrequencyAnchor,
        ];

        let rules = self.glossary_rules.lock().ok();
        let rendered = RULES.map(|rule| {
            let counters = rules
                .as_ref()
                .and_then(|rules| rules.get(&rule))
                .copied()
                .unwrap_or_default();
            format!(
                "{} injected={} honored={}",
                rule.as_str(),
                counters.injected,
                counters.honored
            )
        });
        format!("glossary rules | {}", rendered.join(" | "))
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
    let timed_out = entries.iter().filter(|e| e.status == "timeout").count();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glossary_counters_increment_only_the_recorded_rule() {
        let rules = [
            GlossarySelectionRule::SegmentMatched,
            GlossarySelectionRule::AlwaysActive,
            GlossarySelectionRule::RecentlyActive,
            GlossarySelectionRule::HighFrequencyAnchor,
        ];

        for rule in rules {
            let telemetry = TelemetryLog::new();
            telemetry.record_glossary_entry(rule, false);
            assert_eq!(telemetry.glossary_rule_counts(rule), (1, 0));
            for other in rules {
                if other != rule {
                    assert_eq!(telemetry.glossary_rule_counts(other), (0, 0));
                }
            }
            telemetry.record_glossary_entry(rule, true);
            assert_eq!(telemetry.glossary_rule_counts(rule), (2, 1));
        }
    }
}
