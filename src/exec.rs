//! Phase 5: carry out a [`Plan`] (or, for dry-run, describe it).
//!
//! Order of work:
//! 1. create every target folder (serial, sorted);
//! 2. wave 1: transfers (media + sidecars) in parallel on the I/O pool;
//! 3. wave 2: duplicates, which may depend on a wave-1 transfer.
//!
//! Workers never choose names (the plan fixed them), so the only contention
//! left is with *other* processes, which the no-clobber primitives catch.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::time::Instant;

use indicatif::ProgressBar;
use rayon::prelude::*;

use crate::fsops::{self, FsError, MoveHow};
use crate::logging::{ACTION, Progress};
use crate::model::{Action, Mode, NotRun, Outcome};
use crate::plan::{self, Plan};

/// Cooperative stop flag: set by SIGINT or a full destination; checked by
/// every task before it starts. Tasks already running finish, so a
/// cross-device move is never cut in half.
#[derive(Debug, Default)]
pub struct Stop {
    reason: AtomicU8,
}

const STOP_NONE: u8 = 0;
const STOP_INTERRUPTED: u8 = 1;
const STOP_DISK_FULL: u8 = 2;

impl Stop {
    pub fn new() -> Self {
        Stop::default()
    }

    /// Requests a stop; the first reason wins.
    pub fn request(&self, why: NotRun) {
        let code = match why {
            NotRun::DiskFull => STOP_DISK_FULL,
            _ => STOP_INTERRUPTED,
        };
        let _ = self
            .reason
            .compare_exchange(STOP_NONE, code, Ordering::SeqCst, Ordering::SeqCst);
    }

    /// Why the run is stopping, if it is. Also picks up SIGINT.
    pub fn reason(&self) -> Option<NotRun> {
        if crate::signal::interrupted() {
            self.request(NotRun::Interrupted);
        }
        match self.reason.load(Ordering::SeqCst) {
            STOP_INTERRUPTED => Some(NotRun::Interrupted),
            STOP_DISK_FULL => Some(NotRun::DiskFull),
            _ => None,
        }
    }
}

/// The per-file stdout line(s) for an action that succeeded (or would).
fn action_line(action: &Action, mode: Mode, outcome: &Outcome) -> Option<String> {
    let line = match (action, outcome) {
        (
            Action::Transfer {
                src, dst, sidecars, ..
            },
            Outcome::Transferred { .. },
        ) => {
            let mut s = format!("{} {} -> {}", mode.verb(), src.display(), dst.display());
            for sc in sidecars {
                s.push_str(&format!(
                    "\n  + {} -> {}",
                    sc.src.display(),
                    sc.dst.display()
                ));
            }
            s
        }
        (
            Action::Duplicate {
                src,
                of,
                sidecars_left,
                ..
            },
            Outcome::Duplicate { deleted },
        ) => {
            let verb = if *deleted { "dup-deleted" } else { "dup" };
            let mut s = format!("{verb} {} == {}", src.display(), of.display());
            for sc in sidecars_left {
                s.push_str(&format!("\n  sidecar left in place {}", sc.display()));
            }
            s
        }
        (Action::AlreadyInPlace { src }, _) => format!("in-place {}", src.display()),
        (Action::Skip { src, reason }, _) => format!("skip {} ({})", src.display(), reason.label()),
        _ => return None,
    };
    Some(line)
}

fn emit(line: &str) {
    log::info!(target: ACTION, "{line}");
}

/// Dry-run: the outcomes a real run would have if nothing failed, with the
/// same per-file lines prefixed `[dry-run] would `.
pub fn simulate(plan: &Plan, mode: Mode) -> Vec<Outcome> {
    plan.actions
        .iter()
        .map(|a| {
            let outcome = predicted(a);
            if let Outcome::Failed(msg) = &outcome {
                log::error!("{}: {msg}", a.src().display());
            } else if let Some(line) = action_line(a, mode, &outcome) {
                let prefixed: Vec<String> = line
                    .lines()
                    .map(|l| format!("[dry-run] would {l}"))
                    .collect();
                emit(&prefixed.join("\n"));
            }
            outcome
        })
        .collect()
}

