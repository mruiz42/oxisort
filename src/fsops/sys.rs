//! Thin, safe wrappers over the OS primitives std doesn't expose:
//! exclusive clone/rename and metadata-preserving copy.
//!
//! This file and `signal.rs` hold the crate's only `unsafe` code. Each
//! wrapper converts its arguments to C strings, makes one FFI call and maps
//! `-1` to `io::Error::last_os_error()`.

use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

fn cstr(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
}

fn check(ret: libc::c_int) -> io::Result<()> {
    if ret == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Closes `file` and reports the result, which dropping a `File` ignores.
/// NFS and SMB can report delayed write errors only at `close`.
///
/// `EINTR` counts as success: on macOS and Linux the descriptor is released
/// anyway, and retrying could close a descriptor another thread just got.
pub fn close_checked(file: File) -> io::Result<()> {
    use std::os::fd::IntoRawFd;
    let fd = file.into_raw_fd();
    // SAFETY: `fd` came from `into_raw_fd`, so we own it and nothing else
    // closes it; it is not used after this call.
    match check(unsafe { libc::close(fd) }) {
        Err(e) if e.kind() == io::ErrorKind::Interrupted => Ok(()),
        r => r,
    }
}

/// Clones `src` (an open file) to the new path `dst` (APFS copy-on-write).
/// Fails with `EEXIST` if `dst` exists, and `ENOTSUP`/`EXDEV` where cloning
/// isn't possible.
#[cfg(target_os = "macos")]
pub fn clone_file(src: &File, dst: &Path) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let dst = cstr(dst)?;
    // SAFETY: `src` is a valid open fd for the duration of the call and
    // `dst` is a NUL-terminated string that outlives it.
    check(unsafe { libc::fclonefileat(src.as_raw_fd(), libc::AT_FDCWD, dst.as_ptr(), 0) })
}

/// Linux: no clone in this run (FICLONE is filesystem-specific); callers
/// fall back to a data copy.
#[cfg(target_os = "linux")]
pub fn clone_file(_src: &File, _dst: &Path) -> io::Result<()> {
    Err(io::Error::from_raw_os_error(libc::ENOTSUP))
}

/// `copyfile.h`: `COPYFILE_ALL = COPYFILE_METADATA | COPYFILE_DATA`
/// (libc doesn't export the combined constant).
#[cfg(target_os = "macos")]
const COPYFILE_ALL: libc::copyfile_flags_t = libc::COPYFILE_METADATA | libc::COPYFILE_DATA;

/// Copies data and metadata (mode, times, xattrs, ACLs) from `src` into the
/// freshly created, empty `dst`.
#[cfg(target_os = "macos")]
pub fn copy_contents(src: &File, dst: &File, _src_meta: &std::fs::Metadata) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: both fds are open for the duration of the call; a null state
    // pointer is allowed by fcopyfile(3).
    check(unsafe {
        libc::fcopyfile(
            src.as_raw_fd(),
            dst.as_raw_fd(),
            std::ptr::null_mut(),
            COPYFILE_ALL,
        )
    })
}

/// Linux: data via `io::copy` (uses `copy_file_range`), then permissions
/// and times, which `io::copy` doesn't carry.
#[cfg(target_os = "linux")]
pub fn copy_contents(src: &File, dst: &File, src_meta: &std::fs::Metadata) -> io::Result<()> {
    let mut reader = src;
    let mut writer = dst;
    io::copy(&mut reader, &mut writer)?;
    dst.set_permissions(src_meta.permissions())?;
    let mut times = std::fs::FileTimes::new().set_modified(src_meta.modified()?);
    if let Ok(a) = src_meta.accessed() {
        times = times.set_accessed(a);
    }
    dst.set_times(times)
}

/// Renames `src` to `dst` only if `dst` doesn't exist (atomic). Fails with
/// `EEXIST` if it does; `ENOTSUP`/`EINVAL` where the filesystem lacks the
/// primitive (exFAT); `EXDEV` across volumes.
#[cfg(target_os = "macos")]
pub fn rename_noreplace(src: &Path, dst: &Path) -> io::Result<()> {
    let (s, d) = (cstr(src)?, cstr(dst)?);
    // SAFETY: both pointers are valid NUL-terminated strings for the call.
    check(unsafe { libc::renamex_np(s.as_ptr(), d.as_ptr(), libc::RENAME_EXCL) })
}

/// Linux: `renameat2(RENAME_NOREPLACE)`. Written but not run this session
/// (plan decision Q5).
#[cfg(target_os = "linux")]
pub fn rename_noreplace(src: &Path, dst: &Path) -> io::Result<()> {
    let (s, d) = (cstr(src)?, cstr(dst)?);
    // SAFETY: both pointers are valid NUL-terminated strings for the call.
    check(unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            s.as_ptr(),
            libc::AT_FDCWD,
            d.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
compile_error!("fsops::sys has no implementation for this OS yet");
