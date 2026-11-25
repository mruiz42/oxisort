//! No-clobber file operations: copy, move, hard link, symlink.
//!
//! Every write to a destination is exclusive, so a file that appears at a
//! planned destination after planning (another process, a sync client) is
//! never overwritten; the operation fails with
//! [`FsError::DestinationAppeared`] instead. The one exception is the
//! check-then-rename fallback for volumes without an atomic no-replace rename
//! (exFAT; plan decision Q1), which the caller reports with one warning.

mod sys;

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// A failed file operation. The source path is added by the caller's log
/// line, so variants carry only what else is needed.
#[derive(Debug, thiserror::Error)]
pub enum FsError {
    #[error("destination appeared after planning: {}", .0.display())]
    DestinationAppeared(PathBuf),
    #[error("hard links can't cross volumes")]
    CrossDeviceLink,
    #[error("copy has {got} bytes, source has {want}; source kept")]
    SizeMismatch { got: u64, want: u64 },
    #[error("copied, but removing the source failed: {0}")]
    SourceNotRemoved(io::Error),
    #[error("{op}: {source}")]
    Io {
        op: &'static str,
        #[source]
        source: io::Error,
    },
}

impl FsError {
    fn io(op: &'static str) -> impl FnOnce(io::Error) -> FsError {
        move |source| FsError::Io { op, source }
    }

    /// True for "no space left" / "quota exceeded", which stop the run.
    pub fn is_disk_full(&self) -> bool {
        let raw = match self {
            FsError::Io { source, .. } | FsError::SourceNotRemoved(source) => source.raw_os_error(),
            _ => None,
        };
        matches!(raw, Some(libc::ENOSPC) | Some(libc::EDQUOT))
    }
}

/// How a copy was carried out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyHow {
    /// APFS clone (instant, shares blocks).
    Cloned,
    /// Full data copy.
    Copied,
}

/// How a move was carried out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveHow {
    /// Atomic no-replace rename.
    Renamed,
    /// Check-then-rename on a volume without atomic no-replace (exFAT).
    RenamedUnchecked,
    /// Different volume: copy, fsync, verify, delete the source.
    CopiedAcrossDevices,
}

fn appeared_or<'a>(op: &'static str, dst: &'a Path) -> impl FnOnce(io::Error) -> FsError + 'a {
    move |e| {
        if e.kind() == io::ErrorKind::AlreadyExists {
            FsError::DestinationAppeared(dst.to_path_buf())
        } else {
            FsError::Io { op, source: e }
        }
    }
}

/// Copies `src` to the new path `dst`, never replacing an existing file.
///
/// Tries an APFS clone first (keeps all metadata), then reserves `dst` with
/// `O_EXCL` and copies data and metadata into it. A partial destination left
/// by a failed copy is removed.
///
/// # Errors
/// [`FsError::DestinationAppeared`] if `dst` exists; [`FsError::Io`] otherwise.
pub fn copy_file(src: &Path, dst: &Path) -> Result<CopyHow, FsError> {
    copy_exclusive(src, dst, true).map(|(how, _)| how)
}

/// The copy engine. Returns the open destination when it did a data copy,
/// so a cross-device move can fsync it.
fn copy_exclusive(
    src: &Path,
    dst: &Path,
    allow_clone: bool,
) -> Result<(CopyHow, Option<File>), FsError> {
    let src_file = File::open(src).map_err(FsError::io("open source"))?;
    let meta = src_file.metadata().map_err(FsError::io("stat source"))?;
    if allow_clone {
        match sys::clone_file(&src_file, dst) {
            Ok(()) => return Ok((CopyHow::Cloned, None)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                return Err(FsError::DestinationAppeared(dst.to_path_buf()));
            }
            // Not clonable here (other volume, other filesystem): copy data.
            Err(e)
                if matches!(
                    e.raw_os_error(),
                    Some(libc::ENOTSUP | libc::EXDEV | libc::EINVAL | libc::ENOSYS)
                ) => {}
            Err(e) => {
                return Err(FsError::Io {
                    op: "clone",
                    source: e,
                });
            }
        }
    }
    let dst_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(meta.mode() & 0o7777)
        .open(dst)
        .map_err(appeared_or("create destination", dst))?;
    if let Err(e) = sys::copy_contents(&src_file, &dst_file, &meta) {
        drop(dst_file);
        remove_partial(dst);
        return Err(FsError::Io {
            op: "copy",
            source: e,
        });
    }
    Ok((CopyHow::Copied, Some(dst_file)))
}

/// Removes a destination we created ourselves after a failure. Best effort:
/// the original error is what gets reported.
fn remove_partial(dst: &Path) {
    if let Err(e) = fs::remove_file(dst) {
        log::warn!("could not remove partial file {}: {e}", dst.display());
    }
}

