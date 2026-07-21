//! Audiobook assembly: book -> chapters -> chunks -> audio files.
//!
//! The plan is a pure function of the book and options, so a run that is
//! interrupted can be re-invoked and will skip every chunk whose file is
//! already on disk. A `manifest.json` records the plan and outcomes for the
//! operator and for the optional stitch step.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use bookforge_core::ir::Book;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::provider::{AudioFormat, SpeechRequest, TextNormalization, TtsProvider};
use crate::text::{ChunkKind, chapters_from_book_with_options, chunk_blocks};

/// Knobs for a single audiobook build.
#[derive(Debug, Clone)]
pub struct AudiobookOptions {
    pub out_dir: PathBuf,
    pub voice: String,
    pub format: AudioFormat,
    pub speed: f32,
    /// Maximum characters per synthesis request.
    pub max_chars: usize,
    /// Number of chunks synthesized in parallel.
    pub concurrency: usize,
    /// Stable, non-secret identity for the synthesis backend and model. It is
    /// included in chunk hashes so changing models never reuses stale audio.
    pub synthesis_id: String,
    /// Optional voice delivery/pronunciation guidance.
    pub instructions: Option<String>,
    /// Neighboring chunk context sent to providers that support continuity.
    /// Zero disables context entirely.
    pub context_chars: usize,
    pub seed: Option<u32>,
    pub language_code: Option<String>,
    pub text_normalization: Option<TextNormalization>,
    pub heading_break_tag: Option<String>,
    pub chapter_filter: Option<std::collections::BTreeSet<usize>>,
    /// Group physical pdftohtml pages around explicit chapter headings.
    /// This must only be enabled from a positive source-format signal.
    pub pdf_page_grouping: bool,
}

impl Default for AudiobookOptions {
    fn default() -> Self {
        Self {
            out_dir: PathBuf::from("audiobook"),
            voice: "alloy".to_string(),
            format: AudioFormat::Wav,
            speed: 1.0,
            max_chars: 2_000,
            concurrency: 4,
            synthesis_id: "default".to_string(),
            instructions: None,
            context_chars: 300,
            seed: None,
            language_code: None,
            text_normalization: None,
            heading_break_tag: None,
            chapter_filter: None,
            pdf_page_grouping: false,
        }
    }
}

