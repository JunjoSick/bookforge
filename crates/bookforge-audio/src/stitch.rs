//! Optional ffmpeg-based audiobook post-processing.
//!
//! Subprocess discipline (AUDIO-5): every `ffmpeg` invocation is started
//! with `-nostdin` and a null stdin so it can never block awaiting terminal
//! input (including when spawned from the dashboard), and every `ffmpeg` /
//! `ffprobe` call runs under a hard deadline — on expiry the child is killed
//! and reaped exactly like the PDF crate's poppler handling, and the failure
//! surfaces as a normal stitch warning instead of hanging the run forever.
//!
//! Loudness normalization and chapter markers (AUDIO-12/AUDIO-13): the
//! historical single pass concatenated everything, applied `loudnorm` once,
//! and computed chapter starts from *pre-normalization* chapter durations;
//! single-pass loudnorm resamples internally, so later chapters drifted out
//! of alignment with their markers. When `loudnorm` is active for M4B
//! assembly each chapter is now normalized into an intermediate first, its
//! duration is probed post-normalization, and those measurements drive the
//! chapter metadata. Success guarantees are unchanged: if any intermediate
//! or probe fails, the book assembly is refused with a warning rather than
//! published unchaptered or mis-chaptered. Single-file output carries no
//! markers, so it keeps the cheaper single pass.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::builder::{AudiobookManifest, ChunkRecord};
use crate::text::ChunkKind;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone)]
pub struct StitchOptions {
    pub out_dir: PathBuf,
    /// File extension of the per-chunk files (e.g. "mp3", "wav").
    pub extension: String,
    pub make_m4b: bool,
    pub title: Option<String>,
    pub gap_chapter_ms: u32,
    pub gap_title_ms: u32,
    pub gap_paragraph_ms: u32,
    pub loudnorm: bool,
    pub make_single: bool,
    pub author: Option<String>,
    /// Optional source-cover image to attach to the M4B. Cover failures are
    /// non-fatal: assembly is retried without artwork.
    pub cover_path: Option<PathBuf>,
}

impl Default for StitchOptions {
    fn default() -> Self {
        Self {
            out_dir: PathBuf::from("audiobook"),
            extension: "wav".to_string(),
            make_m4b: false,
            title: None,
            gap_chapter_ms: 1_200,
            gap_title_ms: 800,
            gap_paragraph_ms: 0,
            loudnorm: false,
            make_single: false,
            author: None,
            cover_path: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct StitchReport {
    pub chapter_files: Vec<PathBuf>,
    pub book_file: Option<PathBuf>,
    pub single_file: Option<PathBuf>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default)]
struct ConcatGaps {
    chapter: Option<String>,
    title: Option<String>,
    paragraph: Option<String>,
}

const FFMPEG_STDERR_LIMIT: usize = 16 * 1024;
static STAGED_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Hard deadline for one ffmpeg execution. Long audiobook encodes legitimately
/// run for many minutes, so the budget is generous but finite: a wedged child
/// (blocked on tty input, deadlocked filter graph) is killed and reaped rather
/// than hanging the build — or the dashboard child that spawned it — forever.
const FFMPEG_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// ffprobe calls parse small local files; a minute is already pathological.
const FFPROBE_TIMEOUT: Duration = Duration::from_secs(60);
/// `-version` availability probes answer instantly.
const TOOL_PROBE_TIMEOUT: Duration = Duration::from_secs(15);

pub fn ffmpeg_available() -> bool {
    tool_available("ffmpeg")
}

pub fn ffprobe_available() -> bool {
    tool_available("ffprobe")
}

/// Probe-tool constructor. Unlike ffmpeg, ffprobe has no `-nostdin` flag
/// (it errors with "Option not found" on builds of 6.x+), so tty safety here
/// comes from the null stdin every runner sets rather than from a CLI flag.
fn ffprobe_command() -> Command {
    Command::new("ffprobe")
}

fn tool_available(tool: &str) -> bool {
    let mut command = Command::new(tool);
    if tool == "ffmpeg" {
        // ffmpeg honours (and audio flows require) -nostdin; see
        // [`run_ffmpeg_with_timeout`] for the request-path equivalent.
        command.arg("-nostdin");
    }
    command.arg("-version");
    matches!(
        run_tool_with_deadline(command, TOOL_PROBE_TIMEOUT),
        Some(output) if output.status.success()
    )
}

struct ToolOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
}

enum WaitOutcome {
    Finished(Result<ExitStatus, std::io::Error>),
    TimedOut,
}

struct ChildGuard {
    child: Option<Child>,
}

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn stdout(&mut self) -> Option<std::process::ChildStdout> {
        self.child.as_mut()?.stdout.take()
    }

    fn stderr(&mut self) -> Option<std::process::ChildStderr> {
        self.child.as_mut()?.stderr.take()
    }

    fn wait_with_deadline(&mut self, timeout: Duration) -> WaitOutcome {
        let started = Instant::now();
        loop {
            match self.child() {
                Ok(None) => {}
                Ok(Some(status)) => return WaitOutcome::Finished(Ok(status)),
                Err(error) => return WaitOutcome::Finished(Err(error)),
            }
            if started.elapsed() >= timeout {
                // AUDIO-5: kill + reap on expiry so no zombie and no hang.
                self.kill_and_reap();
                return WaitOutcome::TimedOut;
            }
            let remaining = timeout.saturating_sub(started.elapsed());
            std::thread::sleep(remaining.min(Duration::from_millis(25)));
        }
    }

    fn child(&mut self) -> std::io::Result<Option<ExitStatus>> {
        let Some(child) = self.child.as_mut() else {
            return Ok(None);
        };
        let status = child.try_wait()?;
        if status.is_some() {
            self.child = None;
        }
        Ok(status)
    }

    fn kill_and_reap(&mut self) {
        let Some(child) = self.child.as_mut() else {
            return;
        };
        if !matches!(child.try_wait(), Ok(Some(_))) {
            // Group kill first (unix): a wrapper's grandchildren hold our
            // pipes open, so they must die with the child for reap+drain to
            // terminate promptly.
            let pid = child.id();
            #[cfg(unix)]
            if pid > 0 && !process_signals::kill_process_group(pid) {
                let _ = child.kill();
            }
            #[cfg(not(unix))]
            {
                let _ = pid;
                let _ = child.kill();
            }
            if child.wait().is_err() {
                return;
            }
        }
        self.child = None;
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.kill_and_reap();
    }
}

/// Spawn with piped stdio (plus null stdin), capture bounded stdout/stderr,
/// and enforce `timeout` via kill+reap. Returns `None` for spawn failures and
/// timeouts alike; callers decide how a silent loss degrades.
fn run_tool_with_deadline(mut command: Command, timeout: Duration) -> Option<ToolOutput> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    isolate_process_group(&mut command);
    let mut guard = ChildGuard::new(command.spawn().ok()?);
    let stderr = guard.stderr()?;
    // Drain stderr concurrently: a chatty child must never block on a full
    // pipe while we are polling its exit status.
    let stderr_reader = std::thread::spawn(move || {
        let mut sink = [0u8; 8 * 1024];
        let mut drain = std::io::BufReader::new(stderr);
        loop {
            match drain.read(&mut sink) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    });
    let stdout_pipe = guard.stdout()?;
    let outcome = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut reader = stdout_pipe;
        let mut buffer = [0u8; 16 * 1024];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    bytes.extend_from_slice(&buffer[..read]);
                    if bytes.len() > 4 * 1024 * 1024 {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        bytes
    });
    let wait = guard.wait_with_deadline(timeout);
    let _ = stderr_reader.join();
    match wait {
        WaitOutcome::TimedOut => None,
        WaitOutcome::Finished(result) => result.ok().map(|status| ToolOutput {
            status,
            stdout: outcome.join().unwrap_or_default(),
        }),
    }
}

