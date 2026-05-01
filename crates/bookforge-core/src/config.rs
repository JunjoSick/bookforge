use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct TranslationConfig {
    pub source_language: Option<String>,
    pub target_language: String,
    pub provider: String,
    pub model: Option<String>,
    pub concurrency: usize,
    pub max_attempts: usize,
    pub output: PathBuf,
}

#[derive(Debug, Clone)]
pub struct SegmentationConfig {
    pub max_segment_tokens: usize,
    pub context_tokens: usize,
}

impl Default for SegmentationConfig {
    fn default() -> Self {
        Self {
            max_segment_tokens: 1_200,
            context_tokens: 160,
        }
    }
}
