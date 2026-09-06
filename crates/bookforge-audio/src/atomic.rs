//! Atomic publication of completed audiobook artifacts.
//!
//! Unix can atomically rename a file over an existing destination. Windows
//! needs the replacement operation provided by the OS for the same
//! failure-preserving behavior; moving the destination aside first would
//! leave a crash window where a completed artifact is temporarily absent.

use std::fs;
use std::io;
use std::path::Path;

/// Publish `staged` at `destination` without exposing a partial file.
///
/// On failure, an existing destination remains in place. On success, the
/// staged path is consumed by the replacement operation.
pub fn replace_file(staged: &Path, destination: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        // ReplaceFileW requires an existing destination. The rename fast path
        // also avoids an unnecessary FFI call for first publication. If the
        // destination appears between exists() and rename(), fall through to
        // ReplaceFileW and let it perform the replacement.
        if !destination.exists() {
            match fs::rename(staged, destination) {
                Ok(()) => return Ok(()),
                Err(error) if error.kind() != io::ErrorKind::AlreadyExists => {
                    return Err(error);
                }
                Err(_) => {}
            }
        }
        return replace_file_windows(staged, destination);
    }

    #[cfg(not(windows))]
    {
        fs::rename(staged, destination)
    }
}

#[cfg(windows)]
fn replace_file_windows(staged: &Path, destination: &Path) -> io::Result<()> {
    use std::iter::once;
    use std::os::windows::ffi::OsStrExt;

    let staged: Vec<u16> = staged.as_os_str().encode_wide().chain(once(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(once(0))
        .collect();

    // ReplaceFileW replaces the destination while preserving it if the
    // operation fails. The replacement and destination must be on one volume,
    // which is guaranteed because callers create staged siblings.
    let replaced = unsafe {
        replace_file_windows_raw(
            destination.as_ptr(),
            staged.as_ptr(),
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if replaced == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    #[link_name = "ReplaceFileW"]
    fn replace_file_windows_raw(
        replaced_file_name: *const u16,
        replacement_file_name: *const u16,
        backup_file_name: *const u16,
        replace_flags: u32,
        exclude: *mut std::ffi::c_void,
        reserved: *mut std::ffi::c_void,
    ) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacement_replaces_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("artifact.bin");
        let staged = dir.path().join("artifact.bin.part");
        fs::write(&destination, b"old").unwrap();
        fs::write(&staged, b"new").unwrap();

        replace_file(&staged, &destination).unwrap();

        assert_eq!(fs::read(&destination).unwrap(), b"new");
        assert!(!staged.exists());
    }

    #[test]
    fn repeated_replacement_does_not_leave_intermediate_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("artifact.bin");

        for value in 0u8..8 {
            let staged = dir.path().join(format!("artifact-{value}.part"));
            fs::write(&staged, [value]).unwrap();
            replace_file(&staged, &destination).unwrap();
            assert_eq!(fs::read(&destination).unwrap(), [value]);
            assert!(!staged.exists());
        }

        let entries: Vec<_> = fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(entries.len(), 1, "only the published artifact may remain");
    }

    #[test]
    fn failed_replacement_preserves_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("artifact.bin");
        let staged = dir.path().join("broken.part");
        fs::write(&destination, b"old").unwrap();
        fs::create_dir(&staged).unwrap();

        assert!(replace_file(&staged, &destination).is_err());
        assert_eq!(fs::read(&destination).unwrap(), b"old");
    }
}
