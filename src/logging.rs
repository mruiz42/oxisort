//! Output routing: a small `log::Log` implementation plus progress bars.
//!
//! - Per-file action lines (target [`ACTION`]) go to stdout, unless
//!   `--quiet` or `--progress`.
//! - The summary (target [`SUMMARY`]) goes to stderr, unless `--quiet`.
//! - Everything else goes to stderr by level (warn/error always, debug with
//!   `-v`, trace with `-vv`).
//! - With `--log`, every info-and-above line (debug with `-v`) is also
//!   appended to the file with an RFC 3339 timestamp, whatever the console
//!   settings.
//!
//! Console writes go through `MultiProgress::suspend` so bars aren't torn.

use std::fs::{File, OpenOptions};
use std::io::{self, LineWriter, Write};
use std::path::Path;
use std::sync::Mutex;

use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use log::{Level, LevelFilter, Log, Metadata, Record};

use crate::model::Verbosity;

/// Log target for per-file action lines.
pub const ACTION: &str = "oxisort::action";
/// Log target for the end-of-run summary.
pub const SUMMARY: &str = "oxisort::summary";
/// Log target for lines that only go to the `--log` file (run header).
pub const HEADER: &str = "oxisort::header";

/// The process-wide logger.
pub struct Logger {
    console_level: LevelFilter,
    actions_to_stdout: bool,
    summary_to_console: bool,
    file: Option<Mutex<LineWriter<File>>>,
    file_level: LevelFilter,
    progress: Option<MultiProgress>,
}

impl Logger {
    /// Builds the logger. Opens `log_file` for append.
    ///
    /// # Errors
    /// The I/O error from opening the log file.
    pub fn new(
        verbosity: Verbosity,
        progress: Option<MultiProgress>,
        log_file: Option<&Path>,
    ) -> io::Result<Self> {
        let console_level = match verbosity {
            Verbosity::Quiet => LevelFilter::Warn,
            Verbosity::Normal => LevelFilter::Info,
            Verbosity::Verbose(1) => LevelFilter::Debug,
            Verbosity::Verbose(_) => LevelFilter::Trace,
        };
        let file = log_file
            .map(|p| OpenOptions::new().create(true).append(true).open(p))
            .transpose()?
            .map(|f| Mutex::new(LineWriter::new(f)));
        Ok(Logger {
            console_level,
            actions_to_stdout: verbosity != Verbosity::Quiet && progress.is_none(),
            summary_to_console: verbosity != Verbosity::Quiet,
            file,
            file_level: console_level.max(LevelFilter::Info),
            progress,
        })
    }

    /// Installs this logger as the global `log` backend.
    ///
    /// # Errors
    /// If a logger was already installed.
    pub fn install(self) -> Result<(), log::SetLoggerError> {
        let max = self.console_level.max(self.file_level);
        // Leaked on purpose: the logger lives for the whole process.
        log::set_logger(Box::leak(Box::new(self)))?;
        log::set_max_level(max);
        Ok(())
    }

    fn console(&self, to_stdout: bool, text: &str) {
        let write = || {
            // Ignore write errors (e.g. a closed pipe from `| head`): output
            // is best-effort and must never abort a half-done transfer.
            if to_stdout {
                let _ = writeln!(io::stdout().lock(), "{text}");
            } else {
                let _ = writeln!(io::stderr().lock(), "{text}");
            }
        };
        match &self.progress {
            Some(mp) => mp.suspend(write),
            None => write(),
        }
    }

    fn to_file(&self, level: Level, text: &str) {
        let Some(file) = &self.file else { return };
        let ts = chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, false);
        let mut f = file.lock().unwrap_or_else(|p| p.into_inner());
        for line in text.lines() {
            if let Err(e) = writeln!(f, "{ts} {level:<5} {line}") {
                drop(f);
                self.console(
                    false,
                    &format!("oxisort: warning: writing log file failed: {e}"),
                );
                return;
            }
        }
    }
}

impl Log for Logger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= self.console_level.max(self.file_level)
    }

    fn log(&self, record: &Record<'_>) {
        let level = record.level();
        let text = record.args().to_string();
        match record.target() {
            ACTION => {
                if self.actions_to_stdout {
                    self.console(true, &text);
                }
            }
            SUMMARY => {
                if self.summary_to_console {
                    self.console(false, &text);
                }
            }
            HEADER => {}
            _ if level <= self.console_level => {
                let prefix = match level {
                    Level::Error => "oxisort: error: ",
                    Level::Warn => "oxisort: warning: ",
                    _ => "",
                };
                self.console(false, &format!("{prefix}{text}"));
            }
            _ => {}
        }
        if level <= self.file_level {
            self.to_file(level, &text);
        }
    }

    fn flush(&self) {
        let _ = io::stdout().flush();
        if let Some(f) = &self.file {
            let _ = f.lock().unwrap_or_else(|p| p.into_inner()).flush();
        }
    }
}

/// Progress bars for the run phases; every method returns a hidden bar when
/// `--progress` is off, so callers never branch.
#[derive(Clone, Default)]
pub struct Progress {
    mp: Option<MultiProgress>,
}

impl Progress {
    /// Enabled progress on stderr. indicatif draws nothing when stderr is not
    /// a terminal, so redirected output stays clean.
    pub fn enabled() -> Self {
        Progress {
            mp: Some(MultiProgress::with_draw_target(ProgressDrawTarget::stderr())),
        }
    }

    /// Disabled progress: all bars are hidden.
    pub fn disabled() -> Self {
        Progress { mp: None }
    }

    /// The shared `MultiProgress`, for the logger.
    pub fn multi(&self) -> Option<MultiProgress> {
        self.mp.clone()
    }

    /// A spinner for a phase of unknown length.
    pub fn spinner(&self, msg: &'static str) -> ProgressBar {
        let Some(mp) = &self.mp else {
            return ProgressBar::hidden();
        };
        let pb = mp.add(ProgressBar::new_spinner());
        pb.set_style(
            ProgressStyle::with_template("{spinner} {msg} {pos}").expect("valid template"),
        );
        pb.set_message(msg);
        pb.enable_steady_tick(std::time::Duration::from_millis(120));
        pb
    }

    /// A counted bar; `{msg}` is free for a rate the caller updates.
    pub fn bar(&self, len: u64, msg: &'static str) -> ProgressBar {
        let Some(mp) = &self.mp else {
            return ProgressBar::hidden();
        };
        let pb = mp.add(ProgressBar::new(len));
        pb.set_style(
            ProgressStyle::with_template("{prefix:>16} [{bar:30}] {pos}/{len} {msg} {eta}")
                .expect("valid template")
                .progress_chars("=> "),
        );
        pb.set_prefix(msg);
        pb
    }
}
