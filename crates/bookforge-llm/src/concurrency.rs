use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};
use tokio::sync::{AcquireError, OwnedSemaphorePermit, Semaphore};

use bookforge_core::{ProgressEvent, ProgressSink};

pub struct AdaptiveLimiter {
    state: Mutex<usize>,
    min: usize,
    max: usize,
    semaphore: Arc<Semaphore>,
    permits_to_burn: Arc<AtomicUsize>,
    last_grow: Mutex<Option<Instant>>,
    grow_interval: Duration,
    progress: Option<Arc<dyn ProgressSink>>,
}

pub struct AdaptivePermit {
    permit: Option<OwnedSemaphorePermit>,
    permits_to_burn: Arc<AtomicUsize>,
}

impl Drop for AdaptivePermit {
    fn drop(&mut self) {
        // Atomically claim a burn slot iff one is pending. If none is
        // pending, fetch_update returns Err and we fall through to the
        // normal Drop, which releases the permit back into the pool.
        let claimed = self
            .permits_to_burn
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                if n > 0 { Some(n - 1) } else { None }
            })
            .is_ok();

        if claimed && let Some(permit) = self.permit.take() {
            permit.forget();
        }
    }
}

impl AdaptiveLimiter {
    pub fn new(min: usize, max: usize) -> Self {
        Self::new_with_progress(min, max, Duration::from_secs(2), None)
    }

    pub fn new_with_progress(
        min: usize,
        max: usize,
        grow_interval: Duration,
        progress: Option<Arc<dyn ProgressSink>>,
    ) -> Self {
        Self::new_with_bounds(min, min, max, grow_interval, progress)
    }

    pub fn new_with_bounds(
        initial: usize,
        min: usize,
        max: usize,
        grow_interval: Duration,
        progress: Option<Arc<dyn ProgressSink>>,
    ) -> Self {
        let initial = initial.max(1);
        let min = min.max(1);
        let max = max.max(min).max(initial);
        let initial = initial.clamp(min, max);
        Self {
            state: Mutex::new(initial),
            min,
            max,
            semaphore: Arc::new(Semaphore::new(initial)),
            permits_to_burn: Arc::new(AtomicUsize::new(0)),
            last_grow: Mutex::new(None),
            grow_interval,
            progress,
        }
    }

    pub fn current(&self) -> usize {
        *self.state.lock().unwrap()
    }

    pub async fn acquire(&self) -> Result<AdaptivePermit, AcquireError> {
        let permit = self.semaphore.clone().acquire_owned().await?;
        Ok(AdaptivePermit {
            permit: Some(permit),
            permits_to_burn: self.permits_to_burn.clone(),
        })
    }

    pub fn on_success(&self) {
        let now = Instant::now();
        let mut last = self.last_grow.lock().unwrap();
        if let Some(prev) = *last
            && now.duration_since(prev) < self.grow_interval
        {
            return;
        }
        *last = Some(now);
        drop(last);
        self.update("success", |c| c + 1);
    }

    pub fn on_rate_limit(&self) {
        self.update("rate_limited", |c| c / 2);
    }

    pub fn on_timeout(&self) {
        self.update("timeout", |c| c * 3 / 4);
    }

    pub fn on_p95_high(&self) {
        self.update("high_latency", |c| (c as f64 * 0.85) as usize);
    }

    pub fn set_target(&self, target: usize, reason: impl Into<String>) {
        let target = target.clamp(self.min, self.max);
        let reason = reason.into();
        self.update(&reason, |_| target);
    }

