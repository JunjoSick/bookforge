use std::sync::{Arc, Mutex};
use tokio::sync::Semaphore;

pub struct AdaptiveLimiter {
    state: Mutex<usize>,
    min: usize,
    max: usize,
    semaphore: Arc<Semaphore>,
}

impl AdaptiveLimiter {
    pub fn new(min: usize, max: usize) -> Self {
        let min = min.max(1);
        let max = max.max(min);
        Self {
            state: Mutex::new(min),
            min,
            max,
            semaphore: Arc::new(Semaphore::new(min)),
        }
    }

    pub fn current(&self) -> usize {
        *self.state.lock().unwrap()
    }

    pub fn semaphore(&self) -> Arc<Semaphore> {
        self.semaphore.clone()
    }

    pub fn on_success(&self) {
        self.update(|c| c + 1);
    }

    pub fn on_rate_limit(&self) {
        self.update(|c| c / 2);
    }

    pub fn on_timeout(&self) {
        self.update(|c| c * 3 / 4);
    }

    fn update<F: FnOnce(usize) -> usize>(&self, f: F) {
        let mut state = self.state.lock().unwrap();
        let new = f(*state).clamp(self.min, self.max);
        if new > *state {
            self.semaphore.add_permits(new - *state);
            *state = new;
        } else if new < *state {
            // Shrink lazily: forget permits as in-flight tasks release them.
            // Acquires queue FIFO with normal callers, so the pool drains to
            // the new size without blocking the caller.
            let delta = (*state - new) as u32;
            *state = new;
            let sem = self.semaphore.clone();
            tokio::spawn(async move {
                if let Ok(permit) = sem.acquire_many_owned(delta).await {
                    permit.forget();
                }
            });
        }
    }
}
