#[cfg(feature = "tui")]
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use bookforge_audio::{
    AudioFormat, AudiobookOptions, ElevenLabsTtsConfig, ElevenLabsTtsProvider, GeminiTtsConfig,
    GeminiTtsProvider, MockTtsProvider, OpenAiTtsConfig, OpenAiTtsProvider, Progress,
    StitchOptions, build_audiobook, plan_chunks, stitch, validate_options,
};
use bookforge_epub::{ReflowOptions, read_epub, reflow_epub};
use clap::{Args, ValueEnum};
use indicatif::{ProgressBar, ProgressStyle};
use tokio_util::sync::CancellationToken;

use crate::progress::UiMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum AudioProviderKind {
    /// Deterministic, offline provider that emits silent WAV clips. For dry
    /// runs and tests.
    Mock,
    /// Any OpenAI-compatible `/audio/speech` endpoint (OpenAI, or a local
    /// server such as kokoro-fastapi via `--base-url`).
    Openai,
    /// Google Gemini Generate Content text-to-speech.
    Gemini,
    /// ElevenLabs native text-to-speech API.
    Elevenlabs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum AudioFormatArg {
    Mp3,
    Opus,
    Aac,
    Flac,
    Wav,
    Pcm,
}

impl AudioFormatArg {
    fn into_format(self) -> AudioFormat {
        match self {
            AudioFormatArg::Mp3 => AudioFormat::Mp3,
            AudioFormatArg::Opus => AudioFormat::Opus,
            AudioFormatArg::Aac => AudioFormat::Aac,
            AudioFormatArg::Flac => AudioFormat::Flac,
            AudioFormatArg::Wav => AudioFormat::Wav,
            AudioFormatArg::Pcm => AudioFormat::Pcm,
        }
    }
}

#[derive(Debug, Args)]
pub struct AudiobookArgs {
    /// Source EPUB to narrate.
    pub input: PathBuf,

    /// Output directory for the audio files and manifest. Defaults to
    /// `<input-stem>.audiobook`.
    #[arg(long)]
    pub out: Option<PathBuf>,

    #[arg(long, value_enum, default_value_t = AudioProviderKind::Openai)]
    pub provider: AudioProviderKind,

    /// TTS model. The default depends on the selected provider.
    #[arg(long)]
    pub model: Option<String>,

    /// Voice name (OpenAI/Gemini) or voice ID (ElevenLabs). Defaults to
    /// `alloy` for OpenAI and `Kore` for Gemini; ElevenLabs requires it.
    #[arg(long)]
    pub voice: Option<String>,

    /// Output codec/container. Defaults to MP3 for OpenAI/ElevenLabs and WAV
    /// for Gemini/the offline mock.
    #[arg(long, value_enum)]
    pub format: Option<AudioFormatArg>,

    #[arg(long, default_value_t = 1.0)]
    pub speed: f32,

    /// Override the endpoint base URL (e.g. `http://localhost:8880/v1` for a
    /// local server). The default depends on the selected provider.
    #[arg(long)]
    pub base_url: Option<String>,

    /// Environment variable holding the API key. Defaults to
    /// `OPENAI_API_KEY`, `GEMINI_API_KEY`, or `ELEVENLABS_API_KEY`.
    #[arg(long)]
    pub api_key_env: Option<String>,

    /// Maximum characters per synthesis request.
    #[arg(long, default_value_t = 2_000)]
    pub max_chars: usize,

    /// Number of chunks synthesized in parallel.
    #[arg(long, default_value_t = 4)]
    pub concurrency: usize,

    #[arg(long, default_value_t = 120)]
    pub timeout_seconds: u64,

    /// Optional delivery or pronunciation guidance. Supported by Gemini and
    /// gpt-4o-mini-tts-compatible providers.
    #[arg(long)]
    pub instructions: Option<String>,

    /// After synthesis, join each chapter's parts into one file with ffmpeg.
    #[arg(long, default_value_t = false)]
    pub stitch: bool,

    /// Also assemble a single `.m4b` with chapter markers. Implies
    /// `--stitch` and requires ffmpeg (and ffprobe for the markers).
    #[arg(long, default_value_t = false)]
    pub m4b: bool,

    /// Print the chapter/chunk plan and exit without synthesizing.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,

    /// Remove audio chunk files in the output directory left over from earlier
    /// runs (a different voice, model, speed, format, or edited source text).
    /// The current run's chunks are always kept. With `--dry-run` the stale
    /// files are only reported, never deleted.
    #[arg(long, default_value_t = false)]
    pub prune: bool,

    /// Progress output mode. `tui` opens an attached full-screen audiobook
    /// dashboard; `json` emits one JSON object per completed chunk.
    #[arg(long, value_enum, default_value_t = UiMode::Auto)]
    pub ui: UiMode,
}

pub async fn run(args: AudiobookArgs, cancel: CancellationToken) -> Result<()> {
    let (book, pdf_page_grouping) = read_epub_for_audio(&args.input)?;

    let out_dir = args
        .out
        .clone()
        .unwrap_or_else(|| default_out_dir(&args.input));
    let format = args
        .format
        .map(AudioFormatArg::into_format)
        .unwrap_or_else(|| match args.provider {
            AudioProviderKind::Mock | AudioProviderKind::Gemini => AudioFormat::Wav,
            AudioProviderKind::Openai => AudioFormat::Mp3,
            AudioProviderKind::Elevenlabs => AudioFormat::Mp3,
        });
    let voice = match args.provider {
        AudioProviderKind::Mock => args.voice.clone().unwrap_or_else(|| "mock".to_string()),
        AudioProviderKind::Openai => args.voice.clone().unwrap_or_else(|| "alloy".to_string()),
        AudioProviderKind::Gemini => args.voice.clone().unwrap_or_else(|| "Kore".to_string()),
        AudioProviderKind::Elevenlabs => args.voice.clone().ok_or_else(|| {
            anyhow::anyhow!("ElevenLabs requires --voice with an ElevenLabs voice ID")
        })?,
    };
    if args.provider == AudioProviderKind::Mock && format != AudioFormat::Wav {
        anyhow::bail!("the mock provider emits WAV audio; use --format wav or omit --format");
    }
    if args.provider == AudioProviderKind::Gemini
        && !matches!(format, AudioFormat::Wav | AudioFormat::Pcm)
    {
        anyhow::bail!(
            "Gemini TTS returns 24 kHz PCM; use --format wav (recommended) or --format pcm"
        );
    }
    if args.provider == AudioProviderKind::Gemini && (args.speed - 1.0).abs() > f32::EPSILON {
        anyhow::bail!("Gemini TTS does not expose playback-speed control; use --speed 1.0");
    }
    if args.provider == AudioProviderKind::Elevenlabs
        && matches!(format, AudioFormat::Aac | AudioFormat::Flac)
    {
        anyhow::bail!(
            "ElevenLabs does not offer AAC or FLAC output; choose mp3, opus, wav, or pcm"
        );
    }
    if args.provider == AudioProviderKind::Elevenlabs && args.instructions.is_some() {
        anyhow::bail!(
            "ElevenLabs has no free-form instructions field; use --speed, voice settings stored in ElevenLabs, or inline model-supported audio tags"
        );
    }
    if (args.stitch || args.m4b) && format == AudioFormat::Pcm {
        anyhow::bail!(
            "raw PCM does not carry the sample metadata needed for stitching; choose wav, mp3, opus, aac, or flac"
        );
    }
    if args.timeout_seconds == 0 {
        anyhow::bail!("--timeout-seconds must be greater than zero");
    }

    let (model, synthesis_id) = match args.provider {
        AudioProviderKind::Mock => {
            let model = args
                .model
                .clone()
                .unwrap_or_else(|| "mock-silence".to_string());
            (model.clone(), format!("mock:{model}"))
        }
        AudioProviderKind::Openai => {
            let model = args
                .model
                .clone()
                .unwrap_or_else(|| "gpt-4o-mini-tts".to_string());
            let base_url = args
                .base_url
                .as_deref()
                .unwrap_or("https://api.openai.com/v1");
            validate_audio_base_url(base_url)?;
            if base_url
                .trim_end_matches('/')
                .eq_ignore_ascii_case("https://api.openai.com/v1")
                && args.max_chars > 4_096
            {
                anyhow::bail!(
                    "OpenAI speech input is limited to 4096 characters; set --max-chars to 4096 or less"
                );
            }
            (
                model.clone(),
                format!("openai-compatible:{base_url}:{model}"),
            )
        }
        AudioProviderKind::Gemini => {
            let model = args
                .model
                .clone()
                .unwrap_or_else(|| "gemini-3.1-flash-tts-preview".to_string());
            let base_url = args
                .base_url
                .as_deref()
                .unwrap_or("https://generativelanguage.googleapis.com/v1beta");
            validate_audio_base_url(base_url)?;
            (model.clone(), format!("gemini:{base_url}:{model}"))
        }
        AudioProviderKind::Elevenlabs => {
            let model = args
                .model
                .clone()
                .unwrap_or_else(|| "eleven_multilingual_v2".to_string());
            let base_url = args
                .base_url
                .as_deref()
                .unwrap_or("https://api.elevenlabs.io/v1");
            validate_audio_base_url(base_url)?;
            (model.clone(), format!("elevenlabs:{base_url}:{model}"))
        }
    };
    let options = AudiobookOptions {
        out_dir: out_dir.clone(),
        voice: voice.clone(),
        format,
        speed: args.speed,
        max_chars: args.max_chars,
        concurrency: args.concurrency,
        synthesis_id,
        instructions: args.instructions.clone(),
        pdf_page_grouping,
    };
    validate_options(&options)?;

    let plan = plan_chunks(&book, &options);
    if plan.is_empty() {
        anyhow::bail!(
            "no narratable text found in {} (cover-only or empty book?)",
            args.input.display()
        );
    }
    let chapter_count = plan
        .iter()
        .map(|chunk| chunk.chapter_index)
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    let total_chars: usize = plan.iter().map(|chunk| chunk.chars).sum();

    let human_output = !matches!(args.ui, UiMode::Quiet | UiMode::Json | UiMode::Tui);
    if human_output {
        println!("Input: {}", args.input.display());
        println!(
            "Title: {}",
            book.metadata.title.as_deref().unwrap_or("(untitled)")
        );
        println!("Output: {}", out_dir.display());
        println!("Voice: {voice} | Format: {}", format.extension());
        println!("Model: {model}");
        println!(
            "Plan: {chapter_count} chapters, {} chunks, {total_chars} characters",
            plan.len()
        );
    }

    if args.dry_run {
        let stale = if args.prune {
            bookforge_audio::find_stale_chunks(&out_dir, &plan)
                .with_context(|| format!("scanning {} for stale chunks", out_dir.display()))?
        } else {
            Vec::new()
        };
        if args.ui == UiMode::Json {
            println!(
                "{}",
                serde_json::json!({
                    "event": "audiobook_plan",
                    "chapters": chapter_count,
                    "chunks": plan.len(),
                    "characters": total_chars,
                    "dry_run": true,
                    "stale_chunks": stale.len(),
                    "stale_bytes": stale.iter().map(|chunk| chunk.bytes).sum::<u64>(),
                })
            );
        } else if human_output {
            println!("Dry run: no audio synthesized.");
            if args.prune {
                report_stale_chunks(&stale, false);
            }
        }
        return Ok(());
    }

    #[cfg(feature = "tui")]
    let use_tui = args.ui == UiMode::Tui && std::io::stdout().is_terminal();
    #[cfg(not(feature = "tui"))]
    let use_tui = false;

    #[cfg(feature = "tui")]
    let report = if use_tui {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Progress>();
        let callback: Arc<dyn Fn(Progress) + Send + Sync> = Arc::new(move |event| {
            let _ = tx.send(event);
        });
        let mut app = crate::tui::AudioTuiApp::new(crate::tui::AudioTuiInfo {
            title: book
                .metadata
                .title
                .clone()
                .unwrap_or_else(|| "(untitled)".to_string()),
            input: args.input.display().to_string(),
            output: out_dir.display().to_string(),
            provider: format!("{:?}", args.provider).to_ascii_lowercase(),
            model: model.clone(),
            voice: voice.clone(),
            total: plan.len(),
        })?;
        app.draw()?;
        let mut synthesis = Box::pin(synthesize(
            &args,
            &book,
            &options,
            &model,
            cancel.clone(),
            callback,
        ));
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(120));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let outcome = loop {
            tokio::select! {
                result = &mut synthesis => break result,
                Some(event) = rx.recv() => {
                    app.update(&event);
                    app.draw()?;
                }
                _ = tick.tick() => {
                    if app.pump_input()? {
                        cancel.cancel();
                    }
                    app.draw()?;
                }
            }
        };
        app.finish(if outcome.is_ok() {
            "succeeded"
        } else {
            "failed"
        });
        app.draw().ok();
        let restore = app.restore();
        restore?;
        outcome?
    } else {
        let callback = progress_callback(args.ui, plan.len());
        synthesize(&args, &book, &options, &model, cancel.clone(), callback).await?
    };
    #[cfg(not(feature = "tui"))]
    let report = {
        let callback = progress_callback(args.ui, plan.len());
        synthesize(&args, &book, &options, &model, cancel.clone(), callback).await?
    };

    let fallback_tui_output = args.ui == UiMode::Tui && !use_tui;
    let stitch_report = if args.stitch || args.m4b {
        Some(stitch_output(
            &report.manifest_path,
            &out_dir,
            format,
            args.m4b,
            &book,
            human_output || fallback_tui_output,
        )?)
    } else {
        None
    };

    if human_output || fallback_tui_output {
        println!(
            "Done: {} synthesized, {} reused (resume), {} files total",
            report.chunks_synthesized,
            report.chunks_skipped,
            report.files.len()
        );
        println!("Manifest: {}", report.manifest_path.display());
    } else if args.ui == UiMode::Json {
        println!(
            "{}",
            serde_json::json!({
                "event": "audiobook_finished",
                "status": "succeeded",
                "synthesized": report.chunks_synthesized,
                "cached": report.chunks_skipped,
                "chunks": report.chunks_total,
                "manifest": report.manifest_path,
                "chapter_files": stitch_report.as_ref().map(|result| &result.chapter_files),
                "audiobook": stitch_report.as_ref().and_then(|result| result.book_file.as_ref()),
                "warnings": stitch_report.as_ref().map(|result| &result.warnings),
            })
        );
    }

    if stitch_report.is_none() && human_output {
        println!(
            "Tip: pass --stitch to join each chapter into one file, or --m4b for a single audiobook file."
        );
    }

    if args.prune {
        let stale = bookforge_audio::find_stale_chunks(&out_dir, &plan)
            .with_context(|| format!("scanning {} for stale chunks", out_dir.display()))?;
        let (removed, freed) = bookforge_audio::remove_stale_chunks(&stale)
            .with_context(|| format!("removing stale chunks in {}", out_dir.display()))?;
        if args.ui == UiMode::Json {
            println!(
                "{}",
                serde_json::json!({
                    "event": "audiobook_pruned",
                    "removed": removed,
                    "freed_bytes": freed,
                })
            );
        } else if human_output || fallback_tui_output {
            if removed == 0 {
                println!("Prune: no stale chunks from earlier runs.");
            } else {
                println!(
                    "Prune: removed {removed} stale chunk file(s), freed {}.",
                    format_bytes(freed)
                );
            }
        }
    }

    Ok(())
}