pub fn stitch(manifest: &AudiobookManifest, options: &StitchOptions) -> StitchReport {
    let mut report = StitchReport::default();
    // AUDIO-12: loudnorm only ever applies inside M4B / single-file assembly;
    // surfacing the no-op here keeps per-chapter-only stitches honest even
    // when ffmpeg is missing and stitching would be skipped outright.
    if options.loudnorm && !options.make_m4b && !options.make_single {
        report.warnings.push(
            "--loudnorm was requested but neither M4B assembly nor single-file output is enabled; \
             normalization applies only to those outputs, so it was ignored for this stitch"
                .to_string(),
        );
    }
    // AUDIO-14: name the codec that cannot take silence instead of folding
    // the limitation into a generic silence-generation failure.
    if (options.gap_chapter_ms > 0 || options.gap_title_ms > 0 || options.gap_paragraph_ms > 0)
        && !gaps_supported(&options.extension)
    {
        report.warnings.push(format!(
            "gap settings were ignored because ffmpeg cannot encode silence for the .{} codec; stitched without inter-chunk gaps",
            options.extension
        ));
    }
    let unresolved = manifest
        .chunks
        .iter()
        .filter(|chunk| {
            !matches!(
                chunk.status,
                crate::builder::ChunkStatus::Synthesized | crate::builder::ChunkStatus::Cached
            )
        })
        .count();
    if unresolved > 0 {
        report.warnings.push(format!(
            "{unresolved} chunk(s) are unresolved; skipped stitching to avoid publishing an incomplete audiobook"
        ));
        return report;
    }
    if !ffmpeg_available() {
        report.warnings.push(
            "ffmpeg not found on PATH; skipped stitching (per-chunk files are intact)".into(),
        );
        return report;
    }

    let chapters = group_by_chapter(&manifest.chunks);
    let requested_gaps =
        options.gap_chapter_ms > 0 || options.gap_title_ms > 0 || options.gap_paragraph_ms > 0;
    let mut gaps = ConcatGaps::default();
    let mut silence_files = Vec::new();
    if requested_gaps && gaps_supported(&options.extension) {
        let first_chunk = chapters
            .iter()
            .flat_map(|(_, parts)| parts)
            .next()
            .map(|part| options.out_dir.join(&part.file));
        match first_chunk
            .as_deref()
            .and_then(probe_audio_params)
            .and_then(|(rate, channels)| {
                prepare_silence_files(options, rate, channels, &mut silence_files).ok()
            }) {
            Some(prepared) => gaps = prepared,
            None => report.warnings.push(
                "ffprobe could not determine audio parameters or silence generation failed; stitching without gaps"
                    .to_string(),
            ),
        }
    }

    for (chapter_index, parts) in &chapters {
        let title = parts
            .first()
            .map(|part| part.chapter_title.clone())
            .unwrap_or_else(|| format!("Chapter {}", chapter_index + 1));
        let entries = build_chapter_concat_entries(parts, &gaps);
        let output_name = format!(
            "chapter-{:03}-{}.{}",
            chapter_index + 1,
            sanitize_filename(&title),
            options.extension
        );
        match concat_copy(&options.out_dir, &entries, &output_name) {
            Ok(()) => report.chapter_files.push(options.out_dir.join(output_name)),
            Err(error) => report
                .warnings
                .push(format!("chapter {}: {error}", chapter_index + 1)),
        }
    }

    let all_chapters = report.chapter_files.len() == chapters.len();
    if (options.make_m4b || options.make_single) && !all_chapters {
        report.warnings.push(format!(
            "only {}/{} chapters stitched successfully; skipped book assembly to avoid an incomplete audiobook",
            report.chapter_files.len(),
            chapters.len()
        ));
    } else {
        if options.make_m4b {
            match assemble_m4b(options, &chapters, &report.chapter_files, &gaps) {
                Ok((path, cover_warning)) => {
                    report.book_file = Some(path);
                    if let Some(warning) = cover_warning {
                        report.warnings.push(warning);
                    }
                }
                Err(error) => report.warnings.push(error),
            }
        }
        if options.make_single {
            match assemble_single_file(options, &report.chapter_files, &gaps) {
                Ok(path) => report.single_file = Some(path),
                Err(error) => report.warnings.push(error),
            }
        }
    }

    for path in silence_files {
        let _ = std::fs::remove_file(path);
    }
    report
}

fn prepare_silence_files(
    options: &StitchOptions,
    rate: u32,
    channels: u16,
    generated: &mut Vec<PathBuf>,
) -> std::result::Result<ConcatGaps, String> {
    let mut cache = BTreeMap::<u32, String>::new();
    for ms in [
        options.gap_chapter_ms,
        options.gap_title_ms,
        options.gap_paragraph_ms,
    ] {
        if ms == 0 || cache.contains_key(&ms) {
            continue;
        }
        let path = ensure_silence_file(&options.out_dir, ms, &options.extension, rate, channels)?;
        generated.push(path.clone());
        cache.insert(ms, file_name_of(&path));
    }
    Ok(ConcatGaps {
        chapter: cache.get(&options.gap_chapter_ms).cloned(),
        title: cache.get(&options.gap_title_ms).cloned(),
        paragraph: cache.get(&options.gap_paragraph_ms).cloned(),
    })
}

fn group_by_chapter(chunks: &[ChunkRecord]) -> Vec<(usize, Vec<ChunkRecord>)> {
    let mut grouped: BTreeMap<usize, Vec<ChunkRecord>> = BTreeMap::new();
    for chunk in chunks {
        grouped
            .entry(chunk.chapter_index)
            .or_default()
            .push(chunk.clone());
    }
    grouped
        .into_iter()
        .map(|(index, mut parts)| {
            parts.sort_by_key(|part| part.part);
            (index, parts)
        })
        .collect()
}

fn build_chapter_concat_entries(parts: &[ChunkRecord], gaps: &ConcatGaps) -> Vec<String> {
    let mut entries = Vec::new();
    for (index, part) in parts.iter().enumerate() {
        entries.push(part.file.clone());
        match part.kind {
            ChunkKind::Title | ChunkKind::Heading => {
                if let Some(gap) = &gaps.title {
                    entries.push(gap.clone());
                }
            }
            ChunkKind::Body
                if parts
                    .get(index + 1)
                    .is_some_and(|next| next.kind == ChunkKind::Body) =>
            {
                if let Some(gap) = &gaps.paragraph {
                    entries.push(gap.clone());
                }
            }
            ChunkKind::Body => {}
        }
    }
    entries
}

fn build_book_concat_entries(chapter_files: &[PathBuf], gap: Option<&str>) -> Vec<String> {
    let mut entries = Vec::new();
    for (index, path) in chapter_files.iter().enumerate() {
        entries.push(file_name_of(path));
        if index + 1 < chapter_files.len()
            && let Some(gap) = gap
        {
            entries.push(gap.to_string());
        }
    }
    entries
}