/// Moves `src` to the new path `dst`, never replacing an existing file.
///
/// Same volume: atomic no-replace rename (check-then-rename where the
/// filesystem lacks it). Different volume: no-clobber copy, `fsync`
/// (F_FULLFSYNC on macOS), size check, then delete the source.
///
/// # Errors
/// [`FsError::DestinationAppeared`] if `dst` exists; [`FsError::SizeMismatch`]
/// (source kept) if the copy doesn't verify; [`FsError::SourceNotRemoved`]
/// if the copy succeeded but the source couldn't be deleted.
pub fn move_file(src: &Path, dst: &Path) -> Result<MoveHow, FsError> {
    move_file_with(src, dst, false, &|_| {})
}

/// [`move_file`] with test seams: `force_cross_device` drives the copy path
/// on one volume, and `after_copy` runs between copy and verification.
pub(crate) fn move_file_with(
    src: &Path,
    dst: &Path,
    force_cross_device: bool,
    after_copy: &dyn Fn(&Path),
) -> Result<MoveHow, FsError> {
    if !force_cross_device {
        match sys::rename_noreplace(src, dst) {
            Ok(()) => return Ok(MoveHow::Renamed),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                return Err(FsError::DestinationAppeared(dst.to_path_buf()));
            }
            Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {}
            Err(e)
                if matches!(
                    e.raw_os_error(),
                    Some(libc::ENOTSUP | libc::EINVAL | libc::ENOSYS)
                ) =>
            {
                return rename_checked(src, dst);
            }
            Err(e) => {
                return Err(FsError::Io {
                    op: "rename",
                    source: e,
                });
            }
        }
    }
    move_across_devices(src, dst, after_copy)
}

/// Check-then-rename (plan decision Q1). Not atomic against other processes;
/// oxisort itself never targets one name twice.
fn rename_checked(src: &Path, dst: &Path) -> Result<MoveHow, FsError> {
    if fs::symlink_metadata(dst).is_ok() {
        return Err(FsError::DestinationAppeared(dst.to_path_buf()));
    }
    fs::rename(src, dst).map_err(FsError::io("rename"))?;
    Ok(MoveHow::RenamedUnchecked)
}

fn move_across_devices(
    src: &Path,
    dst: &Path,
    after_copy: &dyn Fn(&Path),
) -> Result<MoveHow, FsError> {
    let want = fs::metadata(src).map_err(FsError::io("stat source"))?.len();
    let (_, file) = copy_exclusive(src, dst, false)?;
    let file = file.expect("a non-clone copy returns the destination file");
    if let Err(e) = file.sync_all() {
        drop(file);
        remove_partial(dst);
        return Err(FsError::Io {
            op: "fsync",
            source: e,
        });
    }
    drop(file);
    sync_parent(dst);
    after_copy(dst);
    let got = fs::metadata(dst).map(|m| m.len()).unwrap_or(u64::MAX);
    if got != want {
        remove_partial(dst);
        return Err(FsError::SizeMismatch { got, want });
    }
    fs::remove_file(src).map_err(FsError::SourceNotRemoved)?;
    Ok(MoveHow::CopiedAcrossDevices)
}

/// Best-effort fsync of the folder holding `path`, so the new directory
/// entry is durable too. Some filesystems reject fsync on folders (EINVAL);
/// on macOS F_FULLFSYNC on the file already flushed the device cache.
fn sync_parent(path: &Path) {
    if let Some(parent) = path.parent() {
        if let Ok(dir) = File::open(parent) {
            let _ = dir.sync_all();
        }
    }
}

/// Hard-links `src` at `dst` (never replaces).
///
/// # Errors
/// [`FsError::DestinationAppeared`], [`FsError::CrossDeviceLink`] or
/// [`FsError::Io`].
pub fn hard_link(src: &Path, dst: &Path) -> Result<(), FsError> {
    fs::hard_link(src, dst).map_err(|e| match e.kind() {
        io::ErrorKind::AlreadyExists => FsError::DestinationAppeared(dst.to_path_buf()),
        io::ErrorKind::CrossesDevices => FsError::CrossDeviceLink,
        _ => FsError::Io {
            op: "link",
            source: e,
        },
    })
}

/// Creates a symlink at `dst` pointing to the absolute path `src` (decision
/// Q3: absolute, so the links survive moving the output tree).
///
/// # Errors
/// [`FsError::DestinationAppeared`] or [`FsError::Io`].
pub fn symlink(src: &Path, dst: &Path) -> Result<(), FsError> {
    debug_assert!(src.is_absolute(), "symlink targets are absolute");
    std::os::unix::fs::symlink(src, dst).map_err(appeared_or("symlink", dst))
}

