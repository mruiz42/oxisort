//! No-clobber, crash-safe file operations: copy, move, hard link, symlink.
//!
//! Every write to a destination is exclusive, so a file that appears at a
//! planned destination after planning (another process, a sync client) is
//! never overwritten; the operation fails with
//! [`FsError::DestinationAppeared`] instead. The one exception is the
//! check-then-rename fallback for volumes without an atomic no-replace rename
//! (exFAT; plan decision Q1), which the caller reports with one warning.
//!
//! Data copies are written to a hidden temp name in the target folder and
//! renamed into place only when complete, so a crash leaves at most a
//! `.oxisort-tmp-*` file, never a truncated file under a real name.

mod sys;

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

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
    #[error("copy verification failed (blake3 mismatch); source kept")]
    VerifyFailed,
    #[error("copy verification failed: {0}; source kept")]
    Verify(io::Error),
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

/// Name prefix of the temp files copies are written to before they are
/// renamed into place. Shared with the scan and the OUT walk, which skip
/// (and report) leftovers from an interrupted run instead of sorting them.
pub const TEMP_PREFIX: &str = ".oxisort-tmp-";

/// True for a temp file left by an interrupted oxisort run.
pub fn is_temp_leftover(name: &OsStr) -> bool {
    name.as_bytes().starts_with(TEMP_PREFIX.as_bytes())
}

/// How hard a write tries to survive a crash or power loss, and to prove a
/// copy before its source is deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteOpts {
    /// `--durable`: fsync every file oxisort writes (F_FULLFSYNC on macOS)
    /// before it gets its final name, and fsync the folder holding every
    /// name oxisort creates. Cross-device moves always do this.
    pub durable: bool,
    /// Re-read a cross-device copy from the device and compare its blake3
    /// with the source's before the source is deleted (off: `--no-verify`,
    /// which keeps only the size check).
    pub verify: bool,
}

impl Default for WriteOpts {
    fn default() -> Self {
        WriteOpts {
            durable: false,
            verify: true,
        }
    }
}

/// How a copy was carried out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyHow {
    /// APFS clone (instant, shares blocks).
    Cloned,
    /// Full data copy into a temp file, then an atomic no-replace rename.
    Copied,
    /// Full data copy, placed with check-then-rename on a volume without an
    /// atomic no-replace rename (exFAT).
    CopiedUnchecked,
}

/// How a move was carried out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveHow {
    /// Atomic no-replace rename.
    Renamed,
    /// Check-then-rename on a volume without atomic no-replace (exFAT).
    RenamedUnchecked,
    /// Different volume: copy to a temp name, fsync, verify, rename into
    /// place, delete the source.
    CopiedAcrossDevices,
    /// As `CopiedAcrossDevices`, but the temp was placed with
    /// check-then-rename (exFAT destination).
    CopiedAcrossDevicesUnchecked,
}

impl MoveHow {
    /// True when the final name was claimed with check-then-rename.
    pub fn is_unchecked(self) -> bool {
        matches!(
            self,
            MoveHow::RenamedUnchecked | MoveHow::CopiedAcrossDevicesUnchecked
        )
    }
}

/// A test hook run on a finished temp file.
pub(crate) type TempHook<'a> = &'a dyn Fn(&Path) -> io::Result<()>;

/// Test seams for the write paths. Production code uses the default.
#[derive(Default)]
pub(crate) struct Seam<'a> {
    /// Drive the cross-device copy path on one volume.
    pub force_cross_device: bool,
    /// Place temps with check-then-rename, as on exFAT.
    pub force_unchecked: bool,
    /// Runs on the finished temp file, before it is checked and renamed;
    /// an error counts as a failed copy.
    pub after_copy: Option<TempHook<'a>>,
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
/// Tries an APFS clone first (atomic, keeps all metadata). Otherwise the
/// data and metadata go to a hidden temp file in `dst`'s folder, which is
/// renamed to `dst` only once complete, so a crash never leaves a partial
/// file under the final name (issue #1). Temps are removed on failure.
///
/// # Errors
/// [`FsError::DestinationAppeared`] if `dst` exists; [`FsError::Io`] otherwise.
pub fn copy_file(src: &Path, dst: &Path, opts: &WriteOpts) -> Result<CopyHow, FsError> {
    write_new(src, dst, opts, Kind::Copy, &Seam::default()).map(|(how, _)| how)
}

