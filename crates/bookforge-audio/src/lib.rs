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
    ChunkStatus, Progress, build_audiobook, plan_chunks, validate_options,
};
pub use cleanup::{StaleChunk, find_stale_chunks, remove_stale_chunks};
pub use provider::{
    AudioClip, AudioFormat, ELEVENLABS_MAX_INPUT_CHARS, ElevenLabsTtsConfig, ElevenLabsTtsProvider,
    GeminiTtsConfig, GeminiTtsProvider, MockTtsProvider, OpenAiTtsConfig, OpenAiTtsProvider,
    SpeechRequest, TtsError, TtsProvider, elevenlabs_model_max_input_chars,
};
pub use stitch::{StitchOptions, StitchReport, ffmpeg_available, stitch};
pub use text::{Chapter, chapters_from_book, chunk_text};
