use std::sync::atomic::{AtomicUsize, Ordering};

pub struct AdaptiveLimiter {
    current: AtomicUsize,
    min: usize,
    max: usize,
}

impl AdaptiveLimiter {
    pub fn new(min: usize, max: usize) -> Self {
        let min = min.max(1);
        let max = max.max(min);
        Self {
            current: AtomicUsize::new(min),
            min,
            max,
        }
    }

    pub fn current(&self) -> usize {
        self.current.load(Ordering::Relaxed)
    }

    pub fn on_success(&self) {
        let _ = self.current.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |c| {
            Some((c + 1).min(self.max))
        });
    }

    pub fn on_rate_limit(&self) {
        let _ = self.current.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |c| {
            Some((c / 2).max(self.min))
        });
    }

    pub fn on_timeout(&self) {
        let _ = self.current.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |c| {
            Some((c * 3 / 4).max(self.min))
        });
    }
}