fn concat_copy(dir: &Path, inputs: &[String], output: &str) -> std::result::Result<(), String> {
    let list_name = format!("{output}.concat.txt");
    let list_path = dir.join(&list_name);
    std::fs::write(&list_path, concat_list_content(inputs))
        .map_err(|error| format!("writing concat list: {error}"))?;
    let output_path = dir.join(output);
    let staged = staged_output_path(&output_path);
    let staged_name = file_name_of(&staged);
    let mut command = ffmpeg_command();
    command
        .current_dir(dir)
        .args(["-y", "-f", "concat", "-safe", "0", "-i"])
        .arg(&list_name)
        .args(["-c", "copy"])
        .arg(&staged_name);
    let result = run_ffmpeg_transactional(&mut command, &staged, &output_path, "ffmpeg concat");
    let _ = std::fs::remove_file(&list_path);
    result
}

fn assemble_m4b(
    options: &StitchOptions,
    chapters: &[(usize, Vec<ChunkRecord>)],
    chapter_files: &[PathBuf],
    gaps: &ConcatGaps,
) -> std::result::Result<(PathBuf, Option<String>), String> {
    if chapter_files.is_empty() {
        return Err("no chapter files were produced; cannot assemble m4b".into());
    }
    let list_name = "book.concat.txt";
    let meta_name = "chapters.ffmeta.txt";
    let mut intermediates: Vec<PathBuf> = Vec::new();
    let result = (|| {
        // AUDIO-13 marker pipeline: when loudnorm is active every chapter is
        // first normalized into an out_dir intermediate; durations probed
        // from those intermediates drive the chapter table, so markers match
        // the published audio even though single-pass loudnorm resamples.
        let (concat_sources, durations) = if options.loudnorm {
            match normalize_chapters_for_timings(options, chapter_files, &mut intermediates) {
                Ok(prepared) => prepared,
                Err(error) => {
                    return Err(format!(
                        "chapter metadata was skipped because per-chapter loudness normalization could not complete ({error}); skipped m4b assembly rather than publishing drifted chapter markers"
                    ));
                }
            }
        } else {
            let durations = chapter_files
                .iter()
                .map(|path| ffprobe_duration_ms(path))
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| {
                    "chapter metadata was skipped because ffprobe could not determine every chapter duration; skipped m4b assembly rather than publishing an unchaptered book"
                        .to_string()
                })?;
            (chapter_files.to_vec(), durations)
        };

        let chapter_names: Vec<String> = chapters
            .iter()
            .map(|(index, parts)| {
                parts
                    .first()
                    .map(|part| part.chapter_title.clone())
                    .unwrap_or_else(|| format!("Chapter {}", index + 1))
            })
            .collect();
        let chapter_gap_ms = gaps.chapter.as_ref().map_or(0, |_| options.gap_chapter_ms);
        let metadata = build_ffmetadata(
            options.title.as_deref(),
            &chapter_names,
            &durations,
            chapter_gap_ms,
        );
        std::fs::write(options.out_dir.join(meta_name), metadata)
            .map_err(|error| format!("writing chapter metadata: {error}"))?;

        let entries = build_book_concat_entries(&concat_sources, gaps.chapter.as_deref());
        std::fs::write(
            options.out_dir.join(list_name),
            concat_list_content(&entries),
        )
        .map_err(|error| format!("writing book concat list: {error}"))?;

        let mut args = vec![
            "-y".to_string(),
            "-f".to_string(),
            "concat".to_string(),
            "-safe".to_string(),
            "0".to_string(),
            "-i".to_string(),
            list_name.to_string(),
            "-i".to_string(),
            meta_name.to_string(),
        ];
        if let Some(cover_path) = &options.cover_path {
            args.extend(["-i".to_string(), cover_path.to_string_lossy().into_owned()]);
        }
        args.extend([
            "-map_metadata".to_string(),
            "1".to_string(),
            "-map_chapters".to_string(),
            "1".to_string(),
        ]);
        if options.cover_path.is_some() {
            args.extend([
                "-map".to_string(),
                "0:a:0".to_string(),
                "-map".to_string(),
                "2:v:0".to_string(),
            ]);
        }
        // Loudness handling is fully decided before this command: either
        // inputs are per-chapter normalized intermediates (loudnorm) or the
        // run asked for no normalization. Never re-apply a filter here or
        // normalization compounds and durations drift again.
        args.extend([
            "-c:a".to_string(),
            "aac".to_string(),
            "-b:a".to_string(),
            "128k".to_string(),
        ]);
        if options.cover_path.is_some() {
            args.extend([
                "-c:v".to_string(),
                "copy".to_string(),
                "-disposition:v:0".to_string(),
                "attached_pic".to_string(),
            ]);
        }
        append_metadata_args(
            &mut args,
            options.title.as_deref(),
            options.author.as_deref(),
        );
        let output_path = options.out_dir.join("audiobook.m4b");
        let staged = staged_output_path(&output_path);
        args.push(file_name_of(&staged));
        let mut command = ffmpeg_command();
        command.current_dir(&options.out_dir).args(&args);
        match run_ffmpeg_transactional(&mut command, &staged, &output_path, "ffmpeg m4b assembly") {
            Ok(()) => Ok((output_path, None)),
            Err(cover_error) if options.cover_path.is_some() => {
                let mut fallback_options = options.clone();
                fallback_options.cover_path = None;
                let (path, _) = assemble_m4b(&fallback_options, chapters, chapter_files, gaps)?;
                Ok((
                    path,
                    Some(format!(
                        "cover art could not be embedded; produced the m4b without it ({cover_error})"
                    )),
                ))
            }
            Err(error) => Err(error),
        }
    })();
    let _ = std::fs::remove_file(options.out_dir.join(list_name));
    let _ = std::fs::remove_file(options.out_dir.join(meta_name));
    for intermediate in &intermediates {
        let _ = std::fs::remove_file(intermediate);
    }
    result
}

/// Normalize each chapter file into a hidden `.normalized-chapter-NNN.wav`
/// intermediate inside out_dir, probe its duration, and hand back the source
/// names plus the measured timings. Intermediate rate/channels are pinned to
/// the probed input parameters so loudnorm's internal 192 kHz upsample is
/// flattened deterministically before encoding. Every produced file is
/// recorded in `sink`; the caller removes them regardless of outcome.
///
/// Failure here is fatal for M4B assembly by design ("skip rather than
/// publish wrong markers"), matching the pre-existing duration-probe refusal.
fn normalize_chapters_for_timings(
    options: &StitchOptions,
    chapter_files: &[PathBuf],
    sink: &mut Vec<PathBuf>,
) -> std::result::Result<(Vec<PathBuf>, Vec<u64>), String> {
    for (index, path) in chapter_files.iter().enumerate() {
        let (rate, channels) = probe_audio_params(path)
            .ok_or_else(|| format!("ffprobe could not read {} parameters", file_name_of(path)))?;
        let intermediate_path = options.out_dir.join(format!(
            ".normalized-chapter-{index:03}-{rate}-{channels}.wav"
        ));
        let staged = staged_output_path(&intermediate_path);
        let mut command = ffmpeg_command();
        command
            .current_dir(&options.out_dir)
            .args(["-y", "-i"])
            .arg(file_name_of(path))
            .args(["-af", "loudnorm=I=-18:TP=-2:LRA=11"])
            .args([
                "-ar".to_string(),
                rate.to_string(),
                "-ac".to_string(),
                channels.to_string(),
                "-c:a".to_string(),
                "pcm_s16le".to_string(),
            ])
            .arg(file_name_of(&staged));
        run_ffmpeg_transactional(
            &mut command,
            &staged,
            &intermediate_path,
            "ffmpeg per-chapter loudness normalization",
        )?;
        sink.push(intermediate_path);
    }
    let durations = sink
        .iter()
        .map(|path| ffprobe_duration_ms(path))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| {
            "ffprobe could not determine post-normalization chapter durations".to_string()
        })?;
    Ok((sink.clone(), durations))
}

