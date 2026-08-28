//! Latency-aware throughput model shared by batch planning and the provider.
//!
//! The production dogfood run (English→Italian book via deepseek-v4-flash)
//! requested adaptive batches with up to 23,544 `max_output_tokens` against a
//! 180s preset timeout. A legitimate generation that actually leans on that
//! budget needs far more than 180s at any realistic throughput, so requests
//! could only succeed by luck. Two coordinated mechanisms fix this without
//! guessing from wall-clock observation:
//!
//! 1. **Planning** caps a batch's output dimension so its expected generation
//!    time fits comfortably inside the configured timeout (see
//!    [`planning_output_token_cap`]); batches that cannot respect the cap are
//!    split by the normal planning machinery, while single-item batches keep
//!    their budget.
//! 2. **Provider** extends an individual request's timeout to
//!    `max(configured, expected_seconds(request.max_output_tokens) * 1.25 +
//!    30s)` via reqwest's per-request timeout override, so a slow but
//!    legitimate generation is given a *bounded, explained* window instead of
//!    being silently failed at the preset timeout.
//!
//! The extension is derived from the request's own output budget, not from
//! wall-clock observation, and it never falls below the user's configured
//! timeout. Rationale: silently failing a legit 6-minute generation is worse
//! than a bounded, explained extension — a timed-out long generation costs the
//! full attempt plus retries, while an honest extension costs nothing when the
//! generation finishes early.
//!
//! The same model intentionally makes planning and the provider agree: if
//! planning did its job, most requests finish well inside the configured
//! timeout and the extension never engages; when it does engage (single-item
//! batches, escalated retry budgets), it is proportional to the request.

use std::time::Duration;

/// Conservative floor for provider generation throughput, in output tokens
/// per second.
///
/// Justification for the 30 tok/s constant:
/// * DeepSeek / OpenRouter-class chat endpoints commonly sustain 50–150 tok/s
///   for mid-size completions; 30 tok/s is a deliberately pessimistic floor
///   that still covers free-tier and shared gateways, long prompt prefill
///   interleaving with other tenants, and provider-side batching jitter seen
///   during the production dogfood run.
/// * Overestimating throughput underestimates generation time, which is
///   exactly the failure mode this module exists to fix: a legitimate
///   generation gets cut off by a timeout sized for optimistic throughput.
/// * Underestimating throughput only widens the safety margin: planning splits
///   somewhat more aggressively and the timeout extension is somewhat more
///   generous — both bounded and observable, never incorrect.
///
/// Keep this constant conservative: it is a *floor*, not an average.
pub const MIN_PROVIDER_TOKENS_PER_SECOND: f64 = 30.0;

/// Planning keeps a request's expected generation time at or under this share
/// of the effective timeout. The remaining 20% absorbs prompt prefill, network
/// overhead, and scheduling delay without tripping the timeout.
pub const PLANNING_TIMEOUT_SHARE: f64 = 0.8;

/// Safety multiplier applied to the expected generation time when extending a
/// single request's timeout. Throughput varies run to run; 25% headroom on top
/// of an already-conservative throughput floor keeps the extension honest.
pub const REQUEST_TIMEOUT_HEADROOM: f64 = 1.25;

/// Fixed seconds added to the extended timeout regardless of output size.
/// Covers connection setup, prompt prefill (which scales with the *input*,
/// unknowable here), and provider queueing that the token-rate model cannot
/// see. This base also means the extension floor is never below ~30s, so tiny
/// requests never get a *shorter* window than a patient generation needs.
pub const REQUEST_TIMEOUT_OVERHEAD_SECONDS: f64 = 30.0;

/// Expected wall-clock seconds to generate `max_output_tokens` output tokens
/// at the conservative floor throughput.
pub fn expected_generation_seconds(max_output_tokens: u32) -> f64 {
    f64::from(max_output_tokens) / MIN_PROVIDER_TOKENS_PER_SECOND
}

