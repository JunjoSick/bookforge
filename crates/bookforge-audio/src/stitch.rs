//! Optional post-processing that joins the many per-chunk files into one
//! file per chapter, and (when possible) a single `.m4b` with chapter
//! markers. Everything here shells out to `ffmpeg`/`ffprobe`; if those
//! tools are absent the functions return warnings instead of failing, so a
//! build that produced per-chunk files is never lost to a missing binary.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::builder::{AudiobookManifest, ChunkRecord};

#[derive(Debug, Clone)]
pub struct StitchOptions {
    pub out_dir: PathBuf,
    /// File extension of the per-chunk files (e.g. "mp3", "wav").
    pub extension: String,
    /// Also assemble a single `.m4b` with chapter markers.
    pub make_m4b: bool,
    pub title: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct StitchReport {
    pub chapter_files: Vec<PathBuf>,
    pub book_file: Option<PathBuf>,
    pub warnings: Vec<String>,
}

/// True if `ffmpeg` answers `-version`.
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

/// Concatenate per-chunk files into one file per chapter, then optionally a
/// single `.m4b`. Requires `ffmpeg` to be on PATH.
pub fn stitch(manifest: &AudiobookManifest, options: &StitchOptions) -> StitchReport {
    let mut report = StitchReport::default();

    if !ffmpeg_available() {
        report.warnings.push(
            "ffmpeg not found on PATH; skipped stitching (per-chunk files are intact)".into(),
        );
        return report;
    }

    let chapters = group_by_chapter(&manifest.chunks);
    for (chapter_index, parts) in &chapters {
        let title = parts
            .first()
            .map(|part| part.chapter_title.clone())
            .unwrap_or_else(|| format!("Chapter {}", chapter_index + 1));
        let file_names: Vec<String> = parts.iter().map(|part| part.file.clone()).collect();
        let output_name = format!(
            "chapter-{:03}-{}.{}",
            chapter_index + 1,
            sanitize_filename(&title),
            options.extension
        );
        match concat_copy(&options.out_dir, &file_names, &output_name) {
            Ok(()) => report
                .chapter_files
                .push(options.out_dir.join(&output_name)),
            Err(error) => report
                .warnings
                .push(format!("chapter {}: {error}", chapter_index + 1)),
        }
    }

    if options.make_m4b && report.chapter_files.len() != chapters.len() {
        report.warnings.push(format!(
            "only {}/{} chapters stitched successfully; skipped m4b assembly to avoid an incomplete audiobook",
            report.chapter_files.len(),
            chapters.len()
        ));
    } else if options.make_m4b {
        match assemble_m4b(options, &chapters, &report.chapter_files) {
            Ok(path) => report.book_file = Some(path),
            Err(error) => report.warnings.push(error),
        }
    }

    report
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

/// Run the ffmpeg concat demuxer with stream copy. `cwd` is set to the
/// output dir so the list file can use bare relative names.
fn concat_copy(dir: &Path, inputs: &[String], output: &str) -> std::result::Result<(), String> {
    let list_name = format!("{output}.concat.txt");
    let list_content = concat_list_content(inputs);
    let list_path = dir.join(&list_name);
    std::fs::write(&list_path, list_content)
        .map_err(|error| format!("writing concat list: {error}"))?;

    let status = Command::new("ffmpeg")
        .current_dir(dir)
        .args(["-y", "-f", "concat", "-safe", "0", "-i"])
        .arg(&list_name)
        .args(["-c", "copy"])
        .arg(output)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|error| format!("launching ffmpeg: {error}"))?;

    let _ = std::fs::remove_file(&list_path);

    if status.success() {
        Ok(())
    } else {
        Err(format!("ffmpeg concat exited with {status}"))
    }
}

/// Assemble a single `.m4b` from the per-chapter files, adding chapter
/// markers when `ffprobe` can measure durations. Falls back to a
/// marker-less m4b if `ffprobe` is missing.
fn assemble_m4b(
    options: &StitchOptions,
    chapters: &[(usize, Vec<ChunkRecord>)],
    chapter_files: &[PathBuf],
) -> std::result::Result<PathBuf, String> {
    if chapter_files.is_empty() {
        return Err("no chapter files were produced; cannot assemble m4b".into());
    }

    let chapter_names: Vec<String> = chapters
        .iter()
        .map(|(index, parts)| {
            parts
                .first()
                .map(|part| part.chapter_title.clone())
                .unwrap_or_else(|| format!("Chapter {}", index + 1))
        })
        .collect();

    let file_names: Vec<String> = chapter_files
        .iter()
        .map(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default()
        })
        .collect();