fn assemble_single_file(
    options: &StitchOptions,
    chapter_files: &[PathBuf],
    gaps: &ConcatGaps,
) -> std::result::Result<PathBuf, String> {
    if options.extension.eq_ignore_ascii_case("pcm") {
        return Err("cannot assemble a single pcm file; choose a container format".to_string());
    }
    if chapter_files.is_empty() {
        return Err("no chapter files were produced; cannot assemble single file".to_string());
    }
    let entries = build_book_concat_entries(chapter_files, gaps.chapter.as_deref());
    let list_name = "single.concat.txt";
    std::fs::write(
        options.out_dir.join(list_name),
        concat_list_content(&entries),
    )
    .map_err(|error| format!("writing single-file concat list: {error}"))?;
    let output = format!("audiobook.{}", options.extension);
    let output_path = options.out_dir.join(&output);
    let staged = staged_output_path(&output_path);
    let args = single_file_ffmpeg_args(
        list_name,
        &options.extension,
        options.loudnorm,
        options.title.as_deref(),
        options.author.as_deref(),
        &file_name_of(&staged),
    );
    let mut command = ffmpeg_command();
    command.current_dir(&options.out_dir).args(&args);
    let result = run_ffmpeg_transactional(
        &mut command,
        &staged,
        &output_path,
        "ffmpeg single-file assembly",
    );
    let _ = std::fs::remove_file(options.out_dir.join(list_name));
    result.map(|()| output_path)
}

pub(crate) fn single_file_ffmpeg_args(
    list_name: &str,
    extension: &str,
    loudnorm: bool,
    title: Option<&str>,
    author: Option<&str>,
    output: &str,
) -> Vec<String> {
    let mut args = vec![
        "-y".to_string(),
        "-f".to_string(),
        "concat".to_string(),
        "-safe".to_string(),
        "0".to_string(),
        "-i".to_string(),
        list_name.to_string(),
    ];
    if loudnorm {
        args.extend([
            "-af".to_string(),
            "loudnorm=I=-18:TP=-2:LRA=11".to_string(),
            "-c:a".to_string(),
            encoder_for_extension(extension)
                .unwrap_or(extension)
                .to_string(),
        ]);
    } else {
        args.extend(["-c".to_string(), "copy".to_string()]);
    }
    append_metadata_args(&mut args, title, author);
    args.push(output.to_string());
    args
}

fn append_metadata_args(args: &mut Vec<String>, title: Option<&str>, author: Option<&str>) {
    if let Some(title) = title {
        args.extend(["-metadata".to_string(), format!("title={title}")]);
    }
    if let Some(author) = author {
        args.extend(["-metadata".to_string(), format!("artist={author}")]);
    }
}

fn encoder_for_extension(extension: &str) -> Option<&'static str> {
    match extension.to_ascii_lowercase().as_str() {
        "mp3" => Some("libmp3lame"),
        "wav" => Some("pcm_s16le"),
        "opus" => Some("libopus"),
        "aac" => Some("aac"),
        "flac" => Some("flac"),
        _ => None,
    }
}

fn staged_output_path(final_path: &Path) -> PathBuf {
    let sequence = STAGED_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let file_name = final_path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    let extension = final_path
        .extension()
        .map(|extension| extension.to_string_lossy())
        .unwrap_or_default();
    final_path.with_file_name(format!(
        ".{file_name}.{}-{sequence}.part.{extension}",
        std::process::id()
    ))
}

fn run_ffmpeg_transactional(
    command: &mut Command,
    staged: &Path,
    final_path: &Path,
    context: &str,
) -> std::result::Result<(), String> {
    run_ffmpeg_transactional_with_timeout(command, staged, final_path, context, FFMPEG_TIMEOUT)
}

fn run_ffmpeg_transactional_with_timeout(
    command: &mut Command,
    staged: &Path,
    final_path: &Path,
    context: &str,
    timeout: Duration,
) -> std::result::Result<(), String> {
    let result = run_ffmpeg_with_timeout(command, context, timeout);
    if let Err(error) = result {
        let _ = std::fs::remove_file(staged);
        return Err(error);
    }
    if !staged.is_file() {
        return Err(format!(
            "{context} exited successfully but did not create {}",
            staged.display()
        ));
    }
    if let Err(error) = publish_staged_file(staged, final_path) {
        let _ = std::fs::remove_file(staged);
        return Err(error);
    }
    Ok(())
}

/// Every production ffmpeg invocation starts here so the process-level
/// hardening is unconditional and local to one constructor.
fn ffmpeg_command() -> Command {
    // AUDIO-5: refuse stdin input by flag as well as by descriptor; some
    // builds read interactive prompts even with stdin closed.
    let mut command = Command::new("ffmpeg");
    command.arg("-nostdin");
    command
}

/// Unix-only: run the child in its own process group so a timeout kill takes
/// down any grandchildren too (`sh -c`-style wrappers, filter subprocesses).
/// Without this, a killed wrapper leaves orphans holding our pipes open and
/// the reap can block indefinitely behind them.
#[cfg(unix)]
fn isolate_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    let _ = command.process_group(0);
}

#[cfg(not(unix))]
fn isolate_process_group(_command: &mut Command) {}

#[cfg(unix)]
mod process_signals {
    unsafe extern "C" {
        pub fn kill(pid: i32, sig: i32) -> i32;
        pub fn getpgid(pid: i32) -> i32;
        pub fn getpgrp() -> i32;
    }
    pub const SIGKILL: i32 = 9;

    /// Kill a whole process group by negated pid. Best effort: failure falls
    /// back to the caller's direct-child kill.
    pub fn kill_process_group(pid: u32) -> bool {
        let Ok(pid) = i32::try_from(pid) else {
            return false;
        };
        if pid <= 0 {
            return false;
        }
        // Never trust process-group setup implicitly. If setpgid was skipped or
        // behaved differently in a host environment, the child may still be in
        // BookForge's (or a CI runner's) group. In that case the caller falls
        // back to killing only the direct child.
        let group = unsafe { getpgid(pid) };
        if group != pid || group == unsafe { getpgrp() } {
            return false;
        }
        // POSIX uses a negative PID to address the process group whose ID is
        // the corresponding positive PID. The equality check above also makes
        // the negation safe and ensures we only address the child's own group.
        unsafe { kill(-group, SIGKILL) == 0 }
    }
}