/// What `write_new` is writing for, which decides cloning and syncing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A copy: may clone, syncs only under `durable`.
    Copy,
    /// The copy half of a cross-device move: never clones, always syncs
    /// (the source is deleted afterwards), and checks the copy (size, then
    /// blake3 under `verify`) before it gets its final name.
    Move,
}

/// The one engine behind every data-writing operation: clone straight to
/// `dst`, or copy into a temp file in `dst`'s folder and rename it into
/// place with no-replace semantics. The final name is created only by the
/// clone or the rename, never by a partial write. Returns the blake3 of the
/// bytes when a move verified them.
fn write_new(
    src: &Path,
    dst: &Path,
    opts: &WriteOpts,
    kind: Kind,
    seam: &Seam<'_>,
) -> Result<(CopyHow, Option<blake3::Hash>), FsError> {
    let src_file = File::open(src).map_err(FsError::io("open source"))?;
    let meta = src_file.metadata().map_err(FsError::io("stat source"))?;
    if kind == Kind::Copy {
        match sys::clone_file(&src_file, dst) {
            Ok(()) => {
                if opts.durable {
                    sync_path(dst)?;
                    sync_parent(dst);
                }
                return Ok((CopyHow::Cloned, None));
            }
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
    let (temp, temp_file) = create_temp(dst, meta.mode() & 0o7777)?;
    // From here on, every failure removes the temp: only a crash leaves it.
    let fail = |e: FsError| {
        remove_partial(&temp);
        e
    };
    let verify = kind == Kind::Move && opts.verify;
    if verify {
        // macOS: written uncached, so the verify re-read hits the device.
        sys::set_nocache(&temp_file).map_err(|e| fail(FsError::io("set F_NOCACHE")(e)))?;
    }
    sys::copy_contents(&src_file, &temp_file, &meta).map_err(|e| fail(FsError::io("copy")(e)))?;
    let sync = opts.durable || kind == Kind::Move;
    if sync {
        temp_file
            .sync_all()
            .map_err(|e| fail(FsError::io("fsync")(e)))?;
    }
    if verify {
        // Linux: drop the (now clean) pages for the same reason.
        sys::drop_cache(&temp_file).map_err(|e| fail(FsError::io("drop cache")(e)))?;
    }
    // Delayed write errors (NFS, SMB) surface only at close.
    sys::close_checked(temp_file).map_err(|e| fail(FsError::io("close")(e)))?;
    if let Some(after_copy) = seam.after_copy {
        after_copy(&temp).map_err(|e| fail(FsError::io("copy")(e)))?;
    }
    let mut verified = None;
    if kind == Kind::Move {
        let got = fs::metadata(&temp).map(|m| m.len()).unwrap_or(u64::MAX);
        if got != meta.len() {
            return Err(fail(FsError::SizeMismatch {
                got,
                want: meta.len(),
            }));
        }
        if verify {
            verified = Some(verify_copy(&temp, src).map_err(fail)?);
        }
    }
    let checked = place(&temp, dst, seam.force_unchecked).map_err(fail)?;
    if sync {
        sync_parent(dst);
    }
    let how = if checked {
        CopyHow::Copied
    } else {
        CopyHow::CopiedUnchecked
    };
    Ok((how, verified))
}

/// Issue #2: hashes the copy (from the device: it was written uncached or
/// its pages were dropped) and the source (normally still cached from the
/// copy a moment ago), and requires them to match. Returns the hash.
fn verify_copy(copy: &Path, src: &Path) -> Result<blake3::Hash, FsError> {
    let got = hash_with_buf(copy).map_err(FsError::Verify)?;
    let want = hash_with_buf(src).map_err(FsError::Verify)?;
    if got == want {
        Ok(got)
    } else {
        Err(FsError::VerifyFailed)
    }
}

/// [`crate::hash::hash_file`] with a per-thread buffer, so concurrent
/// transfers don't each allocate 1 MiB per file.
pub fn hash_with_buf(path: &Path) -> io::Result<blake3::Hash> {
    thread_local! {
        static BUF: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    BUF.with(|b| crate::hash::hash_file(path, &mut b.borrow_mut()))
}

/// Process-wide counter that makes temp names unique between threads.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Creates `dst.parent()/.oxisort-tmp-<pid>-<n>-<nanos>` exclusively, with
/// the given permission bits. The pid and counter make names unique across
/// processes and threads; the clock part avoids reusing a leftover's name
/// after a crash. `EEXIST` is retried a few times anyway.
fn create_temp(dst: &Path, mode: u32) -> Result<(PathBuf, File), FsError> {
    let dir = dst.parent().unwrap_or(Path::new("."));
    let mut last = None;
    for _ in 0..16 {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp = dir.join(format!("{TEMP_PREFIX}{}-{n}-{nanos}", std::process::id()));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&temp)
        {
            Ok(f) => return Ok((temp, f)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => {
                return Err(FsError::Io {
                    op: "create temp file",
                    source: e,
                });
            }
        }
    }
    Err(FsError::Io {
        op: "create temp file",
        source: last.unwrap_or_else(|| io::Error::from(io::ErrorKind::AlreadyExists)),
    })
}

/// Renames the finished temp to `dst` without ever replacing a file there.
/// Returns false when it had to use check-then-rename (exFAT), which the
/// caller reports once per run.
fn place(temp: &Path, dst: &Path, force_unchecked: bool) -> Result<bool, FsError> {
    if force_unchecked {
        return rename_checked(temp, dst).map(|()| false);
    }
    match sys::rename_noreplace(temp, dst) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            Err(FsError::DestinationAppeared(dst.to_path_buf()))
        }
        Err(e)
            if matches!(
                e.raw_os_error(),
                Some(libc::ENOTSUP | libc::EINVAL | libc::ENOSYS)
            ) =>
        {
            rename_checked(temp, dst).map(|()| false)
        }
        Err(e) => Err(FsError::Io {
            op: "rename",
            source: e,
        }),
    }
}

/// Removes a file we created ourselves after a failure. Best effort: the
/// original error is what gets reported.
fn remove_partial(path: &Path) {
    if let Err(e) = fs::remove_file(path) {
        log::warn!("could not remove partial file {}: {e}", path.display());
    }
}

/// Moves `src` to the new path `dst`, never replacing an existing file.
///
/// Same volume: atomic no-replace rename (check-then-rename where the
/// filesystem lacks it). Different volume: copy to a temp name next to
/// `dst`, `fsync` (F_FULLFSYNC on macOS), size check, blake3 check of the
/// copy re-read from the device against the source (unless
/// `opts.verify` is off), rename into place, then delete the source.
///
/// Returns how it moved and, for a verified cross-device copy, the hash.
///
/// # Errors
/// [`FsError::DestinationAppeared`] if `dst` exists; [`FsError::SizeMismatch`],
/// [`FsError::VerifyFailed`] or [`FsError::Verify`] (source kept) if the
/// copy doesn't verify; [`FsError::SourceNotRemoved`] if the copy succeeded
/// but the source couldn't be deleted.
pub fn move_file(
    src: &Path,
    dst: &Path,
    opts: &WriteOpts,
) -> Result<(MoveHow, Option<blake3::Hash>), FsError> {
    move_file_with(src, dst, opts, &Seam::default())
}

/// [`move_file`] with test seams.
pub(crate) fn move_file_with(
    src: &Path,
    dst: &Path,
    opts: &WriteOpts,
    seam: &Seam<'_>,
) -> Result<(MoveHow, Option<blake3::Hash>), FsError> {
    if !seam.force_cross_device {
        let how = match sys::rename_noreplace(src, dst) {
            Ok(()) => MoveHow::Renamed,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                return Err(FsError::DestinationAppeared(dst.to_path_buf()));
            }
            Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
                return move_across_devices(src, dst, opts, seam);
            }
            Err(e)
                if matches!(
                    e.raw_os_error(),
                    Some(libc::ENOTSUP | libc::EINVAL | libc::ENOSYS)
                ) =>
            {
                rename_checked(src, dst)?;
                MoveHow::RenamedUnchecked
            }
            Err(e) => {
                return Err(FsError::Io {
                    op: "rename",
                    source: e,
                });
            }
        };
        if opts.durable {
            sync_parent(dst);
        }
        return Ok((how, None));
    }
    move_across_devices(src, dst, opts, seam)
}

