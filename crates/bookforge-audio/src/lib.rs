//! Audiobook generation for BookForge.
//!
//! The pipeline mirrors the translation engine's separation of concerns:
//! deterministic Rust owns structure (chapter extraction, sentence-boundary
//! chunking, file layout, resume) and the model/provider only ever sees a
//! plain text chunk and returns audio bytes.
//!
//! - [`text`] turns a parsed [`bookforge_core::ir::Book`] into chapters and
//!   splits chapter prose into synthesis-sized chunks.
//! - [`provider`] defines [`provider::TtsProvider`] and ships native
//!   OpenAI-compatible, Gemini, and ElevenLabs clients plus a deterministic
//!   mock.
//! - [`builder`] orchestrates book -> chunks -> audio files with bounded
//!   concurrency, atomic writes, file-based resume, and a JSON manifest.
//! - `stitch` optionally joins the files per chapter and into an `.m4b`
//!   via `ffmpeg`, degrading gracefully when it is absent.

pub mod builder;
pub mod capabilities;
pub mod cleanup;
pub(crate) mod lock;
pub mod provider;
pub mod source;
pub mod stitch;
pub(crate) mod text;

pub use builder::{
    AudiobookManifest, AudiobookOptions, AudiobookReport, AudiobookStatus, BuildError,
    ChunkFailure, ChunkRecord, ChunkStatus, GapSettings, Progress, build_audiobook,
    failed_chunk_files, plan_chunks, plan_chunks_for_prune, validate_options,
};
pub use capabilities::{ProviderFeatureSet, feature_set, feature_set_for_id};
pub use cleanup::{StaleChunk, find_stale_chunks, remove_stale_chunks};
pub use provider::{
    AudioClip, AudioFormat, ElevenLabsModelResolution, ElevenLabsSubscription, ElevenLabsTtsConfig,
    ElevenLabsTtsProvider, ElevenLabsVoice, GeminiTtsConfig, GeminiTtsProvider, MockTtsProvider,
    OpenAiTtsConfig, OpenAiTtsProvider, SpeechRequest, TextNormalization, TtsError, TtsProvider,
    TtsProviderKind, elevenlabs_model_max_input_chars, fetch_elevenlabs_subscription,
    fetch_elevenlabs_subscription_with_key_and_cancel, list_elevenlabs_voices,
    list_elevenlabs_voices_with_cancel, resolve_preferred_elevenlabs_model_reported_with_cancel,
};
pub use source::{NarrationSource, NarrationSourceError, read_narration_source};
pub use stitch::{StitchOptions, StitchReport, ffmpeg_available, stitch};
pub use text::ChunkKind;