/// Print a human-readable list of stale chunk files. When `deleted` is false
/// the files were only reported (dry run).
fn report_stale_chunks(stale: &[bookforge_audio::StaleChunk], deleted: bool) {
    if stale.is_empty() {
        println!("Prune: no stale chunks from earlier runs.");
        return;
    }
    let freed: u64 = stale.iter().map(|chunk| chunk.bytes).sum();
    let verb = if deleted { "Removed" } else { "Would remove" };
    println!(
        "{verb} {} stale chunk file(s) ({}):",
        stale.len(),
        format_bytes(freed)
    );
    for chunk in stale {
        println!(
            "  {} ({})",
            chunk
                .path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            format_bytes(chunk.bytes)
        );
    }
}

/// Format a byte count as a short human-readable string.
fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn read_epub_for_audio(input: &std::path::Path) -> Result<(bookforge_core::ir::Book, bool)> {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let staged = std::env::temp_dir().join(format!(
        "bookforge-audio-clean-{}-{}-{sequence}.epub",
        std::process::id(),
        bookforge_core::now_ms()
    ));
    let cleanup = TemporaryEpub(staged.clone());
    let reflow = reflow_epub(
        input,
        &staged,
        &ReflowOptions {
            dry_run: false,
            aggressive: false,
            pdf_cleanup: true,
        },
    )
    .with_context(|| format!("failed to prepare EPUB narration from {}", input.display()))?;
    let mut book =
        read_epub(&staged).with_context(|| format!("failed to read EPUB {}", input.display()))?;
    book.source_path = Some(input.to_path_buf());
    let pdf_page_grouping = reflow.report.totals.pdf_documents_detected > 0;
    drop(cleanup);
    Ok((book, pdf_page_grouping))
}