/// Check-then-rename (plan decision Q1). Not atomic against other processes;
/// oxisort itself never targets one name twice.
fn rename_checked(src: &Path, dst: &Path) -> Result<(), FsError> {
    if fs::symlink_metadata(dst).is_ok() {
        return Err(FsError::DestinationAppeared(dst.to_path_buf()));
    }
    fs::rename(src, dst).map_err(FsError::io("rename"))
}

fn move_across_devices(
    src: &Path,
    dst: &Path,
    opts: &WriteOpts,
    seam: &Seam<'_>,
) -> Result<(MoveHow, Option<blake3::Hash>), FsError> {
    let (how, verified) = write_new(src, dst, opts, Kind::Move, seam)?;
    fs::remove_file(src).map_err(FsError::SourceNotRemoved)?;
    let how = if how == CopyHow::CopiedUnchecked {
        MoveHow::CopiedAcrossDevicesUnchecked
    } else {
        MoveHow::CopiedAcrossDevices
    };
    Ok((how, verified))
}

/// fsyncs an existing file by path (a clone, which we never opened).
fn sync_path(path: &Path) -> Result<(), FsError> {
    let file = File::open(path).map_err(FsError::io("open for fsync"))?;
    file.sync_all().map_err(FsError::io("fsync"))
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
pub fn hard_link(src: &Path, dst: &Path, opts: &WriteOpts) -> Result<(), FsError> {
    fs::hard_link(src, dst).map_err(|e| match e.kind() {
        io::ErrorKind::AlreadyExists => FsError::DestinationAppeared(dst.to_path_buf()),
        io::ErrorKind::CrossesDevices => FsError::CrossDeviceLink,
        _ => FsError::Io {
            op: "link",
            source: e,
        },
    })?;
    if opts.durable {
        sync_parent(dst);
    }
    Ok(())
}

/// Creates a symlink at `dst` pointing to the absolute path `src` (decision
/// Q3: absolute, so the links survive moving the output tree).
///
/// # Errors
/// [`FsError::DestinationAppeared`] or [`FsError::Io`].
pub fn symlink(src: &Path, dst: &Path, opts: &WriteOpts) -> Result<(), FsError> {
    debug_assert!(src.is_absolute(), "symlink targets are absolute");
    std::os::unix::fs::symlink(src, dst).map_err(appeared_or("symlink", dst))?;
    if opts.durable {
        sync_parent(dst);
    }
    Ok(())
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

    const OPTS: WriteOpts = WriteOpts {
        durable: false,
        verify: true,
    };
    const NO_VERIFY: WriteOpts = WriteOpts {
        durable: false,
        verify: false,
    };

    fn copy_seam(seam: &Seam<'_>, src: &Path, dst: &Path) -> Result<CopyHow, FsError> {
        write_new(src, dst, &OPTS, Kind::Copy, seam).map(|(how, _)| how)
    }

    /// The data-copy path (no clone), as on another volume or on Linux.
    fn data_copy(src: &Path, dst: &Path, seam: &Seam<'_>) -> Result<CopyHow, FsError> {
        write_new(src, dst, &OPTS, Kind::Move, seam).map(|(how, _)| how)
    }

    /// Names in `dir` that are oxisort temp files.
    fn temps(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| {
                let name = e.unwrap().file_name();
                is_temp_leftover(&name).then(|| name.to_string_lossy().into_owned())
            })
            .collect()
    }

    fn mtime(p: &Path) -> SystemTime {
        fs::metadata(p).unwrap().modified().unwrap()
    }

    #[test]
    fn copy_preserves_bytes_and_mtime() {
        let (d, src) = setup();
        let dst = d.path().join("dst.bin");
        copy_file(&src, &dst, &OPTS).unwrap();
        assert_eq!(fs::read(&dst).unwrap(), b"hello world");
        assert_eq!(mtime(&src), mtime(&dst));
        assert!(temps(d.path()).is_empty());
    }

    #[test]
    fn data_copy_path_preserves_mtime_and_mode_and_leaves_no_temp() {
        use std::os::unix::fs::PermissionsExt;
        let (d, src) = setup();
        fs::set_permissions(&src, fs::Permissions::from_mode(0o640)).unwrap();
        let dst = d.path().join("dst.bin");
        assert_eq!(
            data_copy(&src, &dst, &Seam::default()).unwrap(),
            CopyHow::Copied
        );
        assert_eq!(mtime(&src), mtime(&dst));
        assert_eq!(fs::read(&dst).unwrap(), b"hello world");
        let mode = fs::metadata(&dst).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
        assert!(temps(d.path()).is_empty());
    }

    #[test]
    fn copy_never_overwrites() {
        let (d, src) = setup();
        let dst = d.path().join("dst.bin");
        fs::write(&dst, b"precious").unwrap();
        for kind in [Kind::Copy, Kind::Move] {
            let err = write_new(&src, &dst, &OPTS, kind, &Seam::default()).unwrap_err();
            assert!(matches!(err, FsError::DestinationAppeared(_)), "{err}");
            assert_eq!(fs::read(&dst).unwrap(), b"precious");
            assert!(temps(d.path()).is_empty(), "temp removed");
        }
    }

    /// Issue #1: a copy that fails part-way (here: after the data is
    /// written, before the rename) never leaves a file under the final
    /// name, and removes its temp.
    #[test]
    fn failed_copy_leaves_no_final_name_and_no_temp() {
        let (d, src) = setup();
        let dst = d.path().join("dst.bin");
        let boom = |_: &Path| -> io::Result<()> { Err(io::Error::other("injected")) };
        let seam = Seam {
            after_copy: Some(&boom),
            ..Seam::default()
        };
        let err = data_copy(&src, &dst, &seam).unwrap_err();
        assert!(err.to_string().contains("injected"), "{err}");
        assert!(!dst.exists());
        assert!(temps(d.path()).is_empty());
        assert_eq!(fs::read(&src).unwrap(), b"hello world");
    }

    /// The temp is complete before the final name exists: the seam sees it
    /// while `dst` is still absent.
    #[test]
    fn final_name_appears_only_after_the_copy_is_complete() {
        let (d, src) = setup();
        let dst = d.path().join("dst.bin");
        let check = |temp: &Path| -> io::Result<()> {
            assert!(is_temp_leftover(temp.file_name().unwrap()));
            assert_eq!(fs::read(temp)?, b"hello world");
            assert!(!temp.with_file_name("dst.bin").exists());
            Ok(())
        };
        let seam = Seam {
            after_copy: Some(&check),
            ..Seam::default()
        };
        data_copy(&src, &dst, &seam).unwrap();
        assert_eq!(fs::read(&dst).unwrap(), b"hello world");
    }

    /// No-clobber still holds when the destination appears while the temp
    /// is being written.
    #[test]
    fn destination_appearing_mid_copy_is_never_replaced() {
        let (d, src) = setup();
        let dst = d.path().join("dst.bin");
        let appear = |temp: &Path| fs::write(temp.with_file_name("dst.bin"), b"theirs");
        for force_unchecked in [false, true] {
            let _ = fs::remove_file(&dst);
            let seam = Seam {
                after_copy: Some(&appear),
                force_unchecked,
                ..Seam::default()
            };
            let err = data_copy(&src, &dst, &seam).unwrap_err();
            assert!(matches!(err, FsError::DestinationAppeared(_)), "{err}");
            assert_eq!(fs::read(&dst).unwrap(), b"theirs");
            assert!(temps(d.path()).is_empty());
        }
    }

    /// The exFAT fallback reports itself and still refuses an existing name.
    #[test]
    fn unchecked_placement_is_reported() {
        let (d, src) = setup();
        let dst = d.path().join("dst.bin");
        let seam = Seam {
            force_unchecked: true,
            ..Seam::default()
        };
        assert_eq!(
            data_copy(&src, &dst, &seam).unwrap(),
            CopyHow::CopiedUnchecked
        );
        assert_eq!(fs::read(&dst).unwrap(), b"hello world");
        let err = copy_seam(&seam, &src, &dst).unwrap_err();
        assert!(matches!(err, FsError::DestinationAppeared(_)), "{err}");

        let moved = d.path().join("moved.bin");
        let seam = Seam {
            force_cross_device: true,
            force_unchecked: true,
            ..Seam::default()
        };
        let (how, _) = move_file_with(&src, &moved, &OPTS, &seam).unwrap();
        assert_eq!(how, MoveHow::CopiedAcrossDevicesUnchecked);
        assert!(how.is_unchecked());
    }

    #[test]
    fn temp_leftover_names() {
        assert!(is_temp_leftover(OsStr::new(".oxisort-tmp-1-0-0")));
        assert!(is_temp_leftover(OsStr::new(".oxisort-tmp-")));
        assert!(!is_temp_leftover(OsStr::new("oxisort-tmp-1")));
        assert!(!is_temp_leftover(OsStr::new("._.oxisort-tmp-1")));
        assert!(!is_temp_leftover(OsStr::new("IMG_0001.jpg")));
    }

    /// `--durable` changes only how often data is flushed (not observable
    /// here); every operation must still work with it.
    #[test]
    fn durable_operations_succeed() {
        let (d, src) = setup();
        let opts = WriteOpts {
            durable: true,
            verify: true,
        };
        let p = |n: &str| d.path().join(n);
        copy_file(&src, &p("c.bin"), &opts).unwrap();
        write_new(&src, &p("d.bin"), &opts, Kind::Move, &Seam::default()).unwrap();
        hard_link(&src, &p("h.bin"), &opts).unwrap();
        symlink(&src, &p("s.bin"), &opts).unwrap();
        move_file(&p("c.bin"), &p("m.bin"), &opts).unwrap();
        let seam = Seam {
            force_cross_device: true,
            ..Seam::default()
        };
        move_file_with(&p("m.bin"), &p("x.bin"), &opts, &seam).unwrap();
        assert_eq!(fs::read(p("x.bin")).unwrap(), b"hello world");
        assert!(temps(d.path()).is_empty());
    }

    #[test]
    fn move_renames_and_never_overwrites() {
        let (d, src) = setup();
        let taken = d.path().join("taken.bin");
        fs::write(&taken, b"precious").unwrap();
        assert!(matches!(
            move_file(&src, &taken, &OPTS),
            Err(FsError::DestinationAppeared(_))
        ));
        assert_eq!(fs::read(&taken).unwrap(), b"precious");
        assert!(src.exists());

        let dst = d.path().join("dst.bin");
        assert_eq!(
            move_file(&src, &dst, &OPTS).unwrap(),
            (MoveHow::Renamed, None),
            "same-volume renames are not hashed"
        );
        assert!(!src.exists());
        assert_eq!(fs::read(&dst).unwrap(), b"hello world");
    }

    fn cross() -> Seam<'static> {
        Seam {
            force_cross_device: true,
            ..Seam::default()
        }
    }

    #[test]
    fn cross_device_move_copies_then_deletes() {
        let (d, src) = setup();
        let dst = d.path().join("dst.bin");
        let (how, hash) = move_file_with(&src, &dst, &OPTS, &cross()).unwrap();
        assert_eq!(how, MoveHow::CopiedAcrossDevices);
        assert_eq!(hash, Some(blake3::hash(b"hello world")));
        assert!(!src.exists());
        assert_eq!(fs::read(&dst).unwrap(), b"hello world");
        assert!(temps(d.path()).is_empty());
    }

    #[test]
    fn cross_device_size_mismatch_keeps_source() {
        for opts in [OPTS, NO_VERIFY] {
            let (d, src) = setup();
            let dst = d.path().join("dst.bin");
            let truncate = |p: &Path| fs::write(p, b"short");
            let seam = Seam {
                after_copy: Some(&truncate),
                ..cross()
            };
            let err = move_file_with(&src, &dst, &opts, &seam).unwrap_err();
            assert!(
                matches!(err, FsError::SizeMismatch { got: 5, want: 11 }),
                "{err}"
            );
            assert_eq!(fs::read(&src).unwrap(), b"hello world");
            assert!(!dst.exists(), "bad copy never gets the final name");
            assert!(temps(d.path()).is_empty(), "bad copy is removed");
        }
    }

    /// Issue #2: a same-size corruption of the copy is caught by the
    /// blake3 re-read; the source is kept and nothing gets the final name.
    #[test]
    fn cross_device_corruption_is_caught_by_verification() {
        let (d, src) = setup();
        let dst = d.path().join("dst.bin");
        let flip = |p: &Path| fs::write(p, b"hello_world");
        let seam = Seam {
            after_copy: Some(&flip),
            ..cross()
        };
        let err = move_file_with(&src, &dst, &OPTS, &seam).unwrap_err();
        assert!(matches!(err, FsError::VerifyFailed), "{err}");
        assert!(err.to_string().contains("source kept"), "{err}");
        assert_eq!(fs::read(&src).unwrap(), b"hello world");
        assert!(!dst.exists());
        assert!(temps(d.path()).is_empty());
    }

    /// `--no-verify`: the old behaviour, size check only.
    #[test]
    fn no_verify_skips_the_hash_check() {
        let (d, src) = setup();
        let dst = d.path().join("dst.bin");
        let flip = |p: &Path| fs::write(p, b"hello_world");
        let seam = Seam {
            after_copy: Some(&flip),
            ..cross()
        };
        let (how, hash) = move_file_with(&src, &dst, &NO_VERIFY, &seam).unwrap();
        assert_eq!((how, hash), (MoveHow::CopiedAcrossDevices, None));
        assert!(!src.exists());
    }

    #[test]
    fn cross_device_move_never_overwrites() {
        let (d, src) = setup();
        let taken = d.path().join("taken.bin");
        fs::write(&taken, b"precious").unwrap();
        let err = move_file_with(&src, &taken, &OPTS, &cross()).unwrap_err();
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
        rename_checked(&src, &dst).unwrap();
        assert!(dst.exists() && !src.exists());
    }

    #[test]
    fn links() {
        let (d, src) = setup();
        let hl = d.path().join("hard.bin");
        hard_link(&src, &hl, &OPTS).unwrap();
        assert_eq!(dev_ino(&src).unwrap(), dev_ino(&hl).unwrap());
        assert!(matches!(
            hard_link(&src, &hl, &OPTS),
            Err(FsError::DestinationAppeared(_))
        ));

        let sl = d.path().join("soft.bin");
        symlink(&src, &sl, &OPTS).unwrap();
        assert_eq!(fs::read_link(&sl).unwrap(), src);
        assert!(matches!(
            symlink(&src, &sl, &OPTS),
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
