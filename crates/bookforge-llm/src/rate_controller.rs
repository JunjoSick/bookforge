use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use bookforge_core::ProgressSink;
use tokio::sync::AcquireError;

use crate::{AdaptiveLimiter, concurrency::AdaptivePermit};

#[derive(Debug, Clone)]
pub struct RateControllerConfig {
    pub min_concurrency: usize,
    pub max_concurrency: usize,
    pub target_p95_latency_ms: u64,
    pub increase_interval: Duration,
    pub decrease_interval: Duration,
    pub observation_window: usize,
    pub stable_success_threshold: f64,
    pub rate_limit_cut_factor: f64,
    pub timeout_cut_factor: f64,
    pub high_latency_cut_factor: f64,
}

impl RateControllerConfig {
    pub fn for_target(initial_concurrency: usize) -> Self {
        let min = 1;
        let max = (initial_concurrency.max(1) * 4).max(1);
        Self {
            min_concurrency: min,
            max_concurrency: max,
            target_p95_latency_ms: 30_000,
            increase_interval: Duration::from_secs(2),
            decrease_interval: Duration::from_secs(2),
            observation_window: 20,
            stable_success_threshold: 0.98,
            rate_limit_cut_factor: 0.50,
            timeout_cut_factor: 0.75,
            high_latency_cut_factor: 0.85,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestStatus {
    Ok,
    RateLimited,
    Timeout,
    ConnectError,
    InvalidJson,
    Truncated,
    OtherError,
}

#[derive(Debug, Clone)]
pub struct RequestObservation {
    pub status: RequestStatus,
    pub latency_ms: u64,
    pub timestamp: Instant,
}

struct RateControllerState {
    observations: VecDeque<RequestObservation>,
    last_increase: Instant,
    last_decrease: Instant,
}

pub struct ProviderRateController {
    limiter: Arc<AdaptiveLimiter>,
    state: Mutex<RateControllerState>,
    config: RateControllerConfig,
}

impl ProviderRateController {
    pub fn new(
        limiter: Arc<AdaptiveLimiter>,
        config: RateControllerConfig,
        _progress: Arc<dyn ProgressSink>,
    ) -> Self {
        let now = Instant::now();
        Self {
            limiter,
            state: Mutex::new(RateControllerState {
                observations: VecDeque::with_capacity(config.observation_window),
                last_increase: now.checked_sub(config.increase_interval).unwrap_or(now),
                last_decrease: now.checked_sub(config.decrease_interval).unwrap_or(now),
            }),
            config,
        }
    }

    pub fn with_limiter(limiter: Arc<AdaptiveLimiter>, config: RateControllerConfig) -> Self {
        Self::new(limiter, config, Arc::new(bookforge_core::NullProgressSink))
    }

    pub async fn acquire(&self) -> Result<AdaptivePermit, AcquireError> {
        self.limiter.acquire().await
    }

    pub fn current(&self) -> usize {
        self.limiter.current()
    }

    pub fn observe(&self, status: RequestStatus, latency_ms: u64) {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap();
        state.observations.push_back(RequestObservation {
            status,
            latency_ms,
            timestamp: now,
        });
        while state.observations.len() > self.config.observation_window {
            state.observations.pop_front();
        }

        match status {
            RequestStatus::RateLimited => {
                self.cut_locked(
                    &mut state,
                    self.config.rate_limit_cut_factor,
                    "rate_limited",
                );
            }
            RequestStatus::Timeout => {
                self.cut_locked(&mut state, self.config.timeout_cut_factor, "timeout");
            }
            RequestStatus::ConnectError => {
                self.cut_locked(&mut state, self.config.timeout_cut_factor, "connect_error");
            }
            RequestStatus::Ok => {
                if self.rolling_p95_locked(&state) > self.config.target_p95_latency_ms {
                    self.cut_locked(
                        &mut state,
                        self.config.high_latency_cut_factor,
                        "high_latency",
                    );
                } else {
                    self.grow_if_stable_locked(&mut state);
                }
            }
            RequestStatus::InvalidJson | RequestStatus::Truncated | RequestStatus::OtherError => {}
        }
    }

    fn cut_locked(&self, state: &mut RateControllerState, factor: f64, reason: &str) {
        let now = Instant::now();
        if now.duration_since(state.last_decrease) < self.config.decrease_interval {
            return;
        }
        state.last_decrease = now;
        let current = self.limiter.current();
        let target = ((current as f64) * factor).floor() as usize;
        self.limiter
            .set_target(target.max(self.config.min_concurrency), reason.to_string());
    }

    fn grow_if_stable_locked(&self, state: &mut RateControllerState) {
        let now = Instant::now();
        if now.duration_since(state.last_increase) < self.config.increase_interval {
            return;
        }
        if state.observations.len() < self.config.observation_window {
            return;
        }
        let ok = state
            .observations
            .iter()
            .filter(|obs| obs.status == RequestStatus::Ok)
            .count();
        let success_rate = ok as f64 / state.observations.len() as f64;
        if success_rate < self.config.stable_success_threshold {
            return;
        }
        if self.rolling_p95_locked(state) > self.config.target_p95_latency_ms {
            return;
        }
        state.last_increase = now;
        let target = (self.limiter.current() + 1).min(self.config.max_concurrency);
        self.limiter.set_target(target, "stable_success");
    }

    fn rolling_p95_locked(&self, state: &RateControllerState) -> u64 {
        let _latest = state.observations.back().map(|obs| obs.timestamp);
        let mut latencies = state
            .observations
            .iter()
            .filter(|obs| obs.status == RequestStatus::Ok)
            .map(|obs| obs.latency_ms)
            .collect::<Vec<_>>();
        if latencies.is_empty() {
            return 0;
        }
        latencies.sort_unstable();
        let idx = ((latencies.len() as f64) * 0.95).ceil() as usize;
        latencies[idx.saturating_sub(1).min(latencies.len() - 1)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn controller(initial: usize) -> ProviderRateController {
        let limiter = Arc::new(AdaptiveLimiter::new_with_bounds(
            initial,
            1,
            initial * 4,
            Duration::ZERO,
            None,
        ));
        ProviderRateController::with_limiter(
            limiter,
            RateControllerConfig {
                min_concurrency: 1,
                max_concurrency: initial * 4,
                target_p95_latency_ms: 1_000,
                increase_interval: Duration::ZERO,
                decrease_interval: Duration::from_secs(60),
                observation_window: 4,
                stable_success_threshold: 0.98,
                rate_limit_cut_factor: 0.50,
                timeout_cut_factor: 0.75,
                high_latency_cut_factor: 0.85,
            },
        )
    }

    #[test]
    fn rate_controller_halves_on_429() {
        let controller = controller(8);
        controller.observe(RequestStatus::RateLimited, 100);
        assert_eq!(controller.current(), 4);
    }

    #[test]
    fn rate_controller_reduces_on_timeout() {
        let controller = controller(8);
        controller.observe(RequestStatus::Timeout, 100);
        assert_eq!(controller.current(), 6);
    }

    #[test]
    fn rate_controller_reduces_on_high_p95() {
        let controller = controller(8);
        for _ in 0..4 {
            controller.observe(RequestStatus::Ok, 2_000);
        }
        assert_eq!(controller.current(), 6);
    }

    #[test]
    fn rate_controller_grows_slowly_after_stable_success() {
        let controller = controller(4);
        for _ in 0..4 {
            controller.observe(RequestStatus::Ok, 100);
        }
        assert_eq!(controller.current(), 5);
    }

    #[test]
    fn rate_controller_does_not_grow_on_every_success() {
        let controller = controller(4);
        controller.observe(RequestStatus::Ok, 100);
        controller.observe(RequestStatus::Ok, 100);
        controller.observe(RequestStatus::Ok, 100);
        assert_eq!(controller.current(), 4);
    }

    #[test]
    fn rate_controller_respects_min_max_concurrency() {
        let controller = controller(1);
        controller.observe(RequestStatus::RateLimited, 100);
        assert_eq!(controller.current(), 1);
        for _ in 0..20 {
            controller.observe(RequestStatus::Ok, 100);
        }
        assert_eq!(controller.current(), 4);
    }
}