fn predicted(a: &Action) -> Outcome {
    match a {
        Action::Transfer { sidecars, .. } => Outcome::Transferred {
            sidecars: sidecars.len(),
            sidecar_errors: Vec::new(),
        },
        Action::Duplicate { delete, .. } => Outcome::Duplicate { deleted: *delete },
        Action::AlreadyInPlace { .. } => Outcome::AlreadyInPlace,
        Action::Skip { reason, .. } => Outcome::Skipped(*reason),
        Action::Fail { error, .. } => Outcome::Failed(error.clone()),
    }
}

/// Shared state for one execution.
struct Exec<'a> {
    mode: Mode,
    stop: &'a Stop,
    bar: ProgressBar,
    bytes: AtomicU64,
    started: Instant,
    /// One warning per run for the exFAT check-then-rename fallback.
    warned_unchecked: AtomicBool,
}

/// Executes the plan for real. Returns one outcome per action, in order.
pub fn execute(
    plan: &Plan,
    mode: Mode,
    io_pool: &rayon::ThreadPool,
    stop: &Stop,
    progress: &Progress,
) -> Vec<Outcome> {
    let n_transfers = plan
        .actions
        .iter()
        .filter(|a| matches!(a, Action::Transfer { .. }))
        .count();
    let ex = Exec {
        mode,
        stop,
        bar: progress.bar(n_transfers as u64, "transferring"),
        bytes: AtomicU64::new(0),
        started: Instant::now(),
        warned_unchecked: AtomicBool::new(false),
    };
    let mut outcomes: Vec<Option<Outcome>> = vec![None; plan.actions.len()];

    // Skips, in-place files and planning failures need no I/O.
    for (i, a) in plan.actions.iter().enumerate() {
        let o = match a {
            Action::Skip { reason, .. } => Outcome::Skipped(*reason),
            Action::AlreadyInPlace { .. } => Outcome::AlreadyInPlace,
            Action::Fail { src, error } => {
                log::error!("{}: {error}", src.display());
                Outcome::Failed(error.clone())
            }
            _ => continue,
        };
        if let Some(line) = action_line(a, mode, &o) {
            emit(&line);
        }
        outcomes[i] = Some(o);
    }

    let dir_errors = create_dirs(&plan::target_dirs(plan));

    // Wave 1: transfers.
    let wave1: Vec<(usize, Outcome)> = io_pool.install(|| {
        plan.actions
            .par_iter()
            .enumerate()
            .filter(|(_, a)| matches!(a, Action::Transfer { .. }))
            .map(|(i, a)| (i, ex.transfer(a, &dir_errors)))
            .collect()
    });
    for (i, o) in wave1 {
        outcomes[i] = Some(o);
    }
    ex.bar.finish_and_clear();

    // Wave 2: duplicates (their dependency's outcome is final now).
    let done = |id: usize| matches!(outcomes[id], Some(Outcome::Transferred { .. }));
    let dep_ok: Vec<bool> = plan
        .actions
        .iter()
        .map(|a| match a {
            Action::Duplicate {
                depends_on: Some(id),
                ..
            } => done(*id),
            _ => true,
        })
        .collect();
    let wave2: Vec<(usize, Outcome)> = io_pool.install(|| {
        plan.actions
            .par_iter()
            .enumerate()
            .filter(|(_, a)| matches!(a, Action::Duplicate { .. }))
            .map(|(i, a)| (i, ex.duplicate(a, dep_ok[i])))
            .collect()
    });
    for (i, o) in wave2 {
        outcomes[i] = Some(o);
    }

    outcomes
        .into_iter()
        .map(|o| o.expect("every action has an outcome"))
        .collect()
}

/// Creates every target folder; returns the error for each one that failed.
fn create_dirs(dirs: &[PathBuf]) -> HashMap<PathBuf, String> {
    let mut errors = HashMap::new();
    for d in dirs {
        if let Err(e) = fs::create_dir_all(d) {
            log::error!("{}: create folder: {e}", d.display());
            errors.insert(d.clone(), format!("create folder {}: {e}", d.display()));
        }
    }
    errors
}