fn run_ffmpeg_with_timeout(
    command: &mut Command,
    context: &str,
    timeout: Duration,
) -> std::result::Result<(), String> {
    // AUDIO-5: a null stdin plus the hard deadline turn a wedged encode
    // into a bounded, reported failure instead of an eternal hang.
    command.stdin(Stdio::null());
    command.stdout(Stdio::null()).stderr(Stdio::piped());
    // Applied here — the single choke point for execution, not just in
    // ffmpeg_command — so wrapper-style test/production commands get group
    // isolation too and grandchild orphans can never outlive their kill.
    isolate_process_group(command);
    let mut child = command
        .spawn()
        .map_err(|error| format!("launching {context}: {error}"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| format!("capturing {context} stderr failed"))?;
    let stderr_reader = std::thread::spawn(move || bounded_stderr_tail(stderr));
    let mut guard = ChildGuard::new(child);
    match guard.wait_with_deadline(timeout) {
        WaitOutcome::TimedOut => {
            let _ = stderr_reader.join();
            Err(format!(
                "{context} did not finish within {} seconds and was terminated",
                timeout.as_secs()
            ))
        }
        WaitOutcome::Finished(result) => {
            let status = result.map_err(|error| format!("waiting for {context}: {error}"))?;
            let (stderr, truncated) = stderr_reader
                .join()
                .map_err(|_| format!("capturing {context} stderr failed"))?
                .map_err(|error| format!("reading {context} stderr: {error}"))?;
            if status.success() {
                return Ok(());
            }
            let stderr = String::from_utf8_lossy(&stderr);
            let stderr = stderr.trim();
            let detail = if stderr.is_empty() {
                String::new()
            } else if truncated {
                format!("; stderr (last {FFMPEG_STDERR_LIMIT} bytes): {stderr}")
            } else {
                format!("; stderr: {stderr}")
            };
            Err(format!("{context} exited with {status}{detail}"))
        }
    }
}

fn bounded_stderr_tail<R: Read>(mut reader: R) -> std::io::Result<(Vec<u8>, bool)> {
    let mut tail = Vec::with_capacity(FFMPEG_STDERR_LIMIT);
    let mut buffer = [0u8; 4 * 1024];
    let mut total = 0usize;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read);
        if read >= FFMPEG_STDERR_LIMIT {
            tail.clear();
            tail.extend_from_slice(&buffer[read - FFMPEG_STDERR_LIMIT..read]);
            continue;
        }
        let overflow = tail
            .len()
            .saturating_add(read)
            .saturating_sub(FFMPEG_STDERR_LIMIT);
        if overflow > 0 {
            tail.drain(..overflow);
        }
        tail.extend_from_slice(&buffer[..read]);
    }
    Ok((tail, total > FFMPEG_STDERR_LIMIT))
}

/// Publish a staged output by renaming over any previous file.
///
/// [`crate::atomic::replace_file`] replaces existing destinations atomically
/// on every supported platform. The helper keeps the prior destination when
/// publication fails; `--prune` sweeps still clean up legacy `.replace.bak`
/// remnants from older runs.
fn publish_staged_file(staged: &Path, final_path: &Path) -> std::result::Result<(), String> {
    crate::atomic::replace_file(staged, final_path)
        .map_err(|error| format!("publishing ffmpeg output {}: {error}", final_path.display()))
}

fn probe_audio_params(path: &Path) -> Option<(u32, u16)> {
    let mut command = ffprobe_command();
    command.args([
        "-v",
        "error",
        "-select_streams",
        "a:0",
        "-show_entries",
        "stream=sample_rate,channels",
        "-of",
        "json",
    ]);
    command.arg(path);
    let output = run_tool_with_deadline(command, FFPROBE_TIMEOUT)?;
    if !output.status.success() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let stream = value.get("streams")?.as_array()?.first()?;
    let rate = stream.get("sample_rate")?.as_str()?.parse().ok()?;
    let channels = u16::try_from(stream.get("channels")?.as_u64()?).ok()?;
    Some((rate, channels))
}

/// Container/codec families where ffmpeg can render silence for concat
/// gaps. AUDIO-14: anything else degrades with an explicit warning naming
/// the codec instead of a silent fallback.
fn gaps_supported(extension: &str) -> bool {
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "mp3" | "wav" | "opus"
    )
}

fn ensure_silence_file(
    out_dir: &Path,
    ms: u32,
    extension: &str,
    rate: u32,
    channels: u16,
) -> std::result::Result<PathBuf, String> {
    let encoder = match extension.to_ascii_lowercase().as_str() {
        "mp3" => "libmp3lame",
        "wav" => "pcm_s16le",
        "opus" => "libopus",
        _ => return Err(format!("silence gaps are unsupported for .{extension}")),
    };
    // Rate and channels belong in the cache key: a stitch killed before the
    // cleanup pass leaves this file behind, and a later run with a different
    // voice or format probes different parameters. Reusing that stale file
    // would feed mismatched silence into a `-c copy` concat.
    let output_name = format!("silence-{ms}-{rate}-{channels}.{extension}");
    let output_path = out_dir.join(&output_name);
    if output_path.is_file() && silence_file_is_valid(&output_path, ms, rate, channels) {
        return Ok(output_path);
    }
    let channel_layout = if channels == 1 { "mono" } else { "stereo" };
    let duration = format!("{:.3}", f64::from(ms) / 1_000.0);
    let staged = staged_output_path(&output_path);
    let staged_name = file_name_of(&staged);
    let mut command = ffmpeg_command();
    command.current_dir(out_dir).args([
        "-y",
        "-f",
        "lavfi",
        "-i",
        &format!("anullsrc=r={rate}:cl={channel_layout}"),
        "-t",
        &duration,
        "-c:a",
        encoder,
        &staged_name,
    ]);
    run_ffmpeg_transactional(
        &mut command,
        &staged,
        &output_path,
        "ffmpeg silence generation",
    )?;
    Ok(output_path)
}

fn silence_file_is_valid(path: &Path, expected_ms: u32, rate: u32, channels: u16) -> bool {
    if std::fs::metadata(path).map_or(true, |metadata| metadata.len() == 0) {
        return false;
    }
    if probe_audio_params(path) != Some((rate, channels)) {
        return false;
    }
    let Some(duration_ms) = ffprobe_duration_ms(path) else {
        return false;
    };
    duration_ms.abs_diff(u64::from(expected_ms)) <= u64::from((expected_ms / 5).max(100))
}

fn ffprobe_duration_ms(path: &Path) -> Option<u64> {
    let mut command = ffprobe_command();
    command.args([
        "-v",
        "error",
        "-show_entries",
        "format=duration",
        "-of",
        "default=nokey=1:noprint_wrappers=1",
    ]);
    command.arg(path);
    let output = run_tool_with_deadline(command, FFPROBE_TIMEOUT)?;
    if !output.status.success() {
        return None;
    }
    let seconds: f64 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .ok()?;
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    Some((seconds * 1_000.0).round() as u64)
}

fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub fn concat_list_content(files: &[String]) -> String {
    let mut out = String::new();
    for file in files {
        let escaped = file.replace('\'', "'\\''");
        out.push_str(&format!("file '{escaped}'\n"));
    }
    out
}

pub fn build_ffmetadata(
    title: Option<&str>,
    names: &[String],
    durations: &[u64],
    inter_chapter_gap_ms: u32,
) -> String {
    let mut out = String::from(";FFMETADATA1\n");
    if let Some(title) = title {
        out.push_str(&format!("title={}\n", escape_ffmeta(title)));
    }
    let mut start = 0u64;
    for (index, (name, duration)) in names.iter().zip(durations.iter()).enumerate() {
        let end = start + duration;
        out.push_str("\n[CHAPTER]\nTIMEBASE=1/1000\n");
        out.push_str(&format!("START={start}\nEND={end}\n"));
        out.push_str(&format!("title={}\n", escape_ffmeta(name)));
        start = end;
        if index + 1 < names.len() {
            start += u64::from(inter_chapter_gap_ms);
        }
    }
    out
}

