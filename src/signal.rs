//! SIGINT handling: the first Ctrl-C asks the run to stop gracefully (tasks
//! in flight finish, the rest are reported as not run, exit 130); the
//! handler is installed with `SA_RESETHAND`, so a second Ctrl-C gets the
//! default action and ends the process immediately. `SA_RESTART` keeps a
//! worker blocked in `read`/`write` (inside `fcopyfile`) from failing with
//! `EINTR`, so the copies in flight really do finish.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigint(_signal: libc::c_int) {
    // Only an atomic store: async-signal-safe.
    INTERRUPTED.store(true, Ordering::SeqCst);
}

/// Installs the SIGINT handler.
///
/// # Errors
/// The OS error if `sigaction` fails.
pub fn install() -> io::Result<()> {
    // SAFETY: `sigaction` is plain data, so all-zero is a valid value; the
    // mask is initialised with sigemptyset before use, and the handler is
    // an `extern "C"` fn that only performs an atomic store.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_sigint as libc::sighandler_t;
        action.sa_flags = libc::SA_RESETHAND | libc::SA_RESTART;
        libc::sigemptyset(&mut action.sa_mask);
        if libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut()) == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// True once SIGINT has been received.
pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}