struct TemporaryEpub(PathBuf);

impl Drop for TemporaryEpub {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn progress_callback(ui: UiMode, total: usize) -> Arc<dyn Fn(Progress) + Send + Sync> {
    if ui == UiMode::Json {
        return Arc::new(|event: Progress| {
            println!(
                "{}",
                serde_json::json!({
                    "event": "audiobook_chunk_finished",
                    "done": event.done,
                    "total": event.total,
                    "chapter": event.chapter_title,
                    "cached": event.skipped,
                })
            );
        });
    }
    if ui == UiMode::Quiet {
        return Arc::new(|_| {});
    }
    let progress = Arc::new(ProgressBar::new(total as u64));
    progress.set_style(
        ProgressStyle::with_template("{bar:40.cyan/blue} {pos}/{len} {msg}")
            .unwrap_or_else(|_| ProgressStyle::default_bar()),
    );
    Arc::new(move |event: Progress| {
        progress.set_position(event.done as u64);
        let state = if event.skipped {
            "cached"
        } else {
            "synthesized"
        };
        progress.set_message(format!("{state}: {}", event.chapter_title));
        if event.done == event.total {
            progress.finish_and_clear();
        }
    })
}

async fn synthesize(
    args: &AudiobookArgs,
    book: &bookforge_core::ir::Book,
    options: &AudiobookOptions,
    model: &str,
    cancel: CancellationToken,
    on_progress: Arc<dyn Fn(Progress) + Send + Sync>,
) -> Result<bookforge_audio::AudiobookReport> {
    let report = match args.provider {
        AudioProviderKind::Mock => {
            let callback = on_progress.clone();
            build_audiobook(
                book,
                Arc::new(MockTtsProvider::new()),
                options,
                cancel.clone(),
                move |event| callback(event),
            )
            .await?
        }
        AudioProviderKind::Openai => {
            let mut config = OpenAiTtsConfig::openai(Some(model.to_string()));
            if let Some(base_url) = args.base_url.clone() {
                config.base_url = base_url;
            }
            if let Some(api_key_env) = args.api_key_env.clone() {
                config.api_key_env = api_key_env;
            }
            config.timeout_seconds = args.timeout_seconds;
            let provider = OpenAiTtsProvider::new_with_cancel(config, cancel.clone())
                .context("failed to build TTS provider")?;
            let callback = on_progress.clone();
            build_audiobook(
                book,
                Arc::new(provider),
                options,
                cancel.clone(),
                move |event| callback(event),
            )
            .await?
        }
        AudioProviderKind::Gemini => {
            let mut config = GeminiTtsConfig::google(Some(model.to_string()));
            if let Some(base_url) = args.base_url.clone() {
                config.base_url = base_url;
            }
            if let Some(api_key_env) = args.api_key_env.clone() {
                config.api_key_env = api_key_env;
            }
            config.timeout_seconds = args.timeout_seconds;
            let provider = GeminiTtsProvider::new_with_cancel(config, cancel.clone())
                .context("failed to build Gemini TTS provider")?;
            let callback = on_progress.clone();
            build_audiobook(
                book,
                Arc::new(provider),
                options,
                cancel.clone(),
                move |event| callback(event),
            )
            .await?
        }
        AudioProviderKind::Elevenlabs => {
            let mut config = ElevenLabsTtsConfig::hosted(Some(model.to_string()));
            if let Some(base_url) = args.base_url.clone() {
                config.base_url = base_url;
            }
            if let Some(api_key_env) = args.api_key_env.clone() {
                config.api_key_env = api_key_env;
            }
            config.timeout_seconds = args.timeout_seconds;
            let provider = ElevenLabsTtsProvider::new_with_cancel(config, cancel.clone())
                .context("failed to build ElevenLabs TTS provider")?;
            let callback = on_progress;
            build_audiobook(
                book,
                Arc::new(provider),
                options,
                cancel.clone(),
                move |event| callback(event),
            )
            .await?
        }
    };
    Ok(report)
}

fn stitch_output(
    manifest_path: &std::path::Path,
    out_dir: &std::path::Path,
    format: AudioFormat,
    make_m4b: bool,
    book: &bookforge_core::ir::Book,
    human_output: bool,
) -> Result<bookforge_audio::StitchReport> {
    let manifest_json = std::fs::read_to_string(manifest_path)
        .with_context(|| format!("failed to read manifest {}", manifest_path.display()))?;
    let manifest: bookforge_audio::AudiobookManifest =
        serde_json::from_str(&manifest_json).context("failed to parse audiobook manifest")?;

    if human_output {
        println!("Stitching with ffmpeg...");
    }
    let stitch_options = StitchOptions {
        out_dir: out_dir.to_path_buf(),
        extension: format.extension().to_string(),
        make_m4b,
        title: book.metadata.title.clone(),
    };
    let result = stitch(&manifest, &stitch_options);
    if human_output {
        for warning in &result.warnings {
            eprintln!("warning: {warning}");
        }
        if !result.chapter_files.is_empty() {
            println!("Chapter files: {}", result.chapter_files.len());
        }
    }
    if make_m4b && result.book_file.is_none() {
        anyhow::bail!(
            "--m4b was requested, but audiobook.m4b could not be assembled; install ffmpeg and review the stitch warnings above"
        );
    }
    if human_output && let Some(book_file) = &result.book_file {
        println!("Audiobook: {}", book_file.display());
    }
    Ok(result)
}

fn default_out_dir(input: &std::path::Path) -> PathBuf {
    let stem = input
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "audiobook".to_string());
    input
        .parent()
        .map(|parent| parent.to_path_buf())
        .unwrap_or_default()
        .join(format!("{stem}.audiobook"))
}

fn validate_audio_base_url(value: &str) -> Result<()> {
    let url = reqwest::Url::parse(value).context("invalid TTS --base-url")?;
    let host = url
        .host_str()
        .context("TTS --base-url must include a host")?;
    let loopback = matches!(host, "localhost" | "127.0.0.1" | "::1");
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        anyhow::bail!("TTS --base-url must use HTTPS, except for loopback HTTP endpoints");
    }
    if !url.username().is_empty() || url.password().is_some() {
        anyhow::bail!("TTS --base-url must not contain credentials; use --api-key-env instead");
    }
    if url.query().is_some() || url.fragment().is_some() {
        anyhow::bail!("TTS --base-url must not contain a query string or fragment");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_audio_base_url;

    #[test]
    fn audio_base_url_allows_https_and_loopback_http_only() {
        assert!(validate_audio_base_url("https://api.openai.com/v1").is_ok());
        assert!(validate_audio_base_url("http://localhost:8880/v1").is_ok());
        assert!(validate_audio_base_url("http://127.0.0.1:8880/v1").is_ok());
        assert!(validate_audio_base_url("http://example.com/v1").is_err());
        assert!(validate_audio_base_url("https://token@example.com/v1").is_err());
    }
}