/// One synthesized (or to-be-synthesized) audio file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkRecord {
    pub chapter_index: usize,
    pub chapter_title: String,
    pub part: usize,
    #[serde(default)]
    pub kind: ChunkKind,
    pub file: String,
    pub chars: usize,
    /// Hash of text plus every synthesis setting that can alter the audio.
    pub synthesis_sha256: String,
    #[serde(default)]
    pub status: ChunkStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChunkStatus {
    #[default]
    Pending,
    Synthesized,
    Cached,
    Failed,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudiobookStatus {
    #[default]
    Planned,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

/// Persisted plan + outcome, written to `manifest.json` in the output dir.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudiobookManifest {
    pub schema_version: u32,
    pub title: Option<String>,
    pub synthesis_id: String,
    pub voice: String,
    pub format: String,
    pub speed: f32,
    pub max_chars: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_normalization: Option<TextNormalization>,
    #[serde(default)]
    pub gaps: GapSettings,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    pub chapters: usize,
    pub chunks: Vec<ChunkRecord>,
    #[serde(default)]
    pub completed_chunks: usize,
    #[serde(default)]
    pub status: AudiobookStatus,
    #[serde(default)]
    pub updated_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GapSettings {
    pub chapter_ms: u32,
    pub title_ms: u32,
    pub paragraph_ms: u32,
}

/// Summary returned to the caller after a build.
#[derive(Debug, Clone)]
pub struct AudiobookReport {
    pub chapters: usize,
    pub chunks_total: usize,
    pub chunks_synthesized: usize,
    pub chunks_skipped: usize,
    pub files: Vec<PathBuf>,
    pub manifest_path: PathBuf,
    pub out_dir: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("book has no narratable text; nothing to synthesize")]
    NoText,

    #[error("invalid audiobook options: {0}")]
    InvalidOptions(String),

    #[error("provider returned {actual} audio for a {requested} request")]
    FormatMismatch {
        requested: &'static str,
        actual: &'static str,
    },

    #[error(transparent)]
    Tts(#[from] crate::provider::TtsError),

    #[error("failed to serialize manifest: {0}")]
    Manifest(#[from] serde_json::Error),
}

type Result<T> = std::result::Result<T, BuildError>;

/// Progress notification emitted after each chunk is resolved.
#[derive(Debug, Clone)]
pub struct Progress {
    pub done: usize,
    pub total: usize,
    pub chapter_title: String,
    pub skipped: bool,
}

/// Internal per-chunk plan item.
struct PlannedChunk {
    chapter_index: usize,
    chapter_title: String,
    part: usize,
    kind: ChunkKind,
    text: String,
    previous_text: Option<String>,
    next_text: Option<String>,
    synthesis_sha256: String,
    path: PathBuf,
}

/// Build the deterministic chunk plan for a book. Public so the CLI can
/// preview counts (e.g. a cost estimate) without synthesizing.
pub fn plan_chunks(book: &Book, options: &AudiobookOptions) -> Vec<ChunkRecord> {
    build_plan(book, options)
        .into_iter()
        .map(|chunk| ChunkRecord {
            chapter_index: chunk.chapter_index,
            chapter_title: chunk.chapter_title,
            part: chunk.part,
            kind: chunk.kind,
            file: chunk
                .path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            chars: chunk.text.chars().count(),
            synthesis_sha256: chunk.synthesis_sha256,
            status: ChunkStatus::Pending,
            audio_sha256: None,
            bytes: None,
            error: None,
        })
        .collect()
}

fn build_plan(book: &Book, options: &AudiobookOptions) -> Vec<PlannedChunk> {
    let ext = options.format.extension();
    let mut planned = Vec::new();
    for chapter in chapters_from_book_with_options(book, options.pdf_page_grouping) {
        if chapter.is_empty() {
            continue;
        }
        if options
            .chapter_filter
            .as_ref()
            .is_some_and(|filter| !filter.contains(&(chapter.index + 1)))
        {
            continue;
        }
        let chunks = chunk_blocks(&chapter.blocks, options.max_chars);
        for (part_idx, mut chunk) in chunks.into_iter().enumerate() {
            if matches!(chunk.kind, ChunkKind::Title | ChunkKind::Heading)
                && let Some(tag) = options.heading_break_tag.as_deref()
                && !tag.is_empty()
            {
                chunk.text.push(' ');
                chunk.text.push_str(tag);
            }
            planned.push(PlannedChunk {
                chapter_index: chapter.index,
                chapter_title: chapter.title.clone(),
                part: part_idx + 1,
                kind: chunk.kind,
                text: chunk.text,
                previous_text: None,
                next_text: None,
                synthesis_sha256: String::new(),
                path: PathBuf::new(),
            });
        }
    }

    for index in 0..planned.len() {
        let previous_text = index.checked_sub(1).and_then(|previous| {
            (planned[previous].chapter_index == planned[index].chapter_index)
                .then(|| context_slice(&planned[previous].text, options.context_chars, true))
                .flatten()
        });
        let next_text = planned.get(index + 1).and_then(|next| {
            (next.chapter_index == planned[index].chapter_index)
                .then(|| context_slice(&next.text, options.context_chars, false))
                .flatten()
        });
        let synthesis_sha256 = synthesis_hash(
            &planned[index].text,
            planned[index].kind,
            previous_text.as_deref(),
            next_text.as_deref(),
            options,
        );
        let file_name = format!(
            "chapter-{:03}-part-{:03}-{}.{ext}",
            planned[index].chapter_index + 1,
            planned[index].part,
            &synthesis_sha256[..16]
        );
        planned[index].previous_text = previous_text;
        planned[index].next_text = next_text;
        planned[index].synthesis_sha256 = synthesis_sha256;
        planned[index].path = options.out_dir.join(file_name);
    }
    planned
}

fn context_slice(text: &str, chars: usize, from_end: bool) -> Option<String> {
    if chars == 0 || text.is_empty() {
        return None;
    }
    if from_end {
        let mut slice = text.chars().rev().take(chars).collect::<Vec<_>>();
        slice.reverse();
        Some(slice.into_iter().collect())
    } else {
        Some(text.chars().take(chars).collect())
    }
}

/// Synthesize an audiobook. Existing non-empty files are treated as already
/// done (resume). `on_progress` is invoked once per chunk, in completion
/// order.
pub async fn build_audiobook<P, F>(
    book: &Book,
    provider: Arc<P>,
    options: &AudiobookOptions,
    cancel: CancellationToken,
    on_progress: F,
) -> Result<AudiobookReport>
where
    P: TtsProvider + 'static,
    F: Fn(Progress) + Send + Sync + 'static,
{
    validate_options(options)?;
    let plan = build_plan(book, options);
    if plan.is_empty() {
        return Err(BuildError::NoText);
    }

    std::fs::create_dir_all(&options.out_dir).map_err(|source| BuildError::Io {
        path: options.out_dir.clone(),
        source,
    })?;

    let total = plan.len();
    let done = Arc::new(AtomicUsize::new(0));
    let synthesized = Arc::new(AtomicUsize::new(0));
    let skipped = Arc::new(AtomicUsize::new(0));
    let semaphore = Arc::new(Semaphore::new(options.concurrency.max(1)));
    let on_progress = Arc::new(on_progress);

    // Chunk records for the manifest, in plan order.
    let records: Vec<ChunkRecord> = plan
        .iter()
        .map(|chunk| ChunkRecord {
            chapter_index: chunk.chapter_index,
            chapter_title: chunk.chapter_title.clone(),
            part: chunk.part,
            kind: chunk.kind,
            file: file_name_of(&chunk.path),
            chars: chunk.text.chars().count(),
            synthesis_sha256: chunk.synthesis_sha256.clone(),
            status: ChunkStatus::Pending,
            audio_sha256: None,
            bytes: None,
            error: None,
        })
        .collect();

    let chapters = records
        .iter()
        .map(|record| record.chapter_index)
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    let manifest_path = options.out_dir.join("manifest.json");
    let manifest = Arc::new(Mutex::new(AudiobookManifest {
        schema_version: 3,
        title: book.metadata.title.clone(),
        synthesis_id: options.synthesis_id.clone(),
        voice: options.voice.clone(),
        format: options.format.extension().to_string(),
        speed: options.speed,
        max_chars: options.max_chars,
        instructions: options.instructions.clone(),
        seed: options.seed,
        language: options.language_code.clone(),
        text_normalization: options.text_normalization,
        gaps: GapSettings {
            chapter_ms: 1_200,
            title_ms: 800,
            paragraph_ms: 0,
        },
        author: (!book.metadata.creators.is_empty()).then(|| book.metadata.creators.join(", ")),
        chapters,
        chunks: records,
        completed_chunks: 0,
        status: AudiobookStatus::Running,
        updated_at_ms: now_ms(),
        error: None,
    }));
    write_manifest_checkpoint(&manifest, &manifest_path)?;

    let mut set = tokio::task::JoinSet::new();
    for (record_index, chunk) in plan.into_iter().enumerate() {
        let provider = Arc::clone(&provider);
        let semaphore = Arc::clone(&semaphore);
        let cancel = cancel.clone();
        let voice = options.voice.clone();
        let format = options.format;
        let speed = options.speed;
        let instructions = options.instructions.clone();
        let seed = options.seed;
        let language_code = options.language_code.clone();
        let text_normalization = options.text_normalization;
        let done = Arc::clone(&done);
        let synthesized = Arc::clone(&synthesized);
        let skipped = Arc::clone(&skipped);
        let on_progress = Arc::clone(&on_progress);
        let manifest = Arc::clone(&manifest);
        let manifest_path = manifest_path.clone();

        set.spawn(async move {
            let _permit = semaphore
                .acquire()
                .await
                .expect("semaphore is never closed");
            if cancel.is_cancelled() {
                return Err(BuildError::Tts(crate::provider::TtsError::Cancelled));
            }

            let rendered = async {
                if let Some(bytes) = read_valid_cached_audio(&chunk.path, format) {
                    skipped.fetch_add(1, Ordering::Relaxed);
                    return Ok::<_, BuildError>((true, bytes));
                }
                let clip = provider
                    .synthesize(SpeechRequest {
                        text: chunk.text.clone(),
                        voice: voice.clone(),
                        format,
                        speed,
                        instructions,
                        previous_text: chunk.previous_text.clone(),
                        next_text: chunk.next_text.clone(),
                        seed,
                        language_code,
                        text_normalization,
                    })
                    .await?;
                if clip.format != format {
                    return Err(BuildError::FormatMismatch {
                        requested: format.extension(),
                        actual: clip.format.extension(),
                    });
                }
                crate::provider::validate_audio_payload(format, None, &clip.bytes)?;
                write_atomic(&chunk.path, &clip.bytes)?;
                synthesized.fetch_add(1, Ordering::Relaxed);
                Ok((false, clip.bytes))
            }
            .await;

            let (was_skipped, audio_bytes) = match rendered {
                Ok(rendered) => rendered,
                Err(error) => {
                    let _ = update_chunk_checkpoint(
                        &manifest,
                        &manifest_path,
                        record_index,
                        ChunkStatus::Failed,
                        None,
                        None,
                        Some(error.to_string()),
                    );
                    return Err(error);
                }
            };
            update_chunk_checkpoint(
                &manifest,
                &manifest_path,
                record_index,
                if was_skipped {
                    ChunkStatus::Cached
                } else {
                    ChunkStatus::Synthesized
                },
                Some(audio_hash(&audio_bytes)),
                Some(audio_bytes.len() as u64),
                None,
            )?;

            let done_now = done.fetch_add(1, Ordering::Relaxed) + 1;
            on_progress(Progress {
                done: done_now,
                total,
                chapter_title: chunk.chapter_title.clone(),
                skipped: was_skipped,
            });
            Ok::<PathBuf, BuildError>(chunk.path)
        });
    }

    // Collect results as tasks finish; on the first error, cancel the run so
    // in-flight provider calls stop, abort the rest, and propagate.
    let mut files = Vec::with_capacity(total);
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(Ok(path)) => files.push(path),
            Ok(Err(error)) => {
                cancel.cancel();
                set.abort_all();
                let status =
                    if matches!(error, BuildError::Tts(crate::provider::TtsError::Cancelled)) {
                        AudiobookStatus::Cancelled
                    } else {
                        AudiobookStatus::Failed
                    };
                let _ = update_manifest_status(
                    &manifest,
                    &manifest_path,
                    status,
                    Some(error.to_string()),
                );
                return Err(error);
            }
            Err(join_error) => {
                cancel.cancel();
                set.abort_all();
                let error = BuildError::Tts(crate::provider::TtsError::Provider(format!(
                    "synthesis task panicked: {join_error}"
                )));
                let _ = update_manifest_status(
                    &manifest,
                    &manifest_path,
                    AudiobookStatus::Failed,
                    Some(error.to_string()),
                );
                return Err(error);
            }
        }
    }
    files.sort();

    update_manifest_status(&manifest, &manifest_path, AudiobookStatus::Succeeded, None)?;

    Ok(AudiobookReport {
        chapters,
        chunks_total: total,
        chunks_synthesized: synthesized.load(Ordering::Relaxed),
        chunks_skipped: skipped.load(Ordering::Relaxed),
        files,
        manifest_path,
        out_dir: options.out_dir.clone(),
    })
}

fn read_valid_cached_audio(path: &Path, format: AudioFormat) -> Option<Vec<u8>> {
    let bytes = std::fs::read(path).ok()?;
    crate::provider::validate_audio_payload(format, None, &bytes)
        .ok()
        .map(|()| bytes)
}

#[allow(clippy::too_many_arguments)]
fn update_chunk_checkpoint(
    manifest: &Arc<Mutex<AudiobookManifest>>,
    path: &Path,
    index: usize,
    status: ChunkStatus,
    audio_sha256: Option<String>,
    bytes: Option<u64>,
    error: Option<String>,
) -> Result<()> {
    {
        let mut manifest = manifest.lock().map_err(|_| {
            BuildError::InvalidOptions("audiobook manifest lock was poisoned".to_string())
        })?;
        let record = manifest.chunks.get_mut(index).ok_or_else(|| {
            BuildError::InvalidOptions("audiobook checkpoint index was invalid".to_string())
        })?;
        record.status = status;
        record.audio_sha256 = audio_sha256;
        record.bytes = bytes;
        record.error = error;
        manifest.completed_chunks = manifest
            .chunks
            .iter()
            .filter(|record| {
                matches!(
                    record.status,
                    ChunkStatus::Synthesized | ChunkStatus::Cached
                )
            })
            .count();
        manifest.updated_at_ms = now_ms();
    }
    write_manifest_checkpoint(manifest, path)
}

fn update_manifest_status(
    manifest: &Arc<Mutex<AudiobookManifest>>,
    path: &Path,
    status: AudiobookStatus,
    error: Option<String>,
) -> Result<()> {
    {
        let mut manifest = manifest.lock().map_err(|_| {
            BuildError::InvalidOptions("audiobook manifest lock was poisoned".to_string())
        })?;
        manifest.status = status;
        manifest.error = error;
        manifest.updated_at_ms = now_ms();
    }
    write_manifest_checkpoint(manifest, path)
}

fn write_manifest_checkpoint(manifest: &Arc<Mutex<AudiobookManifest>>, path: &Path) -> Result<()> {
    let manifest = manifest.lock().map_err(|_| {
        BuildError::InvalidOptions("audiobook manifest lock was poisoned".to_string())
    })?;
    let json = serde_json::to_vec_pretty(&*manifest)?;
    write_atomic(path, &json)
}

fn audio_hash(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub fn validate_options(options: &AudiobookOptions) -> Result<()> {
    if options.voice.trim().is_empty() {
        return Err(BuildError::InvalidOptions(
            "voice cannot be empty".to_string(),
        ));
    }
    if options.synthesis_id.trim().is_empty() {
        return Err(BuildError::InvalidOptions(
            "synthesis identity cannot be empty".to_string(),
        ));
    }
    if options.max_chars == 0 {
        return Err(BuildError::InvalidOptions(
            "max_chars must be greater than zero".to_string(),
        ));
    }
    if options.concurrency == 0 {
        return Err(BuildError::InvalidOptions(
            "concurrency must be greater than zero".to_string(),
        ));
    }
    if !options.speed.is_finite() || !(0.25..=4.0).contains(&options.speed) {
        return Err(BuildError::InvalidOptions(
            "speed must be between 0.25 and 4.0".to_string(),
        ));
    }
    Ok(())
}

fn synthesis_hash(
    text: &str,
    kind: ChunkKind,
    previous_text: Option<&str>,
    next_text: Option<&str>,
    options: &AudiobookOptions,
) -> String {
    synthesis_hash_with_version(
        "bookforge-audio-v2",
        text,
        kind,
        previous_text,
        next_text,
        options,
    )
}

fn synthesis_hash_with_version(
    version: &str,
    text: &str,
    kind: ChunkKind,
    previous_text: Option<&str>,
    next_text: Option<&str>,
    options: &AudiobookOptions,
) -> String {
    let mut hasher = Sha256::new();
    let seed = options
        .seed
        .map(|seed| seed.to_string())
        .unwrap_or_default();
    for value in [
        version,
        &options.synthesis_id,
        &options.voice,
        options.format.as_api_str(),
        &options.speed.to_bits().to_string(),
        options.instructions.as_deref().unwrap_or(""),
        text,
        seed.as_str(),
        options.language_code.as_deref().unwrap_or(""),
        options
            .text_normalization
            .map(TextNormalization::as_str)
            .unwrap_or(""),
        previous_text.unwrap_or(""),
        next_text.unwrap_or(""),
        match kind {
            ChunkKind::Title => "title",
            ChunkKind::Heading => "heading",
            ChunkKind::Body => "body",
        },
    ] {
        hasher.update((value.len() as u64).to_le_bytes());
        hasher.update(value.as_bytes());
    }
    // Neighbor context deliberately means editing chunk N invalidates N±1.
    let digest = hasher.finalize();
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Write bytes by writing a temp sibling then renaming, so an interrupted
/// write never leaves a half-file that a resume would mistake for done.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("part.tmp");
    std::fs::write(&tmp, bytes).map_err(|source| BuildError::Io {
        path: tmp.clone(),
        source,
    })?;
    if !path.exists() {
        return std::fs::rename(&tmp, path).map_err(|source| BuildError::Io {
            path: path.to_path_buf(),
            source,
        });
    }

    // Windows rename does not replace an existing file. Keep the previous
    // complete file as a backup until the replacement is safely in place.
    let backup = path.with_extension("replace.bak");
    let _ = std::fs::remove_file(&backup);
    std::fs::rename(path, &backup).map_err(|source| BuildError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => {
            let _ = std::fs::remove_file(backup);
            Ok(())
        }
        Err(source) => {
            let _ = std::fs::rename(&backup, path);
            Err(BuildError::Io {
                path: path.to_path_buf(),
                source,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These tests exercise planning and synthesis against a hand-built Book,
    // so they need no EPUB fixture and no network.
    fn book_with_sections() -> Book {
        use bookforge_core::ir::{
            Block, BlockId, BlockKind, BookFormat, BookId, DomPath, Metadata, Section, SectionId,
            TextRun,
        };

        let make_block = |id: &str, section: &str, text: &str| Block {
            id: BlockId(id.to_string()),
            section_id: SectionId(section.to_string()),
            kind: if matches!(id, "b1" | "b3") {
                BlockKind::Heading(1)
            } else {
                BlockKind::Paragraph
            },
            dom_path: DomPath(vec![0]),
            text_runs: vec![TextRun {
                id: format!("{id}_r0"),
                text: text.to_string(),
            }],
            inline_marks: Vec::new(),
            protected_spans: Vec::new(),
            token_estimate: 10,
        };

        Book {
            source_path: None,
            id: BookId("b".to_string()),
            format: BookFormat::Epub,
            metadata: Metadata {
                title: Some("Test Book".to_string()),
                creators: vec![],
                language: Some("en".to_string()),
            },
            manifest: vec![],
            spine: vec![],
            sections: vec![
                Section {
                    id: SectionId("s1".to_string()),
                    href: "c1.xhtml".to_string(),
                    spine_index: 0,
                    title: Some("First".to_string()),
                    heading_level: Some(1),
                    block_ids: vec![BlockId("b1".to_string()), BlockId("b2".to_string())],
                    prev: None,
                    next: Some(SectionId("s2".to_string())),
                },
                Section {
                    id: SectionId("s2".to_string()),
                    href: "c2.xhtml".to_string(),
                    spine_index: 1,
                    title: None,
                    heading_level: None,
                    block_ids: vec![BlockId("b3".to_string())],
                    prev: Some(SectionId("s1".to_string())),
                    next: None,
                },
            ],
            blocks: vec![
                make_block("b1", "s1", "One sentence here. Two sentence here."),
                make_block("b2", "s1", "A third paragraph."),
                make_block("b3", "s2", "Second chapter body."),
            ],
        }
    }

    #[test]
    fn plan_covers_both_chapters_and_titles() {
        let book = book_with_sections();
        let options = AudiobookOptions {
            max_chars: 40,
            ..AudiobookOptions::default()
        };
        let plan = plan_chunks(&book, &options);
        assert!(plan.iter().any(|c| c.chapter_index == 0));
        assert!(plan.iter().any(|c| c.chapter_index == 1));
        // Untitled second section falls back to "Chapter 2".
        assert!(plan.iter().any(|c| c.chapter_title == "Chapter 2"));
        // File names are zero-padded and format-suffixed.
        assert!(plan[0].file.starts_with("chapter-001-part-001"));
        assert_eq!(plan[0].synthesis_sha256.len(), 64);
        for chapter_index in [0, 1] {
            assert_eq!(
                plan.iter()
                    .find(|chunk| chunk.chapter_index == chapter_index)
                    .unwrap()
                    .kind,
                ChunkKind::Title
            );
        }
    }

    #[test]
    fn context_slices_are_unicode_safe_and_zero_disables_them() {
        assert_eq!(context_slice("aé日z", 2, false).as_deref(), Some("aé"));
        assert_eq!(context_slice("aé日z", 2, true).as_deref(), Some("日z"));
        assert_eq!(context_slice("aé日z", 99, false).as_deref(), Some("aé日z"));
        assert_eq!(context_slice("aé日z", 0, false), None);
    }

    #[test]
    fn planned_context_stops_at_chapter_boundaries() {
        let options = AudiobookOptions {
            max_chars: 40,
            context_chars: 8,
            ..AudiobookOptions::default()
        };
        let plan = build_plan(&book_with_sections(), &options);
        for pair in plan.windows(2) {
            if pair[0].chapter_index != pair[1].chapter_index {
                assert_eq!(pair[0].next_text, None);
                assert_eq!(pair[1].previous_text, None);
            }
        }
        assert!(plan.iter().any(|chunk| chunk.next_text.is_some()));
    }

    #[test]
    fn heading_break_tag_is_appended_only_to_structural_chunks() {
        let options = AudiobookOptions {
            heading_break_tag: Some("<break time=\"0.6s\" />".to_string()),
            ..AudiobookOptions::default()
        };
        let plan = build_plan(&book_with_sections(), &options);
        assert!(plan[0].text.ends_with(" <break time=\"0.6s\" />"));
        assert!(
            plan.iter()
                .filter(|chunk| chunk.kind == ChunkKind::Body)
                .all(|chunk| !chunk.text.contains("<break"))
        );
    }

    #[test]
    fn synthesis_hash_changes_for_every_new_consistency_input_and_kind() {
        let options = AudiobookOptions::default();
        let base = synthesis_hash("text", ChunkKind::Body, None, None, &options);

        let mut changed = options.clone();
        changed.seed = Some(7);
        assert_ne!(
            base,
            synthesis_hash("text", ChunkKind::Body, None, None, &changed)
        );

        let mut changed = options.clone();
        changed.language_code = Some("it".to_string());
        assert_ne!(
            base,
            synthesis_hash("text", ChunkKind::Body, None, None, &changed)
        );

        let mut changed = options.clone();
        changed.text_normalization = Some(TextNormalization::Auto);
        assert_ne!(
            base,
            synthesis_hash("text", ChunkKind::Body, None, None, &changed)
        );

        assert_ne!(
            base,
            synthesis_hash("text", ChunkKind::Body, Some("before"), None, &options)
        );
        assert_ne!(
            base,
            synthesis_hash("text", ChunkKind::Body, None, Some("after"), &options)
        );
        assert_ne!(
            base,
            synthesis_hash("text", ChunkKind::Title, None, None, &options)
        );
    }

    #[test]
    fn synthesis_hash_uses_v2_cache_tag() {
        let options = AudiobookOptions::default();
        let actual = synthesis_hash("text", ChunkKind::Body, None, None, &options);
        assert_eq!(
            actual,
            synthesis_hash_with_version(
                "bookforge-audio-v2",
                "text",
                ChunkKind::Body,
                None,
                None,
                &options,
            )
        );
        assert_ne!(
            actual,
            synthesis_hash_with_version(
                "bookforge-audio-v1",
                "text",
                ChunkKind::Body,
                None,
                None,
                &options,
            )
        );
    }

    #[test]
    fn v2_manifest_without_new_fields_still_deserializes() {
        let json = serde_json::json!({
            "schema_version": 2,
            "title": "Old Book",
            "synthesis_id": "mock:model",
            "voice": "voice",
            "format": "wav",
            "speed": 1.0,
            "max_chars": 2000,
            "chapters": 1,
            "chunks": [{
                "chapter_index": 0,
                "chapter_title": "One",
                "part": 1,
                "file": "chapter-001-part-001-old.wav",
                "chars": 10,
                "synthesis_sha256": "0"
            }]
        });
        let manifest: AudiobookManifest = serde_json::from_value(json).unwrap();
        assert_eq!(manifest.schema_version, 2);
        assert_eq!(manifest.chunks[0].kind, ChunkKind::Body);
        assert_eq!(manifest.seed, None);
        assert_eq!(manifest.language, None);
        assert_eq!(manifest.text_normalization, None);
        assert_eq!(manifest.gaps, GapSettings::default());
        assert_eq!(manifest.author, None);
    }

    #[tokio::test]
    async fn build_writes_files_and_manifest_then_resumes() {
        use crate::provider::MockTtsProvider;
        let dir = tempfile::tempdir().unwrap();
        let book = book_with_sections();
        let options = AudiobookOptions {
            out_dir: dir.path().join("out"),
            max_chars: 40,
            concurrency: 2,
            ..AudiobookOptions::default()
        };

        let report = build_audiobook(
            &book,
            Arc::new(MockTtsProvider::new()),
            &options,
            CancellationToken::new(),
            |_p| {},
        )
        .await
        .expect("build should succeed");

        assert!(report.chunks_total >= 3);
        assert_eq!(report.chunks_synthesized, report.chunks_total);
        assert_eq!(report.chunks_skipped, 0);
        assert!(report.manifest_path.exists());
        for file in &report.files {
            assert!(file.exists(), "expected {file:?} to exist");
        }

        // Second run resumes: nothing re-synthesized.
        let resumed = build_audiobook(
            &book,
            Arc::new(MockTtsProvider::new()),
            &options,
            CancellationToken::new(),
            |_p| {},
        )
        .await
        .expect("resume should succeed");
        assert_eq!(resumed.chunks_synthesized, 0);
        assert_eq!(resumed.chunks_skipped, resumed.chunks_total);
    }

    #[tokio::test]
    async fn changing_synthesis_settings_creates_new_chunks() {
        use crate::provider::MockTtsProvider;
        let dir = tempfile::tempdir().unwrap();
        let book = book_with_sections();
        let mut options = AudiobookOptions {
            out_dir: dir.path().join("out"),
            max_chars: 40,
            ..AudiobookOptions::default()
        };

        let first_plan = plan_chunks(&book, &options);
        build_audiobook(
            &book,
            Arc::new(MockTtsProvider::new()),
            &options,
            CancellationToken::new(),
            |_| {},
        )
        .await
        .expect("first build");

        options.voice = "different-voice".to_string();
        let second_plan = plan_chunks(&book, &options);
        assert_ne!(first_plan[0].file, second_plan[0].file);
        let report = build_audiobook(
            &book,
            Arc::new(MockTtsProvider::new()),
            &options,
            CancellationToken::new(),
            |_| {},
        )
        .await
        .expect("changed build");
        assert_eq!(report.chunks_synthesized, report.chunks_total);
        assert_eq!(report.chunks_skipped, 0);
    }

    #[test]
    fn invalid_options_are_rejected_before_synthesis() {
        let options = AudiobookOptions {
            speed: 5.0,
            ..AudiobookOptions::default()
        };
        assert!(matches!(
            validate_options(&options),
            Err(BuildError::InvalidOptions(_))
        ));
    }

    #[tokio::test]
    async fn partial_run_only_synthesizes_missing_chunks() {
        use crate::provider::MockTtsProvider;
        let dir = tempfile::tempdir().unwrap();
        let book = book_with_sections();
        let options = AudiobookOptions {
            out_dir: dir.path().join("out"),
            max_chars: 40,
            concurrency: 1,
            ..AudiobookOptions::default()
        };

        // Pre-render exactly the first planned chunk so the next run must
        // skip it and synthesize the rest.
        std::fs::create_dir_all(&options.out_dir).unwrap();
        let plan = plan_chunks(&book, &options);
        let valid_wav = crate::provider::pcm_s16le_mono_wav(8_000, &[0, 0]);
        std::fs::write(options.out_dir.join(&plan[0].file), valid_wav).unwrap();

        let report = build_audiobook(
            &book,
            Arc::new(MockTtsProvider::new()),
            &options,
            CancellationToken::new(),
            |_p| {},
        )
        .await
        .expect("build should succeed");
        assert_eq!(report.chunks_skipped, 1);
        assert_eq!(report.chunks_synthesized, report.chunks_total - 1);
    }

    #[tokio::test]
    async fn corrupt_cached_audio_is_regenerated_instead_of_reused() {
        use crate::provider::MockTtsProvider;
        let dir = tempfile::tempdir().unwrap();
        let book = book_with_sections();
        let options = AudiobookOptions {
            out_dir: dir.path().join("out"),
            max_chars: 40,
            concurrency: 1,
            ..AudiobookOptions::default()
        };

        std::fs::create_dir_all(&options.out_dir).unwrap();
        let plan = plan_chunks(&book, &options);
        let corrupt_path = options.out_dir.join(&plan[0].file);
        std::fs::write(&corrupt_path, b"nonempty but not a wave file").unwrap();

        let report = build_audiobook(
            &book,
            Arc::new(MockTtsProvider::new()),
            &options,
            CancellationToken::new(),
            |_p| {},
        )
        .await
        .expect("corrupt cache entry should be recoverable");

        assert_eq!(report.chunks_skipped, 0);
        assert_eq!(report.chunks_synthesized, report.chunks_total);
        assert_eq!(&std::fs::read(corrupt_path).unwrap()[..4], b"RIFF");
    }

    #[test]
    fn page_furniture_is_never_planned_for_narration() {
        let mut book = book_with_sections();
        for block in &mut book.blocks {
            block.kind = bookforge_core::ir::BlockKind::PageFurniture;
        }

        let plan = plan_chunks(&book, &AudiobookOptions::default());
        assert!(plan.is_empty());
    }

    #[tokio::test]
    async fn empty_book_is_an_error() {
        use crate::provider::MockTtsProvider;
        let dir = tempfile::tempdir().unwrap();
        let mut book = book_with_sections();
        for block in &mut book.blocks {
            block.text_runs.clear();
        }
        let options = AudiobookOptions {
            out_dir: dir.path().join("out"),
            ..AudiobookOptions::default()
        };
        let err = build_audiobook(
            &book,
            Arc::new(MockTtsProvider::new()),
            &options,
            CancellationToken::new(),
            |_p| {},
        )
        .await
        .unwrap_err();
        assert!(matches!(err, BuildError::NoText));
    }
}
