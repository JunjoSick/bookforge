use std::path::PathBuf;

use bookforge_core::{
    GlossaryFormat, JsonMode, ProviderPreset,
    config::{DoubleCheckMode, FallbackScope, TranslationProfile},
};
use clap::Args;

use crate::{LanguageArgs, ProviderArgs as CliProviderArgs, QaMode, progress::UiMode};

#[derive(Debug, Args)]
pub struct TranslateArgs {
    pub input: PathBuf,

    #[command(flatten)]
    pub language: LanguageArgs,

    #[command(flatten)]
    pub provider: CliProviderArgs,

    #[arg(long, value_enum, default_value_t = TranslationProfile::Balanced)]
    pub profile: TranslationProfile,

    #[arg(long)]
    pub max_segment_tokens: Option<usize>,

    #[arg(long)]
    pub context_tokens: Option<usize>,

    #[arg(long)]
    pub batch_target_tokens: Option<usize>,

    #[arg(long)]
    pub batch_max_items: Option<usize>,

    #[arg(long)]
    pub compact_prompts: Option<bool>,

    #[arg(long)]
    pub retry_failed_only: Option<bool>,

    #[arg(long)]
    pub adaptive_concurrency: Option<bool>,

    #[arg(long)]
    pub turbo_text_only: bool,

    #[arg(long)]
    pub concurrency: Option<usize>,

    #[arg(long)]
    pub max_attempts: Option<usize>,

    #[arg(long)]
    pub provider_max_attempts: Option<usize>,

    #[arg(long)]
    pub validation_max_attempts: Option<usize>,

    #[arg(long)]
    pub out: Option<PathBuf>,

    #[arg(long)]
    pub book_id: Option<String>,

    #[arg(long)]
    pub series_id: Option<String>,

    #[arg(long = "glossary")]
    pub glossary: Vec<PathBuf>,

    #[arg(long, default_value_t = 800)]
    pub glossary_budget_tokens: usize,

    #[arg(long, value_enum, default_value_t = GlossaryFormat::Json)]
    pub glossary_format: GlossaryFormat,

    #[arg(long)]
    pub prompt_extra: Option<String>,

    #[arg(long, value_enum, default_value_t = QaMode::Off)]
    pub qa: QaMode,

    #[arg(long, default_value_t = 8)]
    pub qa_concurrency: usize,

    #[arg(long)]
    pub qa_batch_target_tokens: Option<usize>,

    #[arg(long)]
    pub qa_model: Option<String>,

    #[arg(long)]
    pub qa_provider: Option<String>,

    #[arg(long)]
    pub qa_base_url: Option<String>,

    #[arg(long)]
    pub qa_api_key_env: Option<String>,

    #[arg(long, value_enum, default_value_t = DoubleCheckMode::Off)]
    pub double_check: DoubleCheckMode,

    #[arg(long)]
    pub double_check_model: Option<String>,

    #[arg(long)]
    pub double_check_provider: Option<String>,

    #[arg(long)]
    pub double_check_base_url: Option<String>,

    #[arg(long)]
    pub double_check_api_key_env: Option<String>,

    #[arg(long, default_value_t = 4)]
    pub double_check_concurrency: usize,

    #[arg(long)]
    pub double_check_batch_target_tokens: Option<usize>,

    #[arg(long, default_value_t = false)]
    pub auto_correct: bool,

    #[arg(long, default_value_t = 1)]
    pub correction_rounds: usize,

    #[arg(long)]
    pub fallback_provider: Option<String>,

    #[arg(long)]
    pub fallback_model: Option<String>,

    #[arg(long)]
    pub fallback_base_url: Option<String>,

    #[arg(long)]
    pub fallback_api_key_env: Option<String>,

    #[arg(long, value_enum, default_value_t = FallbackScope::Failed)]
    pub fallback_only: FallbackScope,

    #[arg(long, default_value_t = false)]
    pub no_thinking: bool,

    #[arg(long)]
    pub model_context_tokens: Option<u32>,

    #[arg(long)]
    pub max_output_tokens: Option<u32>,

    #[arg(long)]
    pub batch_max_output_tokens: Option<u32>,

    #[arg(long, value_enum, default_value_t = JsonMode::Auto)]
    pub json_mode: JsonMode,

    #[arg(long, value_enum, default_value_t = UiMode::Auto)]
    pub ui: UiMode,

    #[arg(long)]
    pub progress_jsonl: Option<PathBuf>,

    #[arg(long, value_enum)]
    pub provider_preset: Option<ProviderPreset>,
}