impl Exec<'_> {
    fn transfer(&self, a: &Action, dir_errors: &HashMap<PathBuf, String>) -> Outcome {
        let Action::Transfer {
            src,
            dst,
            size,
            sidecars,
            ..
        } = a
        else {
            unreachable!("only transfers reach wave 1")
        };
        if let Some(why) = self.stop.reason() {
            return Outcome::NotRun(why);
        }
        if let Some(err) = dst.parent().and_then(|p| dir_errors.get(p)) {
            return Outcome::Failed(err.clone());
        }
        let outcome = match self.one(src, dst) {
            Err(e) => {
                log::error!("{}: {}: {e}", src.display(), self.mode.verb());
                Outcome::Failed(format!("{}: {e}", self.mode.verb()))
            }
            Ok(()) => {
                let mut sidecar_errors = Vec::new();
                let mut done = 0;
                for sc in sidecars {
                    match self.one(&sc.src, &sc.dst) {
                        Ok(()) => done += 1,
                        Err(e) => {
                            log::error!("{}: sidecar {}: {e}", sc.src.display(), self.mode.verb());
                            sidecar_errors.push(format!(
                                "{}: sidecar {}: {e}",
                                sc.src.display(),
                                self.mode.verb()
                            ));
                        }
                    }
                }
                Outcome::Transferred {
                    sidecars: done,
                    sidecar_errors,
                }
            }
        };
        if let Some(line) = action_line(a, self.mode, &outcome) {
            emit(&line);
        }
        self.tick(*size);
        outcome
    }

    /// One file operation in the run's mode. A full disk stops the run.
    fn one(&self, src: &Path, dst: &Path) -> Result<(), FsError> {
        let r = match self.mode {
            Mode::Copy => fsops::copy_file(src, dst).map(drop),
            Mode::Move => fsops::move_file(src, dst).map(|how| {
                if how == MoveHow::RenamedUnchecked
                    && !self.warned_unchecked.swap(true, Ordering::SeqCst)
                {
                    log::warn!(
                        "destination volume lacks atomic no-clobber rename; using check-then-rename"
                    );
                }
            }),
            Mode::HardLink => fsops::hard_link(src, dst),
            Mode::Symlink => fsops::symlink(src, dst),
        };
        if let Err(e) = &r {
            if e.is_disk_full() {
                self.stop.request(NotRun::DiskFull);
            }
        }
        r
    }

    fn tick(&self, size: u64) {
        let total = self.bytes.fetch_add(size, Ordering::Relaxed) + size;
        let secs = self.started.elapsed().as_secs_f64().max(1e-3);
        self.bar
            .set_message(format!("{:.1} MiB/s", total as f64 / secs / 1_048_576.0));
        self.bar.inc(1);
    }

    fn duplicate(&self, a: &Action, dependency_ok: bool) -> Outcome {
        let Action::Duplicate {
            src,
            of,
            delete,
            size,
            dev_ino,
            ..
        } = a
        else {
            unreachable!("only duplicates reach wave 2")
        };
        if !dependency_ok {
            log::error!(
                "{}: not processed: the transfer to {} failed",
                src.display(),
                of.display()
            );
            return Outcome::NotRun(NotRun::DependencyFailed);
        }
        let outcome = if *delete {
            if let Some(why) = self.stop.reason() {
                return Outcome::NotRun(why);
            }
            match delete_verified(src, of, *size, *dev_ino) {
                Ok(()) => Outcome::Duplicate { deleted: true },
                Err(msg) => {
                    log::error!("{}: {msg}", src.display());
                    return Outcome::Failed(msg);
                }
            }
        } else {
            Outcome::Duplicate { deleted: false }
        };
        if let Some(line) = action_line(a, self.mode, &outcome) {
            emit(&line);
        }
        outcome
    }
}

/// Deletes a duplicate source only after re-checking, right now, that the
/// copy it duplicates is a regular file (not a symlink that might point back
/// at the source) with the expected size and a different inode, and that
/// the source itself is unchanged.
fn delete_verified(src: &Path, of: &Path, size: u64, dev_ino: (u64, u64)) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let of_meta = fs::symlink_metadata(of)
        .map_err(|e| format!("source kept: {} is gone: {e}", of.display()))?;
    if !of_meta.is_file() || of_meta.len() != size || (of_meta.dev(), of_meta.ino()) == dev_ino {
        return Err(format!(
            "source kept: {} changed since planning",
            of.display()
        ));
    }
    let src_meta = fs::symlink_metadata(src).map_err(|e| format!("delete duplicate: {e}"))?;
    if !src_meta.is_file() || src_meta.len() != size || (src_meta.dev(), src_meta.ino()) != dev_ino
    {
        return Err("source kept: it changed since planning".to_owned());
    }
    fs::remove_file(src).map_err(|e| format!("delete duplicate: {e}"))
}