    fn update<F: FnOnce(usize) -> usize>(&self, reason: &str, f: F) {
        let mut state = self.state.lock().unwrap();
        let previous = *state;
        let new = f(*state).clamp(self.min, self.max);
        if new == previous {
            return;
        }

        if new > previous {
            // Cancel pending burns first so we don't add and immediately
            // burn the same permits.
            let mut remaining = new - previous;
            *state = new;
            while remaining > 0 {
                match self.permits_to_burn.fetch_update(
                    Ordering::AcqRel,
                    Ordering::Acquire,
                    |burn| {
                        if burn == 0 {
                            None
                        } else {
                            let cancel = burn.min(remaining);
                            Some(burn - cancel)
                        }
                    },
                ) {
                    Ok(prev_burn) => {
                        let cancelled = prev_burn.min(remaining);
                        remaining -= cancelled;
                    }
                    Err(_) => break,
                }
            }
            if remaining > 0 {
                self.semaphore.add_permits(remaining);
            }
        } else {
            let delta = previous - new;
            *state = new;
            self.permits_to_burn.fetch_add(delta, Ordering::AcqRel);
        }

        if let Some(ref progress) = self.progress {
            progress.emit(ProgressEvent::ConcurrencyChanged {
                previous,
                current: new,
                reason: reason.to_string(),
                timestamp_ms: bookforge_core::progress::now_ms(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    async fn acquire_n(limiter: &AdaptiveLimiter, n: usize) -> Vec<AdaptivePermit> {
        let mut permits = Vec::with_capacity(n);
        for _ in 0..n {
            permits.push(limiter.acquire().await.unwrap());
        }
        permits
    }

    #[tokio::test]
    async fn shrink_does_not_block_subsequent_acquires() {
        let limiter = AdaptiveLimiter::new(4, 8);
        let permits = acquire_n(&limiter, 4).await;

        // All 4 permits are held. Shrink to 2; this must NOT block on
        // an acquire_many — it should just bump the burn counter.
        limiter.on_rate_limit(); // 4 -> 2

        drop(permits);

        // After dropping all 4: 2 burns fire, 2 permits return to the pool.
        let p = timeout(Duration::from_millis(200), limiter.acquire())
            .await
            .expect("acquire should not block")
            .expect("acquire ok");
        drop(p);
    }

    #[tokio::test]
    async fn burn_counter_drains_to_target_after_shrink() {
        let limiter = AdaptiveLimiter::new_with_progress(1, 8, Duration::ZERO, None);
        // Grow to 4.
        limiter.on_success(); // 1 -> 2
        limiter.on_success(); // 2 -> 3
        limiter.on_success(); // 3 -> 4

        // Hold all 4.
        let permits = acquire_n(&limiter, 4).await;

        // Shrink to 1 — burn counter goes to 3.
        limiter.on_rate_limit(); // 4 -> 2
        limiter.on_rate_limit(); // 2 -> 1
        // total burned target = 4 - 1 = 3

        drop(permits);

        // Now only 1 permit should be available. Take it.
        let p1 = timeout(Duration::from_millis(200), limiter.acquire())
            .await
            .expect("first acquire ok")
            .expect("first acquire ok");

        // A second acquire should block until p1 is released.
        let res = timeout(Duration::from_millis(50), limiter.acquire()).await;
        assert!(res.is_err(), "second acquire should have blocked");

        drop(p1);
    }

    #[tokio::test]
    async fn drop_does_not_underflow_when_no_burn_pending() {
        let limiter = AdaptiveLimiter::new(2, 4);
        for _ in 0..10 {
            let p = limiter.acquire().await.unwrap();
            drop(p); // Should not underflow permits_to_burn (which stays at 0).
        }
        assert_eq!(limiter.permits_to_burn.load(Ordering::Acquire), 0);
        assert_eq!(limiter.current(), 2);
    }

    #[tokio::test]
    async fn grow_cancels_pending_burns_first() {
        let limiter = AdaptiveLimiter::new_with_progress(1, 8, Duration::ZERO, None);
        // 1 -> 4
        limiter.on_success();
        limiter.on_success();
        limiter.on_success();
        // Shrink 4 -> 1: burn counter = 3
        limiter.on_rate_limit();
        limiter.on_rate_limit();
        assert_eq!(limiter.permits_to_burn.load(Ordering::Acquire), 3);
        // Grow 1 -> 4: should cancel burns rather than add permits.
        limiter.on_success();
        limiter.on_success();
        limiter.on_success();
        assert_eq!(limiter.permits_to_burn.load(Ordering::Acquire), 0);
    }
}
