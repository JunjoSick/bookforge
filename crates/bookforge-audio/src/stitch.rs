//! Optional ffmpeg-based audiobook post-processing.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::builder::{AudiobookManifest, ChunkRecord};
use crate::text::ChunkKind;

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

pub fn ffmpeg_available() -> bool {
    tool_available("ffmpeg")
}

pub fn ffprobe_available() -> bool {
    tool_available("ffprobe")
}

fn tool_available(tool: &str) -> bool {
    Command::new(tool)
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

pub fn stitch(manifest: &AudiobookManifest, options: &StitchOptions) -> StitchReport {
    let mut report = StitchReport::default();
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
    if requested_gaps {
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
                Ok(path) => report.book_file = Some(path),
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
    let mut command = Command::new("ffmpeg");
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
) -> std::result::Result<PathBuf, String> {
    if chapter_files.is_empty() {
        return Err("no chapter files were produced; cannot assemble m4b".into());
    }
    let list_name = "book.concat.txt";
    let meta_name = "chapters.ffmeta.txt";
    let result = (|| {
        let chapter_names: Vec<String> = chapters
            .iter()
            .map(|(index, parts)| {
                parts
                    .first()
                    .map(|part| part.chapter_title.clone())
                    .unwrap_or_else(|| format!("Chapter {}", index + 1))
            })
            .collect();
        let durations = chapter_files
            .iter()
            .map(|path| ffprobe_duration_ms(path))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| {
                "chapter metadata was skipped because ffprobe could not determine every chapter duration; skipped m4b assembly rather than publishing an unchaptered book"
                    .to_string()
            })?;
        let chapter_gap_ms = gaps.chapter.as_ref().map_or(0, |_| options.gap_chapter_ms);
        let metadata = build_ffmetadata(
            options.title.as_deref(),
            &chapter_names,
            &durations,
            chapter_gap_ms,
        );
        std::fs::write(options.out_dir.join(meta_name), metadata)
            .map_err(|error| format!("writing chapter metadata: {error}"))?;

        let entries = build_book_concat_entries(chapter_files, gaps.chapter.as_deref());
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
            "-map_metadata".to_string(),
            "1".to_string(),
        ];
        if options.loudnorm {
            args.extend(["-af".to_string(), "loudnorm=I=-18:TP=-2:LRA=11".to_string()]);
        }
        args.extend([
            "-c:a".to_string(),
            "aac".to_string(),
            "-b:a".to_string(),
            "128k".to_string(),
        ]);
        append_metadata_args(
            &mut args,
            options.title.as_deref(),
            options.author.as_deref(),
        );
        let output_path = options.out_dir.join("audiobook.m4b");
        let staged = staged_output_path(&output_path);
        args.push(file_name_of(&staged));
        let mut command = Command::new("ffmpeg");
        command.current_dir(&options.out_dir).args(&args);
        run_ffmpeg_transactional(&mut command, &staged, &output_path, "ffmpeg m4b assembly")
            .map(|()| output_path)
    })();
    let _ = std::fs::remove_file(options.out_dir.join(list_name));
    let _ = std::fs::remove_file(options.out_dir.join(meta_name));
    result
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
    let mut command = Command::new("ffmpeg");
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

pub fn single_file_ffmpeg_args(
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
    let result = run_ffmpeg(command, context);
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

fn run_ffmpeg(command: &mut Command, context: &str) -> std::result::Result<(), String> {
    command.stdout(Stdio::null()).stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| format!("launching {context}: {error}"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| format!("capturing {context} stderr failed"))?;
    let stderr_reader = std::thread::spawn(move || bounded_stderr_tail(stderr));
    let status = child
        .wait()
        .map_err(|error| format!("waiting for {context}: {error}"))?;
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

fn publish_staged_file(staged: &Path, final_path: &Path) -> std::result::Result<(), String> {
    if !final_path.exists() {
        return std::fs::rename(staged, final_path).map_err(|error| {
            format!("publishing ffmpeg output {}: {error}", final_path.display())
        });
    }

    let sequence = STAGED_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let file_name = final_path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    let backup = final_path.with_file_name(format!(
        ".{file_name}.{}-{sequence}.replace.bak",
        std::process::id()
    ));
    std::fs::rename(final_path, &backup).map_err(|error| {
        format!(
            "staging previous ffmpeg output {}: {error}",
            final_path.display()
        )
    })?;
    match std::fs::rename(staged, final_path) {
        Ok(()) => {
            let _ = std::fs::remove_file(backup);
            Ok(())
        }
        Err(error) => {
            let _ = std::fs::rename(&backup, final_path);
            Err(format!(
                "publishing ffmpeg output {}: {error}",
                final_path.display()
            ))
        }
    }
}

fn probe_audio_params(path: &Path) -> Option<(u32, u16)> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            "stream=sample_rate,channels",
            "-of",
            "json",
        ])
        .arg(path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let stream = value.get("streams")?.as_array()?.first()?;
    let rate = stream.get("sample_rate")?.as_str()?.parse().ok()?;
    let channels = u16::try_from(stream.get("channels")?.as_u64()?).ok()?;
    Some((rate, channels))
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
    let mut command = Command::new("ffmpeg");
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
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=nokey=1:noprint_wrappers=1",
        ])
        .arg(path)
        .output()
        .ok()?;
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
    let trimmed = if trimmed.is_empty() {
        "untitled".to_string()
    } else {
        trimmed
    };
    trimmed.chars().take(40).collect::<String>().to_lowercase()
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
        assert_eq!(sanitize_filename("***"), "untitled");
        assert_eq!(sanitize_filename("  spaced  out  "), "spaced-out");
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
}
