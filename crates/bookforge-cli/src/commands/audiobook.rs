use std::collections::BTreeSet;
#[cfg(feature = "tui")]
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use bookforge_audio::{
    AudioFormat, AudiobookOptions, ElevenLabsTtsConfig, ElevenLabsTtsProvider, GeminiTtsConfig,
    GeminiTtsProvider, MockTtsProvider, OpenAiTtsConfig, OpenAiTtsProvider, Progress,
    StitchOptions, TextNormalization, build_audiobook, elevenlabs_model_max_input_chars,
    fetch_elevenlabs_subscription, list_elevenlabs_voices, plan_chunks,
    resolve_preferred_elevenlabs_model, stitch, validate_options,
};
use bookforge_epub::{ReflowOptions, read_epub, reflow_epub};
use clap::{Args, ValueEnum};
use indicatif::{ProgressBar, ProgressStyle};
use tokio_util::sync::CancellationToken;

use crate::{audio_cost::AudioCost, progress::UiMode};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum BreakTagsArg {
    Auto,
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum TextNormalizationArg {
    Auto,
    On,
    Off,
}

impl TextNormalizationArg {
    fn into_audio(self) -> TextNormalization {
        match self {
            Self::Auto => TextNormalization::Auto,
            Self::On => TextNormalization::On,
            Self::Off => TextNormalization::Off,
        }
    }
}

#[derive(Debug, Args)]
pub struct AudiobookArgs {
    /// Source EPUB to narrate.
    pub input: Option<PathBuf>,

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

    /// Pause between chapters in milliseconds when stitching.
    #[arg(long, default_value_t = 1_200)]
    pub gap_chapter_ms: u32,

    /// Pause after chapter titles and headings in milliseconds.
    #[arg(long, default_value_t = 800)]
    pub gap_title_ms: u32,

    /// Pause between body chunks in milliseconds.
    #[arg(long, default_value_t = 0)]
    pub gap_paragraph_ms: u32,

    /// Add supported ElevenLabs SSML-like breaks after headings.
    #[arg(long, value_enum, default_value_t = BreakTagsArg::Auto)]
    pub break_tags: BreakTagsArg,

    /// Deterministic ElevenLabs synthesis seed.
    #[arg(long)]
    pub seed: Option<u32>,

    /// Narration language code. Defaults to the EPUB language.
    #[arg(long)]
    pub language: Option<String>,

    /// ElevenLabs text-normalization policy.
    #[arg(long, value_enum, default_value_t = TextNormalizationArg::Auto)]
    pub text_normalization: TextNormalizationArg,

    /// Normalize loudness while assembling whole-book files.
    #[arg(long, default_value_t = false)]
    pub loudnorm: bool,

    /// Also assemble a flat whole-book file in the selected audio format.
    #[arg(long, default_value_t = false)]
    pub single: bool,

    /// Narrate only these 1-based chapters (for example `1-3,7`).
    #[arg(long, value_parser = parse_chapter_ranges)]
    pub chapters: Option<BTreeSet<usize>>,

    /// List the voices on an ElevenLabs account and exit.
    #[arg(long, default_value_t = false)]
    pub list_voices: bool,

    /// After synthesis, join each chapter's parts into one file with ffmpeg.
    #[arg(long, default_value_t = false)]
    pub stitch: bool,

    /// Explicitly assemble a chapter-marked `.m4b`. This is already the
    /// default when ffmpeg is available; the flag remains a strict override.
    #[arg(long, default_value_t = false)]
    pub m4b: bool,

    /// Do not automatically create the default chapter-marked `.m4b`.
    #[arg(long, default_value_t = false)]
    pub no_book_file: bool,

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

#[derive(Debug, Clone, Copy)]
struct QuotaInfo {
    remaining: u64,
    limit: u64,
}

pub async fn run(args: AudiobookArgs, cancel: CancellationToken) -> Result<()> {
    if args.list_voices {
        return list_voices_and_exit(&args).await;
    }
    let input = args
        .input
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("INPUT is required unless --list-voices is passed"))?;
    if args.seed.is_some() && args.provider != AudioProviderKind::Elevenlabs {
        anyhow::bail!("--seed is supported only with --provider elevenlabs");
    }
    if args.timeout_seconds == 0 {
        anyhow::bail!("--timeout-seconds must be greater than zero");
    }

    let (book, pdf_page_grouping) = read_epub_for_audio(input)?;

    let out_dir = args.out.clone().unwrap_or_else(|| default_out_dir(input));
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
            anyhow::anyhow!(
                "ElevenLabs requires --voice with an ElevenLabs voice ID; run `bookforge audiobook --list-voices --provider elevenlabs` to see the voices on your account"
            )
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
    let ffmpeg_available = bookforge_audio::ffmpeg_available();
    let make_m4b = args.m4b || (!args.no_book_file && ffmpeg_available);
    let postprocess = args.stitch || make_m4b || args.single;
    if postprocess && format == AudioFormat::Pcm {
        anyhow::bail!(
            "raw PCM does not carry the sample metadata needed for stitching, --m4b, or --single; choose wav, mp3, opus, aac, or flac (or pass --no-book-file for per-chunk PCM output)"
        );
    }

    let elevenlabs_dry_run_default =
        args.provider == AudioProviderKind::Elevenlabs && args.model.is_none() && args.dry_run;
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
            let model = if let Some(model) = args.model.clone() {
                model
            } else if args.dry_run {
                "eleven_multilingual_v2".to_string()
            } else {
                let mut config = ElevenLabsTtsConfig::hosted(None);
                if let Some(base_url) = args.base_url.clone() {
                    config.base_url = base_url;
                }
                if let Some(api_key_env) = args.api_key_env.clone() {
                    config.api_key_env = api_key_env;
                }
                config.timeout_seconds = args.timeout_seconds.min(15);
                match resolve_preferred_elevenlabs_model(
                    &config,
                    args.max_chars,
                    (args.speed - 1.0).abs() > f32::EPSILON,
                )
                .await
                {
                    Ok(model) => model,
                    Err(error) => {
                        eprintln!(
                            "warning: ElevenLabs model preflight failed ({error}); using default eleven_multilingual_v2"
                        );
                        "eleven_multilingual_v2".to_string()
                    }
                }
            };
            let model_limit = elevenlabs_model_max_input_chars(&model);
            if args.max_chars > model_limit {
                anyhow::bail!(
                    "ElevenLabs model {model} is limited to {model_limit} characters; set --max-chars to {model_limit} or less"
                );
            }
            let base_url = args
                .base_url
                .as_deref()
                .unwrap_or("https://api.elevenlabs.io/v1");
            validate_audio_base_url(base_url)?;
            (model.clone(), format!("elevenlabs:{base_url}:{model}"))
        }
    };
    if args.provider == AudioProviderKind::Elevenlabs
        && model == "eleven_v3"
        && (args.speed - 1.0).abs() > f32::EPSILON
    {
        anyhow::bail!(
            "eleven_v3 has no speed control on the ElevenLabs TTS endpoint; use --speed 1.0 or pick another model"
        );
    }
    let language_code = resolve_language_code(
        args.provider,
        &model,
        args.language.as_deref(),
        book.metadata.language.as_deref(),
    );
    let heading_break_tag = resolve_heading_break_tag(args.provider, &model, args.break_tags);
    let options = AudiobookOptions {
        out_dir: out_dir.clone(),
        voice: voice.clone(),
        format,
        speed: args.speed,
        max_chars: args.max_chars,
        concurrency: args.concurrency,
        synthesis_id,
        instructions: args.instructions.clone(),
        context_chars: 300,
        seed: args.seed,
        language_code,
        text_normalization: (args.provider == AudioProviderKind::Elevenlabs)
            .then(|| args.text_normalization.into_audio()),
        heading_break_tag,
        chapter_filter: args.chapters.clone(),
        pdf_page_grouping,
    };
    validate_options(&options)?;

    let plan = plan_chunks(&book, &options);
    if plan.is_empty() {
        anyhow::bail!(
            "no narratable text found in {} (cover-only or empty book?)",
            input.display()
        );
    }
    let chapter_count = plan
        .iter()
        .map(|chunk| chunk.chapter_index)
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    let total_chars: usize = plan.iter().map(|chunk| chunk.chars).sum();
    let provider_name = audio_provider_name(args.provider);
    crate::audio_cost::load_audio_pricing()?;
    let estimated_cost = crate::audio_cost::estimate_audio_cost(provider_name, &model, total_chars);
    let cost_line = format_audio_cost_line(estimated_cost);
    let quota = elevenlabs_quota_preflight(&args, &model, total_chars).await;

    let human_output = !matches!(args.ui, UiMode::Quiet | UiMode::Json | UiMode::Tui);
    if human_output {
        println!("Input: {}", input.display());
        println!(
            "Title: {}",
            book.metadata.title.as_deref().unwrap_or("(untitled)")
        );
        println!("Output: {}", out_dir.display());
        println!("Voice: {voice} | Format: {}", format.extension());
        if elevenlabs_dry_run_default {
            println!(
                "Model: {model} (default; a live run auto-selects the best available ElevenLabs model)"
            );
        } else {
            println!("Model: {model}");
        }
        println!(
            "Plan: {chapter_count} chapters, {} chunks, {total_chars} characters",
            plan.len()
        );
        println!("{cost_line}");
        if let Some(quota) = quota {
            println!(
                "ElevenLabs quota: {} remaining of {}",
                quota.remaining, quota.limit
            );
        }
        if !ffmpeg_available && !args.no_book_file && !args.m4b {
            eprintln!(
                "warning: ffmpeg not found on PATH; only per-chunk audio files will be produced"
            );
        }
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
                    "provider": provider_name,
                    "model": model,
                    "estimated_cost_usd": estimated_cost.and_then(|cost| cost.usd),
                    "estimated_credits": estimated_cost.and_then(|cost| cost.credits),
                    "quota_remaining": quota.map(|quota| quota.remaining),
                    "quota_limit": quota.map(|quota| quota.limit),
                    "book_file": make_m4b,
                    "single_file": args.single,
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

    if args.ui == UiMode::Json {
        println!(
            "{}",
            serde_json::json!({
                "event": "audiobook_plan",
                "chapters": chapter_count,
                "chunks": plan.len(),
                "characters": total_chars,
                "provider": provider_name,
                "model": model,
                "estimated_cost_usd": estimated_cost.and_then(|cost| cost.usd),
                "estimated_credits": estimated_cost.and_then(|cost| cost.credits),
                "quota_remaining": quota.map(|quota| quota.remaining),
                "quota_limit": quota.map(|quota| quota.limit),
                "book_file": make_m4b,
                "single_file": args.single,
                "dry_run": false,
            })
        );
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
            input: input.display().to_string(),
            output: out_dir.display().to_string(),
            provider: provider_name.to_string(),
            model: model.clone(),
            voice: voice.clone(),
            cost_line: Some(cost_line.clone()),
            chapters_total: chapter_count,
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
    let stitch_report = if postprocess {
        Some(stitch_output(StitchRequest {
            manifest_path: &report.manifest_path,
            out_dir: &out_dir,
            book: &book,
            format,
            make_m4b,
            require_m4b: args.m4b,
            make_single: args.single,
            gap_chapter_ms: args.gap_chapter_ms,
            gap_title_ms: args.gap_title_ms,
            gap_paragraph_ms: args.gap_paragraph_ms,
            loudnorm: args.loudnorm,
            human_output: human_output || fallback_tui_output,
        })?)
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
        let chapter_files = stitch_report
            .as_ref()
            .map_or(0, |result| result.chapter_files.len());
        println!(
            "Artifacts: {} chunk files, {chapter_files} chapter files, book file: {}, single file: {}",
            report.files.len(),
            stitch_report
                .as_ref()
                .and_then(|result| result.book_file.as_ref())
                .map_or("none".to_string(), |path| path.display().to_string()),
            stitch_report
                .as_ref()
                .and_then(|result| result.single_file.as_ref())
                .map_or("none".to_string(), |path| path.display().to_string()),
        );
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
                "chunk_files": report.files,
                "chapter_files": stitch_report.as_ref().map(|result| &result.chapter_files),
                "audiobook": stitch_report.as_ref().and_then(|result| result.book_file.as_ref()),
                "single_file": stitch_report.as_ref().and_then(|result| result.single_file.as_ref()),
                "warnings": stitch_report.as_ref().map(|result| &result.warnings),
            })
        );
    }

    if stitch_report.is_none() && human_output && args.no_book_file {
        println!(
            "Tip: pass --stitch to join each chapter, --m4b for a chapter-marked book, or --single for a flat whole-book file."
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

pub(crate) fn parse_chapter_ranges(value: &str) -> Result<BTreeSet<usize>> {
    let mut chapters = BTreeSet::new();
    for raw_item in value.split(',') {
        let item = raw_item.trim();
        if item.is_empty() {
            anyhow::bail!("chapter range contains an empty item");
        }
        if let Some((raw_start, raw_end)) = item.split_once('-') {
            let start = parse_chapter_number(raw_start.trim())?;
            let end = parse_chapter_number(raw_end.trim())?;
            if start > end {
                anyhow::bail!("chapter range {start}-{end} is reversed");
            }
            chapters.extend(start..=end);
        } else {
            chapters.insert(parse_chapter_number(item)?);
        }
    }
    Ok(chapters)
}

fn parse_chapter_number(value: &str) -> Result<usize> {
    let chapter = value
        .parse::<usize>()
        .map_err(|_| anyhow::anyhow!("invalid chapter number '{value}'"))?;
    if chapter == 0 {
        anyhow::bail!("chapter numbers are 1-based; 0 is not valid");
    }
    Ok(chapter)
}

fn normalize_language_code(value: &str) -> Option<String> {
    value
        .trim()
        .split(['-', '_'])
        .next()
        .filter(|primary| !primary.is_empty())
        .map(str::to_ascii_lowercase)
}

fn resolve_language_code(
    provider: AudioProviderKind,
    model: &str,
    explicit: Option<&str>,
    metadata: Option<&str>,
) -> Option<String> {
    let language = explicit
        .and_then(normalize_language_code)
        .or_else(|| metadata.and_then(normalize_language_code));
    if provider == AudioProviderKind::Elevenlabs
        && matches!(model, "eleven_flash_v2_5" | "eleven_turbo_v2_5")
    {
        return language;
    }
    if provider == AudioProviderKind::Elevenlabs && language.is_some() {
        eprintln!(
            "warning: ElevenLabs model {model} rejects language_code; ignoring {} language",
            if explicit.is_some() {
                "the explicit --language"
            } else {
                "the EPUB"
            }
        );
    } else if explicit.is_some() {
        eprintln!(
            "warning: --language is only applied to ElevenLabs flash/turbo v2.5 models; ignoring it for {}",
            audio_provider_name(provider)
        );
    }
    None
}

fn resolve_heading_break_tag(
    provider: AudioProviderKind,
    model: &str,
    setting: BreakTagsArg,
) -> Option<String> {
    (setting == BreakTagsArg::Auto
        && provider == AudioProviderKind::Elevenlabs
        && matches!(
            model,
            "eleven_flash_v2_5" | "eleven_turbo_v2_5" | "eleven_multilingual_v2"
        ))
    .then(|| "<break time=\"0.6s\" />".to_string())
}

fn audio_provider_name(provider: AudioProviderKind) -> &'static str {
    match provider {
        AudioProviderKind::Mock => "mock",
        AudioProviderKind::Openai => "openai",
        AudioProviderKind::Gemini => "gemini",
        AudioProviderKind::Elevenlabs => "elevenlabs",
    }
}

fn format_audio_cost_line(cost: Option<AudioCost>) -> String {
    match cost {
        Some(AudioCost {
            usd: Some(usd),
            credits: Some(credits),
        }) => format!("Estimated cost: ~${usd:.2} / ~{credits:.0} credits"),
        Some(AudioCost { usd: Some(usd), .. }) => format!("Estimated cost: ~${usd:.2}"),
        Some(AudioCost {
            credits: Some(credits),
            ..
        }) => format!("Estimated cost: ~{credits:.0} credits"),
        _ => "Estimated cost: no pricing for this model".to_string(),
    }
}

async fn list_voices_and_exit(args: &AudiobookArgs) -> Result<()> {
    if args.provider != AudioProviderKind::Elevenlabs {
        anyhow::bail!("--list-voices requires --provider elevenlabs");
    }
    if args.timeout_seconds == 0 {
        anyhow::bail!("--timeout-seconds must be greater than zero");
    }
    let base_url = args
        .base_url
        .as_deref()
        .unwrap_or("https://api.elevenlabs.io/v1");
    validate_audio_base_url(base_url)?;
    let key_env = args.api_key_env.as_deref().unwrap_or("ELEVENLABS_API_KEY");
    let api_key = std::env::var(key_env)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("environment variable '{key_env}' is not set"))?;
    let voices = list_elevenlabs_voices(base_url, &api_key, args.timeout_seconds)
        .await
        .context("failed to list ElevenLabs voices")?;
    let id_width = voices
        .iter()
        .map(|voice| voice.voice_id.chars().count())
        .chain(std::iter::once("voice_id".len()))
        .max()
        .unwrap_or("voice_id".len());
    let name_width = voices
        .iter()
        .map(|voice| voice.name.chars().count())
        .chain(std::iter::once("name".len()))
        .max()
        .unwrap_or("name".len());
    println!("{:<id_width$}  {:<name_width$}  labels", "voice_id", "name");
    for voice in voices {
        let labels = if voice.labels.is_empty() {
            "-".to_string()
        } else {
            voice
                .labels
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        println!(
            "{:<id_width$}  {:<name_width$}  {labels}",
            voice.voice_id, voice.name
        );
    }
    Ok(())
}

async fn elevenlabs_quota_preflight(
    args: &AudiobookArgs,
    model: &str,
    planned_chars: usize,
) -> Option<QuotaInfo> {
    if args.provider != AudioProviderKind::Elevenlabs {
        return None;
    }
    let key_env = args.api_key_env.as_deref().unwrap_or("ELEVENLABS_API_KEY");
    let key_is_set = std::env::var(key_env)
        .ok()
        .is_some_and(|value| !value.trim().is_empty());
    if args.dry_run && !key_is_set {
        return None;
    }
    let mut config = ElevenLabsTtsConfig::hosted(Some(model.to_string()));
    if let Some(base_url) = args.base_url.clone() {
        config.base_url = base_url;
    }
    if let Some(api_key_env) = args.api_key_env.clone() {
        config.api_key_env = api_key_env;
    }
    config.timeout_seconds = args.timeout_seconds.min(15);
    match fetch_elevenlabs_subscription(&config).await {
        Ok(subscription) => {
            let remaining = subscription
                .character_limit
                .saturating_sub(subscription.character_count);
            if planned_chars as u64 > remaining {
                eprintln!(
                    "warning: planned audiobook has {planned_chars} characters, exceeding the ElevenLabs quota of {remaining} remaining characters"
                );
            }
            Some(QuotaInfo {
                remaining,
                limit: subscription.character_limit,
            })
        }
        Err(error) => {
            eprintln!("warning: ElevenLabs quota preflight failed ({error}); continuing");
            None
        }
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

/// Inputs for [`stitch_output`], named at the call site.
///
/// Four of these are bools, and two of them differ only in intent:
/// `make_m4b` means "assemble an m4b if ffmpeg can", while `require_m4b` means
/// "the user asked for one, so failing to build it is an error". As positional
/// arguments they sat next to each other and were easy to transpose.
struct StitchRequest<'a> {
    manifest_path: &'a std::path::Path,
    out_dir: &'a std::path::Path,
    book: &'a bookforge_core::ir::Book,
    format: AudioFormat,
    make_m4b: bool,
    require_m4b: bool,
    make_single: bool,
    gap_chapter_ms: u32,
    gap_title_ms: u32,
    gap_paragraph_ms: u32,
    loudnorm: bool,
    human_output: bool,
}

fn stitch_output(request: StitchRequest<'_>) -> Result<bookforge_audio::StitchReport> {
    let StitchRequest {
        manifest_path,
        out_dir,
        book,
        format,
        make_m4b,
        require_m4b,
        make_single,
        gap_chapter_ms,
        gap_title_ms,
        gap_paragraph_ms,
        loudnorm,
        human_output,
    } = request;

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
        gap_chapter_ms,
        gap_title_ms,
        gap_paragraph_ms,
        loudnorm,
        make_single,
        author: (!book.metadata.creators.is_empty()).then(|| book.metadata.creators.join(", ")),
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
    if require_m4b && result.book_file.is_none() {
        anyhow::bail!(
            "--m4b was requested, but audiobook.m4b could not be assembled; install ffmpeg and review the stitch warnings above"
        );
    }
    if make_single && result.single_file.is_none() {
        anyhow::bail!(
            "--single was requested, but the flat whole-book file could not be assembled; install ffmpeg and review the stitch warnings above"
        );
    }
    if human_output && let Some(book_file) = &result.book_file {
        println!("Audiobook: {}", book_file.display());
    }
    if human_output && let Some(single_file) = &result.single_file {
        println!("Single file: {}", single_file.display());
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
    use super::{
        AudioProviderKind, BreakTagsArg, normalize_language_code, parse_chapter_ranges,
        resolve_heading_break_tag, resolve_language_code, validate_audio_base_url,
    };

    #[test]
    fn audio_base_url_allows_https_and_loopback_http_only() {
        assert!(validate_audio_base_url("https://api.openai.com/v1").is_ok());
        assert!(validate_audio_base_url("http://localhost:8880/v1").is_ok());
        assert!(validate_audio_base_url("http://127.0.0.1:8880/v1").is_ok());
        assert!(validate_audio_base_url("http://example.com/v1").is_err());
        assert!(validate_audio_base_url("https://token@example.com/v1").is_err());
    }

    #[test]
    fn chapter_ranges_accept_singletons_ranges_and_whitespace() {
        assert_eq!(
            parse_chapter_ranges("1-3, 7").unwrap(),
            [1, 2, 3, 7].into_iter().collect()
        );
        assert_eq!(
            parse_chapter_ranges("4").unwrap(),
            [4].into_iter().collect()
        );
    }

    #[test]
    fn chapter_ranges_reject_zero_reversed_empty_and_garbage() {
        let zero = parse_chapter_ranges("0").unwrap_err().to_string();
        assert!(zero.contains("1-based"));

        let reversed = parse_chapter_ranges("5-2").unwrap_err().to_string();
        assert!(reversed.contains("reversed"));

        for value in ["", "1,,2", ",1", "1,"] {
            let empty = parse_chapter_ranges(value).unwrap_err().to_string();
            assert!(empty.contains("empty item"), "{value:?}: {empty}");
        }

        for value in ["x", "1-x", "1-2-3"] {
            let garbage = parse_chapter_ranges(value).unwrap_err().to_string();
            assert!(
                garbage.contains("invalid chapter number"),
                "{value:?}: {garbage}"
            );
        }
    }

    #[test]
    fn language_normalization_uses_lowercase_primary_subtag() {
        assert_eq!(normalize_language_code("en-US").as_deref(), Some("en"));
        assert_eq!(normalize_language_code("PT_br").as_deref(), Some("pt"));
        assert_eq!(normalize_language_code("  "), None);
        assert_eq!(
            resolve_language_code(
                AudioProviderKind::Elevenlabs,
                "eleven_flash_v2_5",
                None,
                Some("en-US")
            )
            .as_deref(),
            Some("en")
        );
    }

    #[test]
    fn automatic_break_tags_follow_the_elevenlabs_model_policy() {
        for model in [
            "eleven_flash_v2_5",
            "eleven_turbo_v2_5",
            "eleven_multilingual_v2",
        ] {
            assert!(
                resolve_heading_break_tag(AudioProviderKind::Elevenlabs, model, BreakTagsArg::Auto)
                    .is_some()
            );
        }
        assert!(
            resolve_heading_break_tag(
                AudioProviderKind::Elevenlabs,
                "eleven_v3",
                BreakTagsArg::Auto
            )
            .is_none()
        );
        assert!(
            resolve_heading_break_tag(
                AudioProviderKind::Openai,
                "eleven_flash_v2_5",
                BreakTagsArg::Auto
            )
            .is_none()
        );
        assert!(
            resolve_heading_break_tag(
                AudioProviderKind::Elevenlabs,
                "eleven_flash_v2_5",
                BreakTagsArg::Off
            )
            .is_none()
        );
    }
}