/// Per-request effective timeout: `max(configured, expected_seconds(output) *
/// 1.25 + 30s)`.
///
/// Returns the timeout to apply to one request plus whether the extension
/// engaged (i.e. the derived window exceeded the configured timeout). Requests
/// without an output budget (`None`) cannot be modeled, so they keep the
/// configured timeout exactly.
pub fn effective_request_timeout(
    configured: Duration,
    max_output_tokens: Option<u32>,
) -> (Duration, bool) {
    let Some(max_output_tokens) = max_output_tokens else {
        return (configured, false);
    };
    let derived = expected_generation_seconds(max_output_tokens) * REQUEST_TIMEOUT_HEADROOM
        + REQUEST_TIMEOUT_OVERHEAD_SECONDS;
    let derived = Duration::from_secs_f64(derived);
    if derived > configured {
        (derived, true)
    } else {
        (configured, false)
    }
}

/// Integer-seconds variant of [`effective_request_timeout`] for progress
/// events. Returns `None` when no output budget is known (mirroring the
/// event's optional field).
pub fn effective_request_timeout_seconds(
    configured_seconds: u64,
    max_output_tokens: Option<u32>,
) -> Option<u64> {
    max_output_tokens.map(|tokens| {
        effective_request_timeout(Duration::from_secs(configured_seconds), Some(tokens))
            .0
            .as_secs()
    })
}

/// Largest output budget whose expected generation time stays within
/// [`PLANNING_TIMEOUT_SHARE`] of the configured timeout. Batch planning treats
/// this as an additional output-dimension constraint: a batch whose expected
/// output exceeds it is split, while single-item batches (which cannot split)
/// keep their budget and rely on the provider-side timeout extension.
pub fn planning_output_token_cap(configured_timeout_seconds: u64) -> u32 {
    let cap = f64::from(u32::try_from(configured_timeout_seconds).unwrap_or(u32::MAX))
        * PLANNING_TIMEOUT_SHARE
        * MIN_PROVIDER_TOKENS_PER_SECOND;
    // Floor at 1 so a pathologically tiny configured timeout degrades to "no
    // headroom" instead of a zero/negative cap that would break clamping.
    (cap.floor() as u64).max(1) as u32
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expected_generation_uses_the_documented_floor_throughput() {
        // 30 tok/s: 600 tokens take 20s.
        assert!((expected_generation_seconds(600) - 20.0).abs() < 1e-9);
        // The dogfood worst case: 23,544 tokens take ~785s (13 minutes).
        assert!((expected_generation_seconds(23_544) - 784.8).abs() < 1e-9);
    }

    #[test]
    fn effective_timeout_extends_with_the_output_budget() {
        let (timeout, extended) = effective_request_timeout(Duration::from_secs(120), Some(23_544));
        // 784.8s * 1.25 + 30s = 1011s.
        assert_eq!(timeout, Duration::from_secs_f64(1011.0));
        assert!(extended);
    }

    #[test]
    fn effective_timeout_never_reduces_the_configured_timeout() {
        // A tiny budget cannot shrink a long configured timeout...
        let (timeout, extended) = effective_request_timeout(Duration::from_secs(600), Some(64));
        assert_eq!(timeout, Duration::from_secs(600));
        assert!(!extended);
        // ...and a missing budget keeps the configured timeout exactly.
        let (timeout, extended) = effective_request_timeout(Duration::from_secs(600), None);
        assert_eq!(timeout, Duration::from_secs(600));
        assert!(!extended);
    }

    #[test]
    fn effective_timeout_seconds_mirrors_the_duration_variant() {
        assert_eq!(
            effective_request_timeout_seconds(180, Some(23_544)),
            Some(1011)
        );
        assert_eq!(effective_request_timeout_seconds(180, None), None);
    }

    #[test]
    fn planning_cap_derives_from_the_configured_timeout() {
        // 0.8 * 180s * 30 tok/s = 4,320 tokens.
        assert_eq!(planning_output_token_cap(180), 4_320);
        assert_eq!(planning_output_token_cap(600), 14_400);
        // Degenerate timeouts still yield a positive cap.
        assert_eq!(planning_output_token_cap(0), 1);
    }
}
