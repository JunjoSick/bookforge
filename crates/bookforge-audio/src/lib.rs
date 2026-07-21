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
//! - [`stitch`] optionally joins the files per chapter and into an `.m4b`
//!   via `ffmpeg`, degrading gracefully when it is absent.

pub mod builder;
pub mod cleanup;
pub mod provider;
pub mod stitch;
pub mod text;

pub use builder::{
    AudiobookManifest, AudiobookOptions, AudiobookReport, AudiobookStatus, BuildError, ChunkRecord,
    ChunkStatus, GapSettings, Progress, build_audiobook, plan_chunks, validate_options,
};
pub use cleanup::{StaleChunk, find_stale_chunks, remove_stale_chunks};
pub use provider::{
    AudioClip, AudioFormat, ELEVENLABS_MAX_INPUT_CHARS, ELEVENLABS_PREFERRED_MODELS,
    ElevenLabsSubscription, ElevenLabsTtsConfig, ElevenLabsTtsProvider, ElevenLabsVoice,
    GeminiTtsConfig, GeminiTtsProvider, MockTtsProvider, OpenAiTtsConfig, OpenAiTtsProvider,
    SpeechRequest, TextNormalization, TtsError, TtsProvider, elevenlabs_model_max_input_chars,
    fetch_elevenlabs_subscription, fetch_elevenlabs_subscription_with_key, list_elevenlabs_voices,
    resolve_preferred_elevenlabs_model,
};
pub use stitch::{StitchOptions, StitchReport, ffmpeg_available, single_file_ffmpeg_args, stitch};
pub use text::{
    Chapter, ChunkKind, NarrationBlock, NarrationBlockKind, NarrationChunk, chapters_from_book,
    chunk_blocks, chunk_text,
};
