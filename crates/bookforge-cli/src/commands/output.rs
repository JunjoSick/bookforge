use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use bookforge_core::path::paths_are_aliases;

/// Compare filesystem aliases, including a not-yet-created destination whose
/// parent is a symlink. This prevents commands from destroying their source
/// when users spell the same file through different paths (symlink, hardlink,
/// or lexical aliasing).
pub(crate) fn ensure_distinct_paths(label: &str, left: &Path, right: &Path) -> Result<()> {
    if paths_are_aliases(left, right)
        .with_context(|| format!("resolving {label} path pair ({left:?}, {right:?})"))?
    {
        bail!(
            "{label} paths must be different: {} / {}",
            left.display(),
            right.display()
        );
    }
    Ok(())
}

/// Write an artifact away from its final path, then publish it only after the
/// complete byte stream has been produced. Existing good reports remain in
/// place if serialization or writing fails.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating output directory {}", parent.display()))?;
    }
    let (staged, mut file) = create_sibling_file(path, "report")?;
    let result = (|| -> Result<()> {
        file.write_all(bytes)
            .with_context(|| format!("writing temporary report {}", staged.display()))?;
        file.sync_all()
            .with_context(|| format!("flushing temporary report {}", staged.display()))?;
        drop(file);
        publish(&staged, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&staged);
    }
    result
}

fn create_sibling_file(path: &Path, label: &str) -> Result<(PathBuf, File)> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("bookforge-report.json");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for attempt in 0..128u32 {
        let candidate = path.with_file_name(format!(
            ".{name}.bookforge-{label}-{}-{nonce}-{attempt}",
            std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    bail!(
        "could not reserve a temporary report path beside {}",
        path.display()
    );
}

#[cfg(unix)]
fn publish(staged: &Path, destination: &Path) -> Result<()> {
    // Unix rename replaces the destination atomically. The old report remains
    // untouched until the complete staged byte stream has been flushed.
    fs::rename(staged, destination).with_context(|| {
        format!(
            "publishing temporary artifact {} as {}",
            staged.display(),
            destination.display()
        )
    })?;
    Ok(())
}

#[cfg(not(unix))]
fn publish(staged: &Path, destination: &Path) -> Result<()> {
    if !destination.exists() {
        fs::rename(staged, destination).with_context(|| {
            format!(
                "publishing temporary artifact {} as {}",
                staged.display(),
                destination.display()
            )
        })?;
        return Ok(());
    }

    let backup = destination.with_file_name(format!(
        ".{}.bookforge-report-backup-{}",
        destination
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("report"),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    fs::rename(destination, &backup).with_context(|| {
        format!(
            "staging existing report destination {}",
            destination.display()
        )
    })?;
    match fs::rename(staged, destination) {
        Ok(()) => {
            let _ = fs::remove_file(backup);
            Ok(())
        }
        Err(error) => {
            let _ = fs::rename(&backup, destination);
            Err(error).with_context(|| format!("publishing report {}", destination.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_write_publishes_complete_bytes() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("report.json");
        write_atomic(&path, br#"{"ok":true}"#).expect("report writes");
        assert_eq!(fs::read(&path).expect("report reads"), br#"{"ok":true}"#);
    }

    #[test]
    fn failed_report_publication_restores_existing_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let destination = dir.path().join("report.json");
        let staged = dir.path().join("missing-stage.json");
        fs::write(&destination, b"known-good").expect("existing report writes");

        let error = publish(&staged, &destination).expect_err("missing stage must fail");

        assert!(!error.to_string().is_empty());
        assert_eq!(
            fs::read(&destination).expect("existing report remains"),
            b"known-good"
        );
    }

    #[cfg(unix)]
    #[test]
    fn aliases_through_symlinks_are_rejected() {
        let dir = tempfile::tempdir().expect("temp dir");
        let source = dir.path().join("source.epub");
        let alias = dir.path().join("alias.epub");
        fs::write(&source, b"source").expect("source writes");
        std::os::unix::fs::symlink(&source, &alias).expect("symlink creates");
        let error = ensure_distinct_paths("input/output", &source, &alias)
            .expect_err("symlink aliases must be rejected");
        assert!(error.to_string().contains("must be different"));
    }

    #[cfg(unix)]
    #[test]
    fn hardlink_aliases_are_rejected() {
        let dir = tempfile::tempdir().expect("temp dir");
        let source = dir.path().join("source.pdf");
        let hardlink = dir.path().join("alias.pdf");
        fs::write(&source, b"source").expect("source writes");
        fs::hard_link(&source, &hardlink).expect("hardlink creates");
        let error = ensure_distinct_paths("input/output", &source, &hardlink)
            .expect_err("hardlink aliases must be rejected");
        assert!(error.to_string().contains("must be different"));
    }

    #[test]
    fn lexical_dot_aliases_are_rejected() {
        let dir = tempfile::tempdir().expect("temp dir");
        let output = dir.path().join("out.epub");
        let lexical = dir.path().join(".").join("out.epub");
        let error = ensure_distinct_paths("input/output", &output, &lexical)
            .expect_err("lexical aliases must be rejected");
        assert!(error.to_string().contains("must be different"));
    }

    #[test]
    fn missing_source_and_report_path_are_still_rejected_as_aliases() {
        let dir = tempfile::tempdir().expect("temp dir");
        let source = dir.path().join("missing.epub");

        let error = ensure_distinct_paths("input/report", &source, &source)
            .expect_err("equal missing paths must be rejected");
        assert!(error.to_string().contains("must be different"));
    }
}