/// `(st_dev, st_ino)` of a path without following symlinks.
pub fn dev_ino(path: &Path) -> io::Result<(u64, u64)> {
    fs::symlink_metadata(path).map(|m| (m.dev(), m.ino()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn setup() -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("src.bin");
        fs::write(&src, b"hello world").unwrap();
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_420_167_845);
        File::options()
            .write(true)
            .open(&src)
            .unwrap()
            .set_modified(t)
            .unwrap();
        (d, src)
    }

    #[test]
    fn copy_preserves_bytes_and_mtime() {
        let (d, src) = setup();
        let dst = d.path().join("dst.bin");
        copy_file(&src, &dst).unwrap();
        assert_eq!(fs::read(&dst).unwrap(), b"hello world");
        let m = |p: &Path| fs::metadata(p).unwrap().modified().unwrap();
        assert_eq!(m(&src), m(&dst));
    }

    #[test]
    fn data_copy_path_preserves_mtime() {
        let (d, src) = setup();
        let dst = d.path().join("dst.bin");
        let (how, _) = copy_exclusive(&src, &dst, false).unwrap();
        assert_eq!(how, CopyHow::Copied);
        let m = |p: &Path| fs::metadata(p).unwrap().modified().unwrap();
        assert_eq!(m(&src), m(&dst));
        assert_eq!(fs::read(&dst).unwrap(), b"hello world");
    }

    #[test]
    fn copy_never_overwrites() {
        let (d, src) = setup();
        let dst = d.path().join("dst.bin");
        fs::write(&dst, b"precious").unwrap();
        for allow_clone in [true, false] {
            let err = copy_exclusive(&src, &dst, allow_clone).unwrap_err();
            assert!(matches!(err, FsError::DestinationAppeared(_)), "{err}");
            assert_eq!(fs::read(&dst).unwrap(), b"precious");
        }
    }

    #[test]
    fn move_renames_and_never_overwrites() {
        let (d, src) = setup();
        let taken = d.path().join("taken.bin");
        fs::write(&taken, b"precious").unwrap();
        assert!(matches!(
            move_file(&src, &taken),
            Err(FsError::DestinationAppeared(_))
        ));
        assert_eq!(fs::read(&taken).unwrap(), b"precious");
        assert!(src.exists());

        let dst = d.path().join("dst.bin");
        assert_eq!(move_file(&src, &dst).unwrap(), MoveHow::Renamed);
        assert!(!src.exists());
        assert_eq!(fs::read(&dst).unwrap(), b"hello world");
    }

    #[test]
    fn cross_device_move_copies_then_deletes() {
        let (d, src) = setup();
        let dst = d.path().join("dst.bin");
        let how = move_file_with(&src, &dst, true, &|_| {}).unwrap();
        assert_eq!(how, MoveHow::CopiedAcrossDevices);
        assert!(!src.exists());
        assert_eq!(fs::read(&dst).unwrap(), b"hello world");
    }

    #[test]
    fn cross_device_size_mismatch_keeps_source() {
        let (d, src) = setup();
        let dst = d.path().join("dst.bin");
        let truncate = |p: &Path| fs::write(p, b"short").unwrap();
        let err = move_file_with(&src, &dst, true, &truncate).unwrap_err();
        assert!(
            matches!(err, FsError::SizeMismatch { got: 5, want: 11 }),
            "{err}"
        );
        assert_eq!(fs::read(&src).unwrap(), b"hello world");
        assert!(!dst.exists(), "bad copy is removed");
    }

    #[test]
    fn cross_device_move_never_overwrites() {
        let (d, src) = setup();
        let taken = d.path().join("taken.bin");
        fs::write(&taken, b"precious").unwrap();
        let err = move_file_with(&src, &taken, true, &|_| {}).unwrap_err();
        assert!(matches!(err, FsError::DestinationAppeared(_)));
        assert_eq!(fs::read(&taken).unwrap(), b"precious");
        assert!(src.exists());
    }

    #[test]
    fn checked_rename_fallback() {
        let (d, src) = setup();
        let taken = d.path().join("taken.bin");
        fs::write(&taken, b"precious").unwrap();
        assert!(matches!(
            rename_checked(&src, &taken),
            Err(FsError::DestinationAppeared(_))
        ));
        let dst = d.path().join("dst.bin");
        assert_eq!(
            rename_checked(&src, &dst).unwrap(),
            MoveHow::RenamedUnchecked
        );
        assert!(dst.exists() && !src.exists());
    }

    #[test]
    fn links() {
        let (d, src) = setup();
        let hl = d.path().join("hard.bin");
        hard_link(&src, &hl).unwrap();
        assert_eq!(dev_ino(&src).unwrap(), dev_ino(&hl).unwrap());
        assert!(matches!(
            hard_link(&src, &hl),
            Err(FsError::DestinationAppeared(_))
        ));

        let sl = d.path().join("soft.bin");
        symlink(&src, &sl).unwrap();
        assert_eq!(fs::read_link(&sl).unwrap(), src);
        assert!(matches!(
            symlink(&src, &sl),
            Err(FsError::DestinationAppeared(_))
        ));
    }

    #[test]
    fn disk_full_detection() {
        let full = FsError::Io {
            op: "copy",
            source: io::Error::from_raw_os_error(libc::ENOSPC),
        };
        assert!(full.is_disk_full());
        assert!(!FsError::CrossDeviceLink.is_disk_full());
    }
}