    // Durations power the chapter markers. Without ffprobe we still emit a
    // playable m4b, just without a chapter list.
    let mut ffmeta_arg: Vec<String> = Vec::new();
    if ffprobe_available() {
        let mut durations_ms = Vec::with_capacity(chapter_files.len());
        for path in chapter_files {
            match ffprobe_duration_ms(path) {
                Some(ms) => durations_ms.push(ms),
                None => {
                    durations_ms.clear();
                    break;
                }
            }
        }
        if durations_ms.len() == chapter_names.len() {
            let meta = build_ffmetadata(options.title.as_deref(), &chapter_names, &durations_ms);
            let meta_name = "chapters.ffmeta.txt";
            std::fs::write(options.out_dir.join(meta_name), meta)
                .map_err(|error| format!("writing chapter metadata: {error}"))?;
            ffmeta_arg = vec![meta_name.to_string()];
        }
    }

    let list_content = concat_list_content(&file_names);
    let list_name = "book.concat.txt";
    std::fs::write(options.out_dir.join(list_name), list_content)
        .map_err(|error| format!("writing book concat list: {error}"))?;

    let output = "audiobook.m4b";
    let mut command = Command::new("ffmpeg");
    command
        .current_dir(&options.out_dir)
        .args(["-y", "-f", "concat", "-safe", "0", "-i"])
        .arg(list_name);
    if let Some(meta_name) = ffmeta_arg.first() {
        command.args(["-i", meta_name, "-map_metadata", "1"]);
    }
    command
        .args(["-c:a", "aac", "-b:a", "128k"])
        .arg(output)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let status = command
        .status()
        .map_err(|error| format!("launching ffmpeg for m4b: {error}"))?;

    let _ = std::fs::remove_file(options.out_dir.join(list_name));
    let _ = std::fs::remove_file(options.out_dir.join("chapters.ffmeta.txt"));

    if status.success() {
        Ok(options.out_dir.join(output))
    } else {
        Err(format!("ffmpeg m4b assembly exited with {status}"))
    }
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
    let text = String::from_utf8_lossy(&output.stdout);
    let seconds: f64 = text.trim().parse().ok()?;
    Some((seconds * 1000.0).round() as u64)
}

/// Build the content of an ffmpeg concat-demuxer list. Single quotes in
/// names are escaped per ffmpeg's rules (`'` becomes `'\''`).
pub fn concat_list_content(files: &[String]) -> String {
    let mut out = String::new();
    for file in files {
        let escaped = file.replace('\'', "'\\''");
        out.push_str(&format!("file '{escaped}'\n"));
    }
    out
}

/// Build an FFMETADATA1 document with one `[CHAPTER]` per entry. `durations`
/// are per-chapter lengths in milliseconds; chapter start/end times are the
/// running cumulative sum.
pub fn build_ffmetadata(title: Option<&str>, names: &[String], durations: &[u64]) -> String {
    let mut out = String::from(";FFMETADATA1\n");
    if let Some(title) = title {
        out.push_str(&format!("title={}\n", escape_ffmeta(title)));
    }
    let mut start = 0u64;
    for (name, duration) in names.iter().zip(durations.iter()) {
        let end = start + duration;
        out.push_str("\n[CHAPTER]\nTIMEBASE=1/1000\n");
        out.push_str(&format!("START={start}\n"));
        out.push_str(&format!("END={end}\n"));
        out.push_str(&format!("title={}\n", escape_ffmeta(name)));
        start = end;
    }
    out
}

fn escape_ffmeta(value: &str) -> String {
    // ffmetadata treats =, ;, #, \, and newlines as special; escape with \.
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

/// Make a title safe for use as a filename component.
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

    fn chunk(chapter_index: usize, part: usize) -> ChunkRecord {
        ChunkRecord {
            chapter_index,
            chapter_title: format!("Chapter {}", chapter_index + 1),
            part,
            file: format!("c{chapter_index}-p{part}.wav"),
            chars: 10,
            synthesis_sha256: "0".repeat(64),
            status: crate::ChunkStatus::Synthesized,
            audio_sha256: Some("1".repeat(64)),
            bytes: Some(44),
            error: None,
        }
    }

    #[test]
    fn concat_list_escapes_single_quotes() {
        let content = concat_list_content(&["it's here.mp3".to_string(), "plain.mp3".to_string()]);
        assert_eq!(content, "file 'it'\\''s here.mp3'\nfile 'plain.mp3'\n");
    }

    #[test]
    fn chapter_parts_are_sorted_before_stitching() {
        let grouped = group_by_chapter(&[chunk(0, 2), chunk(1, 1), chunk(0, 1)]);
        assert_eq!(grouped[0].1[0].part, 1);
        assert_eq!(grouped[0].1[1].part, 2);
    }

    #[test]
    fn ffmetadata_has_cumulative_chapter_times() {
        let meta = build_ffmetadata(
            Some("My Book"),
            &["Intro".to_string(), "One".to_string()],
            &[1_000, 2_500],
        );
        assert!(meta.starts_with(";FFMETADATA1\n"));
        assert!(meta.contains("title=My Book"));
        assert!(meta.contains("START=0\nEND=1000\ntitle=Intro"));
        assert!(meta.contains("START=1000\nEND=3500\ntitle=One"));
    }

    #[test]
    fn ffmetadata_escapes_special_characters() {
        let meta = build_ffmetadata(None, &["A = B; C".to_string()], &[10]);
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
}
