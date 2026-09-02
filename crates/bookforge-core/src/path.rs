//! Filesystem alias detection shared by every destructive-write entry point
//! (PDF convert, EPUB reflow/rebuild, CLI report writers).
//!
//! Two names are the "same destination" when they resolve to the same file:
//! identical spelled path, a symlink chain to the same inode, a hardlink, or
//! — for a destination that does not exist yet — a lexical spelling whose
//! canonicalized parent-plus-filename collides with another path. On Windows,
//! comparison is additionally case-folded because the filesystem is
//! case-insensitive even though the spelled paths differ.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

/// Upper bound on symlink hops resolved while comparing two paths, mirroring
/// the kernel's own symlink-following limit so a hostile link chain cannot
/// turn path comparison into unbounded work.
const MAX_SYMLINK_HOPS: usize = 40;

/// Returns `true` when `left` and `right` name the same filesystem
/// destination, so callers can reject destructive alias writes up front.
pub fn paths_are_aliases(left: &Path, right: &Path) -> io::Result<bool> {
    let left = comparison_path(left)?;
    let right = comparison_path(right)?;
    if left.canonical == right.canonical {
        return Ok(true);
    }
    if let (Some(a), Some(b)) = (left.identity, right.identity)
        && a == b
    {
        return Ok(true);
    }
    #[cfg(windows)]
    {
        if case_fold(&left.canonical) == case_fold(&right.canonical) {
            return Ok(true);
        }
    }
    Ok(false)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileIdentity {
    volume: u64,
    index: u64,
}

#[derive(Debug)]
struct Comparison {
    canonical: PathBuf,
    identity: Option<FileIdentity>,
}

fn file_identity(metadata: &fs::Metadata) -> Option<FileIdentity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(FileIdentity {
            volume: metadata.dev(),
            index: metadata.ino(),
        })
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // Some Windows filesystems cannot provide a stable file identity.
        // Preserve that absence: synthesizing zeroes would make every pair of
        // such files compare as the same hardlink.
        Some(FileIdentity {
            volume: u64::from(metadata.volume_serial_number()?),
            index: metadata.file_index()?,
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = metadata;
        None
    }
}

/// Resolve a path to its canonical spelling plus, when it exists, the file
/// identity that distinguishes hardlinks from distinct files.
fn comparison_path(path: &Path) -> io::Result<Comparison> {
    let mut current = path.to_path_buf();
    for _ in 0..=MAX_SYMLINK_HOPS {
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let target = fs::read_link(&current)?;
                current = if target.is_absolute() {
                    target
                } else {
                    current
                        .parent()
                        .unwrap_or_else(|| Path::new("."))
                        .join(target)
                };
            }
            Ok(_) => {
                let canonical = fs::canonicalize(&current)?;
                let identity = file_identity(&fs::metadata(&canonical)?);
                return Ok(Comparison {
                    canonical,
                    identity,
                });
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let parent = current.parent().unwrap_or_else(|| Path::new("."));
                let file_name = current.file_name().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("path has no file name: {}", path.display()),
                    )
                })?;
                let canonical = fs::canonicalize(parent)?.join(file_name);
                return Ok(Comparison {
                    canonical,
                    identity: None,
                });
            }
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("too many symlinks while resolving {}", path.display()),
    ))
}

#[cfg(windows)]
fn case_fold(path: &Path) -> String {
    path.to_string_lossy().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_path_is_an_alias() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("source.pdf");
        fs::write(&path, b"data").expect("source writes");
        assert!(paths_are_aliases(&path, &path).expect("compare"));
    }

    #[test]
    fn distinct_paths_are_not_aliases() {
        let dir = tempfile::tempdir().expect("temp dir");
        let a = dir.path().join("a.pdf");
        let b = dir.path().join("b.pdf");
        fs::write(&a, b"a").expect("a writes");
        fs::write(&b, b"b").expect("b writes");
        assert!(!paths_are_aliases(&a, &b).expect("compare"));
    }

    #[test]
    fn missing_path_and_spelled_same_path_are_aliases() {
        let dir = tempfile::tempdir().expect("temp dir");
        let missing = dir.path().join("not-yet.epub");
        assert!(paths_are_aliases(&missing, &missing).expect("compare"));
    }

    #[test]
    fn lexical_dot_aliases_are_detected_for_missing_paths() {
        let dir = tempfile::tempdir().expect("temp dir");
        let plain = dir.path().join("out.epub");
        let dotted = dir.path().join(".").join("out.epub");
        assert!(paths_are_aliases(&plain, &dotted).expect("compare"));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_to_missing_target_is_an_alias() {
        let dir = tempfile::tempdir().expect("temp dir");
        let source = dir.path().join("source.epub");
        let alias = dir.path().join("alias.epub");
        fs::write(&source, b"source").expect("source writes");
        std::os::unix::fs::symlink(&source, &alias).expect("symlink creates");
        assert!(paths_are_aliases(&source, &alias).expect("compare"));
    }

    #[cfg(unix)]
    #[test]
    fn hardlink_aliases_are_detected() {
        let dir = tempfile::tempdir().expect("temp dir");
        let source = dir.path().join("source.pdf");
        let hardlink = dir.path().join("hardlink.pdf");
        fs::write(&source, b"data").expect("source writes");
        fs::hard_link(&source, &hardlink).expect("hardlink creates");
        assert!(paths_are_aliases(&source, &hardlink).expect("compare"));
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symlink_and_its_spelled_target_are_aliases() {
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("missing-target.epub");
        let link = dir.path().join("link.epub");
        std::os::unix::fs::symlink(&target, &link).expect("symlink creates");
        assert!(paths_are_aliases(&link, &target).expect("compare"));
    }

    #[cfg(unix)]
    #[test]
    fn alias_through_a_symlinked_parent_directory_is_detected() {
        let dir = tempfile::tempdir().expect("temp dir");
        fs::create_dir(dir.path().join("real")).expect("real dir creates");
        let plain = dir.path().join("real").join("out.epub");
        fs::write(&plain, b"source").expect("source writes");
        std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("alias"))
            .expect("alias dir symlink creates");

        let through_link = dir.path().join("alias").join("out.epub");
        assert!(paths_are_aliases(&plain, &through_link).expect("compare"));
    }

    #[cfg(unix)]
    #[test]
    fn missing_output_through_a_symlinked_parent_is_an_alias() {
        let dir = tempfile::tempdir().expect("temp dir");
        fs::create_dir(dir.path().join("real")).expect("real dir creates");
        let source = dir.path().join("real").join("source.epub");
        fs::write(&source, b"source").expect("source writes");
        std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("alias"))
            .expect("alias dir symlink creates");

        // The destination does not exist yet, but its canonicalized parent
        // resolves through the symlink to the directory holding the source.
        let pending_output = dir.path().join("alias").join("out.epub");
        let spelled_source = dir.path().join("real").join("out.epub");
        assert!(paths_are_aliases(&spelled_source, &pending_output).expect("compare"));
    }
}