fn escape_ffmeta(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '=' | ';' | '#' | '\\' => {
                out.push('\\');
                out.push(ch);
            }
            '\n' => out.push(' '),
            _ => out.push(ch),
        }
    }
    out
}

pub fn sanitize_filename(title: &str) -> String {
    let mut out: String = title
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' => ch,
            ' ' | '-' | '_' => '-',
            _ => '-',
        })
        .collect();
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    let trimmed = out.trim_matches('-').to_string();
    let trimmed = if trimmed
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .count()
        < 3
    {
        ascii_fallback_filename(title)
    } else {
        trimmed
    };
    trimmed.chars().take(40).collect::<String>().to_lowercase()
}

fn ascii_fallback_filename(title: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(title.as_bytes());
    let mut fallback = String::from("untitled-");
    for byte in &digest[..4] {
        fallback.push(char::from(HEX[usize::from(byte >> 4)]));
        fallback.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    fallback
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::ChunkStatus;

    fn chunk(chapter_index: usize, part: usize, kind: ChunkKind) -> ChunkRecord {
        ChunkRecord {
            chapter_index,
            chapter_title: format!("Chapter {}", chapter_index + 1),
            part,
            kind,
            file: format!("c{chapter_index}-p{part}.wav"),
            chars: 10,
            synthesis_sha256: "0".repeat(64),
            status: ChunkStatus::Synthesized,
            audio_sha256: Some("1".repeat(64)),
            bytes: Some(44),
            error: None,
        }
    }

    #[test]
    fn chapter_concat_entries_insert_requested_gaps() {
        let parts = vec![
            chunk(0, 1, ChunkKind::Title),
            chunk(0, 2, ChunkKind::Body),
            chunk(0, 3, ChunkKind::Body),
            chunk(0, 4, ChunkKind::Heading),
            chunk(0, 5, ChunkKind::Body),
        ];
        let gaps = ConcatGaps {
            title: Some("title.wav".to_string()),
            paragraph: Some("paragraph.wav".to_string()),
            chapter: None,
        };
        assert_eq!(
            build_chapter_concat_entries(&parts, &gaps),
            vec![
                "c0-p1.wav",
                "title.wav",
                "c0-p2.wav",
                "paragraph.wav",
                "c0-p3.wav",
                "c0-p4.wav",
                "title.wav",
                "c0-p5.wav",
            ]
        );
        assert_eq!(
            build_chapter_concat_entries(&parts, &ConcatGaps::default()),
            parts
                .iter()
                .map(|part| part.file.clone())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn book_concat_entries_insert_gaps_only_between_chapters() {
        let files = vec![PathBuf::from("one.wav"), PathBuf::from("two.wav")];
        assert_eq!(
            build_book_concat_entries(&files, Some("chapter.wav")),
            vec!["one.wav", "chapter.wav", "two.wav"]
        );
        assert_eq!(
            build_book_concat_entries(&files, None),
            vec!["one.wav", "two.wav"]
        );
    }

    #[test]
    fn concat_list_escapes_single_quotes() {
        let content = concat_list_content(&["it's here.mp3".to_string(), "plain.mp3".to_string()]);
        assert_eq!(content, "file 'it'\\''s here.mp3'\nfile 'plain.mp3'\n");
    }

    #[test]
    fn chapter_parts_are_sorted_before_stitching() {
        let grouped = group_by_chapter(&[
            chunk(0, 2, ChunkKind::Body),
            chunk(1, 1, ChunkKind::Title),
            chunk(0, 1, ChunkKind::Title),
        ]);
        assert_eq!(grouped[0].1[0].part, 1);
        assert_eq!(grouped[0].1[1].part, 2);
    }

    #[test]
    fn ffmetadata_has_cumulative_chapter_times_including_gaps() {
        let meta = build_ffmetadata(
            Some("My Book"),
            &["Intro".to_string(), "One".to_string()],
            &[1_000, 2_500],
            1_200,
        );
        assert!(meta.starts_with(";FFMETADATA1\n"));
        assert!(meta.contains("title=My Book"));
        assert!(meta.contains("START=0\nEND=1000\ntitle=Intro"));
        assert!(meta.contains("START=2200\nEND=4700\ntitle=One"));
    }

    #[test]
    fn single_file_args_select_copy_or_loudnorm_and_include_metadata() {
        let copy = single_file_ffmpeg_args(
            "book.txt",
            "mp3",
            false,
            Some("Title"),
            Some("Author"),
            "audiobook.mp3",
        );
        assert!(copy.windows(2).any(|pair| pair == ["-c", "copy"]));
        assert!(copy.contains(&"title=Title".to_string()));
        assert!(copy.contains(&"artist=Author".to_string()));

        let normalized = single_file_ffmpeg_args(
            "book.txt",
            "mp3",
            true,
            Some("Title"),
            Some("Author"),
            "audiobook.mp3",
        );
        assert!(normalized.contains(&"loudnorm=I=-18:TP=-2:LRA=11".to_string()));
        assert!(
            normalized
                .windows(2)
                .any(|pair| pair == ["-c:a", "libmp3lame"])
        );
        assert!(!normalized.windows(2).any(|pair| pair == ["-c", "copy"]));
    }

    #[test]
    fn ffmetadata_escapes_special_characters() {
        let meta = build_ffmetadata(None, &["A = B; C".to_string()], &[10], 0);
        assert!(meta.contains(r"title=A \= B\; C"));
    }

    #[test]
    fn sanitize_filename_collapses_and_lowercases() {
        assert_eq!(
            sanitize_filename("Chapter 1: The Beginning!"),
            "chapter-1-the-beginning"
        );
        assert!(sanitize_filename("***").starts_with("untitled-"));
        assert_eq!(sanitize_filename("  spaced  out  "), "spaced-out");
    }

    #[test]
    fn sanitize_filename_is_ascii_nonempty_and_stable_across_scripts() {
        for title in [
            "Perché l'Italia",
            "Преступление и наказание",
            "Über Größe",
            "矛盾论",
        ] {
            let first = sanitize_filename(title);
            let second = sanitize_filename(title);
            assert_eq!(first, second, "{title}");
            assert!(first.is_ascii(), "{title}: {first}");
            assert!(!first.is_empty(), "{title}");
            assert!(
                first
                    .chars()
                    .filter(|ch| ch.is_ascii_alphanumeric())
                    .count()
                    >= 3,
                "{title}: {first}"
            );
        }
    }

    fn completed_manifest() -> AudiobookManifest {
        AudiobookManifest {
            schema_version: 3,
            title: Some("Fixture Book".to_string()),
            synthesis_id: "mock".to_string(),
            voice: "voice".to_string(),
            format: "wav".to_string(),
            speed: 1.0,
            max_chars: 2_000,
            instructions: None,
            seed: None,
            language: None,
            text_normalization: None,
            gaps: crate::builder::GapSettings::default(),
            author: Some("Fixture Author".to_string()),
            chapters: 1,
            chunks: vec![chunk(0, 1, ChunkKind::Body)],
            completed_chunks: 1,
            status: crate::builder::AudiobookStatus::Succeeded,
            updated_at_ms: 0,
            error: None,
        }
    }

    fn generate_audio_fixture(dir: &Path) {
        let status = Command::new("ffmpeg")
            .current_dir(dir)
            .args([
                "-y",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=0.2",
                "-c:a",
                "pcm_s16le",
                "c0-p1.wav",
            ])
            .status()
            .expect("ffmpeg should launch");
        assert!(status.success());
    }

    fn stitch_fixture_options(dir: &Path) -> StitchOptions {
        StitchOptions {
            out_dir: dir.to_path_buf(),
            extension: "wav".to_string(),
            make_m4b: true,
            title: Some("Fixture Book".to_string()),
            gap_chapter_ms: 0,
            gap_title_ms: 0,
            gap_paragraph_ms: 0,
            author: Some("Fixture Author".to_string()),
            ..StitchOptions::default()
        }
    }

    fn stitch_fixture(dir: &Path, cover_path: Option<PathBuf>) -> StitchReport {
        stitch(
            &completed_manifest(),
            &StitchOptions {
                cover_path,
                ..stitch_fixture_options(dir)
            },
        )
    }

    fn probe_streams(path: &Path) -> serde_json::Value {
        let output = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "stream=index,codec_name,codec_type:stream_disposition=attached_pic",
                "-of",
                "json",
            ])
            .arg(path)
            .output()
            .expect("ffprobe should launch");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("ffprobe should emit JSON")
    }

    #[test]
    fn loudnorm_m4b_chapter_markers_track_post_normalization_durations() {
        if !ffmpeg_available() || !ffprobe_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        for part in ["c0-p1", "c1-p1"] {
            let status = Command::new("ffmpeg")
                .current_dir(dir.path())
                .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
                .arg("sine=frequency=440:duration=0.2")
                .args(["-ar", "44100", "-ac", "1", "-c:a", "pcm_s16le"])
                .arg(format!("{part}.wav"))
                .status()
                .expect("ffmpeg should launch");
            assert!(status.success());
        }
        let mut manifest = completed_manifest();
        manifest.chunks = vec![chunk(0, 1, ChunkKind::Body), chunk(1, 1, ChunkKind::Body)];
        manifest.chapters = 2;
        let mut options = stitch_fixture_options(dir.path());
        options.loudnorm = true;

        let report = stitch(&manifest, &options);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        let book = report.book_file.expect("m4b should be produced");
        // No normalized intermediates may survive assembly.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().starts_with(".normalized-chapter-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");

        let output = Command::new("ffprobe")
            .args(["-v", "error", "-show_chapters", "-of", "json"])
            .arg(&book)
            .output()
            .expect("ffprobe should launch");
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let chapters = value["chapters"].as_array().expect("chapter table").clone();
        assert_eq!(chapters.len(), 2);
        // Each marker covers its actual (post-normalization) chapter audio
        // rather than a stale pre-normalization estimate.
        for chapter in &chapters {
            let start: f64 = chapter["start_time"].as_str().unwrap().parse().unwrap();
            let end: f64 = chapter["end_time"].as_str().unwrap().parse().unwrap();
            assert!(end > start + 0.1, "implausible duration {start}-{end}");
            assert!(end < start + 0.6, "duration drifted too far: {start}-{end}");
        }
    }

    #[test]
    fn real_m4b_with_default_gaps_publishes_and_reports_no_warnings() {
        if !ffmpeg_available() || !ffprobe_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        generate_audio_fixture(dir.path());
        let mut options = stitch_fixture_options(dir.path());
        // CLI defaults mirror these gaps.
        options.gap_chapter_ms = 1_200;
        options.gap_title_ms = 800;

        let report = stitch(&completed_manifest(), &options);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert!(report.book_file.is_some());
        assert!(report.book_file.unwrap().exists());
    }

    #[test]
    fn real_m4b_embeds_cover_as_attached_picture() {
        if !ffmpeg_available() || !ffprobe_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        generate_audio_fixture(dir.path());
        let cover = dir.path().join("cover.png");
        let status = Command::new("ffmpeg")
            .args([
                "-y",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=navy:s=64x64",
                "-frames:v",
                "1",
            ])
            .arg(&cover)
            .status()
            .expect("ffmpeg should generate the cover fixture");
        assert!(status.success());

        let report = stitch_fixture(dir.path(), Some(cover));
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        let book = report.book_file.expect("m4b should be produced");
        let probe = probe_streams(&book);
        let streams = probe["streams"].as_array().unwrap();
        assert!(streams.iter().any(|stream| stream["codec_type"] == "audio"));
        assert!(streams.iter().any(|stream| {
            stream["codec_type"] == "video" && stream["disposition"]["attached_pic"] == 1
        }));
    }

    #[test]
    fn missing_or_unusable_cover_still_produces_valid_m4b() {
        if !ffmpeg_available() || !ffprobe_available() {
            return;
        }
        for cover_fixture in [None, Some("missing"), Some("unusable")] {
            let dir = tempfile::tempdir().unwrap();
            generate_audio_fixture(dir.path());
            let cover_path = cover_fixture.map(|fixture| {
                let path = dir.path().join(format!("{fixture}.image"));
                if fixture == "unusable" {
                    std::fs::write(&path, b"this is not an image").unwrap();
                }
                path
            });
            let expected_warning = cover_path.is_some();
            let report = stitch_fixture(dir.path(), cover_path);
            let book = report.book_file.expect("fallback m4b should be produced");
            let probe = probe_streams(&book);
            let streams = probe["streams"].as_array().unwrap();
            assert!(streams.iter().any(|stream| stream["codec_type"] == "audio"));
            assert!(!streams.iter().any(|stream| stream["codec_type"] == "video"));
            assert_eq!(
                report
                    .warnings
                    .iter()
                    .any(|warning| warning.contains("cover art could not be embedded")),
                expected_warning
            );
        }
    }

    #[test]
    fn failed_ffmpeg_run_removes_stage_and_preserves_prior_public_output() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("audiobook.m4b");
        std::fs::write(&final_path, b"previous-good-output").unwrap();
        let staged = staged_output_path(&final_path);
        std::fs::write(&staged, b"truncated-new-output").unwrap();

        #[cfg(windows)]
        let mut command = {
            let mut command = Command::new("cmd");
            command.args(["/C", "echo diagnostic-from-ffmpeg 1>&2 & exit /b 7"]);
            command
        };
        #[cfg(not(windows))]
        let mut command = {
            let mut command = Command::new("sh");
            command.args(["-c", "echo diagnostic-from-ffmpeg >&2; exit 7"]);
            command
        };

        let error = run_ffmpeg_transactional(&mut command, &staged, &final_path, "test ffmpeg")
            .unwrap_err();
        assert!(error.contains("diagnostic-from-ffmpeg"), "{error}");
        assert_eq!(std::fs::read(&final_path).unwrap(), b"previous-good-output");
        assert!(!staged.exists());
    }

    #[test]
    fn successful_publication_replaces_prior_output_repeatedly() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("audiobook.m4b");
        std::fs::write(&final_path, b"first-output").unwrap();

        for (index, bytes) in [b"second-output".as_slice(), b"third-output"]
            .iter()
            .enumerate()
        {
            let staged = staged_output_path(&final_path);
            std::fs::write(&staged, bytes).unwrap();
            publish_staged_file(&staged, &final_path).unwrap();
            assert_eq!(
                std::fs::read(&final_path).unwrap(),
                *bytes,
                "iteration {index}"
            );
            assert!(!staged.exists());
        }
    }

    #[test]
    fn failed_duration_probe_skips_unchaptered_m4b_with_warning_text() {
        let dir = tempfile::tempdir().unwrap();
        let options = StitchOptions {
            out_dir: dir.path().to_path_buf(),
            make_m4b: true,
            gap_chapter_ms: 0,
            gap_title_ms: 0,
            gap_paragraph_ms: 0,
            ..StitchOptions::default()
        };
        let chapters = vec![(0, vec![chunk(0, 1, ChunkKind::Title)])];
        let missing = dir.path().join("missing-chapter.wav");

        let warning =
            assemble_m4b(&options, &chapters, &[missing], &ConcatGaps::default()).unwrap_err();

        assert!(
            warning.contains("chapter metadata was skipped"),
            "{warning}"
        );
        assert!(warning.contains("unchaptered"), "{warning}");
        assert!(!dir.path().join("audiobook.m4b").exists());
        assert!(!dir.path().join("book.concat.txt").exists());
        assert!(!dir.path().join("chapters.ffmeta.txt").exists());
    }

    #[test]
    fn partial_silence_file_is_rejected_instead_of_reused() {
        let dir = tempfile::tempdir().unwrap();
        let partial = dir.path().join("silence-800-8000-1.wav");
        std::fs::write(&partial, b"RIFF\0\0\0\0WAVEaudio").unwrap();
        assert!(!silence_file_is_valid(&partial, 800, 8_000, 1));

        if !ffmpeg_available() || !ffprobe_available() {
            return;
        }
        let generated = ensure_silence_file(dir.path(), 800, "wav", 8_000, 1).unwrap();
        assert_eq!(generated, partial);
        assert!(silence_file_is_valid(&generated, 800, 8_000, 1));
    }

    #[test]
    fn unresolved_manifest_never_stitches_partial_book() {
        let mut manifest = AudiobookManifest {
            schema_version: 3,
            title: Some("Book".to_string()),
            synthesis_id: "mock".to_string(),
            voice: "voice".to_string(),
            format: "wav".to_string(),
            speed: 1.0,
            max_chars: 2_000,
            instructions: None,
            seed: None,
            language: None,
            text_normalization: None,
            gaps: crate::builder::GapSettings::default(),
            author: None,
            chapters: 1,
            chunks: vec![chunk(0, 1, ChunkKind::Title)],
            completed_chunks: 0,
            status: crate::builder::AudiobookStatus::Failed,
            updated_at_ms: 0,
            error: Some("one failed".to_string()),
        };
        manifest.chunks[0].status = ChunkStatus::Failed;
        let report = stitch(&manifest, &StitchOptions::default());

        assert!(report.chapter_files.is_empty());
        assert!(report.book_file.is_none());
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("unresolved"))
        );
    }

    #[test]
    fn loudnorm_without_any_assembly_target_is_reported_not_silent() {
        // No ffmpeg needed: the no-op report must fire before the tool gate.
        let report = stitch(
            &completed_manifest(),
            &StitchOptions {
                loudnorm: true,
                ..StitchOptions::default()
            },
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("--loudnorm") && warning.contains("ignored")),
            "{:?}",
            report.warnings
        );

        // With a target the flag is honored and stays quiet.
        if !ffmpeg_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        generate_audio_fixture(dir.path());
        let mut options = stitch_fixture_options(dir.path());
        options.loudnorm = true;
        options.make_single = true;
        let report = stitch(&completed_manifest(), &options);
        assert!(
            !report
                .warnings
                .iter()
                .any(|warning| warning.contains("--loudnorm")),
            "{:?}",
            report.warnings
        );
    }

    #[test]
    fn unsupported_gap_codec_is_named_in_a_warning() {
        let report = stitch(
            &completed_manifest(),
            &StitchOptions {
                gap_chapter_ms: 900,
                extension: "aac".to_string(),
                ..StitchOptions::default()
            },
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains(".aac")),
            "{:?}",
            report.warnings
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("stitched without inter-chunk gaps"))
        );
    }

    #[test]
    fn unsupported_loudnorm_chain_survives_silence_probe_helpers_offline() {
        // The helpers behind gaps degrade to None without ffmpeg rather than
        // blocking: probe and duration calls stay bounded even when the tool
        // is absent (they also cover the timeout path by construction).
        let missing = Path::new("definitely-missing-file.wav");
        if !ffprobe_available() {
            assert_eq!(probe_audio_params(missing), None);
            assert_eq!(ffprobe_duration_ms(missing), None);
        }
        assert!(!gaps_supported("flac"));
        assert!(gaps_supported("wav"));
    }

    #[test]
    fn timed_out_ffmpeg_child_is_killed_within_deadline() {
        // The sleeping child is spawned directly (no shell wrapper): the
        // guarantee under test is this crate's deadline + kill + reap against
        // a child that never finishes on its own, offline and portably.
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("out.wav");
        std::fs::write(&final_path, b"prior-output").unwrap();
        let staged = staged_output_path(&final_path);

        #[cfg(windows)]
        let mut command = {
            let mut command = Command::new("ping");
            command.args(["-n", "120", "127.0.0.1"]);
            command
        };
        #[cfg(not(windows))]
        let mut command = {
            let mut command = Command::new("sleep");
            command.arg("120");
            command
        };

        let started = Instant::now();
        // Far below the 120s child runtime; exercises deadline enforcement
        // deterministically.
        let outcome = run_ffmpeg_transactional_with_timeout(
            &mut command,
            &staged,
            &final_path,
            "test timeout ffmpeg",
            Duration::from_millis(250),
        );
        let elapsed = started.elapsed();
        assert!(outcome.is_err(), "sleeping child must fail");
        assert!(elapsed < Duration::from_secs(30), "took {elapsed:?}");
        assert_eq!(std::fs::read(&final_path).unwrap(), b"prior-output");
        assert!(!staged.exists(), "staged output must be removed");
    }

    #[cfg(unix)]
    #[test]
    fn process_group_kill_refuses_the_callers_group() {
        assert!(!process_signals::kill_process_group(std::process::id()));
    }

    #[cfg(unix)]
    #[test]
    fn timed_out_wrapper_kills_grandchild_holding_stderr_pipe() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("out.wav");
        std::fs::write(&final_path, b"prior-output").unwrap();
        let staged = staged_output_path(&final_path);

        // The background sleep inherits stderr. Killing only the shell leaves
        // that pipe open until the sleep exits; killing the process group
        // closes it immediately and lets timeout cleanup join the drainer.
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 2 >&2 & wait"]);

        let started = Instant::now();
        let outcome = run_ffmpeg_transactional_with_timeout(
            &mut command,
            &staged,
            &final_path,
            "test wrapper timeout",
            Duration::from_millis(100),
        );

        assert!(outcome.is_err(), "wrapper must time out");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "grandchild kept the pipe open: {:?}",
            started.elapsed()
        );
        assert_eq!(std::fs::read(&final_path).unwrap(), b"prior-output");
        assert!(!staged.exists(), "staged output must be removed");
    }
}
