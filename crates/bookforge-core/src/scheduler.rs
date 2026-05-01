#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    pub concurrency: usize,
    pub max_retries: usize,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            concurrency: 4,
            max_retries: 3,
        }
    }
}
