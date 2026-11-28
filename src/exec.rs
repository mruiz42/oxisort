//! Phase 5: carry out a [`Plan`] (or, for dry-run, describe it).
//!
//! Order of work:
//! 1. create every target folder (serial, sorted);
//! 2. wave 1: transfers (media + sidecars) in parallel on the I/O pool;
//! 3. wave 2: duplicates (filed into the duplicates folder, deleted or
//!    reported), each after the wave-1 transfer it depends on.
//!
//! Workers never choose names (the plan fixed them), so the only contention
//! left is with *other* processes, which the no-clobber primitives catch.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

use indicatif::ProgressBar;
use rayon::prelude::*;

use crate::fsops::{self, CopyHow, FsError, WriteOpts};
use crate::logging::{ACTION, Progress};
use crate::model::{
    Action, DeleteGuard, DupHandling, DupHow, Mode, NotRun, Outcome, PlannedSidecar, SidecarError,
};
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
            Outcome::Transferred { sidecar_errors, .. },
        ) => {
            let mut s = format!("{} {} -> {}", mode.verb(), src.display(), dst.display());
            push_sidecar_lines(&mut s, sidecars, sidecar_errors);
            s
        }
        (
            Action::Duplicate {
                src,
                of,
                handling,
                sidecars_left,
                ..
            },
            Outcome::Duplicate {
                how,
                sidecar_errors,
            },
        ) => {
            let (src, of) = (src.display(), of.display());
            let mut s = match (how, handling) {
                (DupHow::Filed, DupHandling::File { dst, sidecars }) => {
                    let mut s = format!(
                        "dup-{} {src} -> {} (duplicate of {of})",
                        mode.verb(),
                        dst.display()
                    );
                    push_sidecar_lines(&mut s, sidecars, sidecar_errors);
                    s
                }
                (DupHow::AlreadyFiled, DupHandling::AlreadyFiled { at }) => {
                    format!("dup {src} == {of} (already in {})", at.display())
                }
                (DupHow::Deleted, _) => format!("dup-deleted {src} == {of}"),
                (_, DupHandling::KeepSameFile) => format!("dup {src} == {of} (same file, kept)"),
                _ => format!("dup {src} == {of}"),
            };
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

/// One indented line per planned sidecar: `+` for those written, `!` for
/// those in `errors` (their reason is in the error log), so the per-file log
/// never claims a sidecar that isn't there.
fn push_sidecar_lines(s: &mut String, sidecars: &[PlannedSidecar], errors: &[SidecarError]) {
    for sc in sidecars {
        let failed = errors.iter().any(|e| e.src == sc.src);
        let (mark, note) = if failed {
            ('!', " (failed)")
        } else {
            ('+', "")
        };
        s.push_str(&format!(
            "\n  {mark} {} -> {}{note}",
            sc.src.display(),
            sc.dst.display()
        ));
    }
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
        Action::Duplicate { handling, .. } => Outcome::Duplicate {
            how: predicted_dup(handling),
            sidecar_errors: Vec::new(),
        },
        Action::AlreadyInPlace { .. } => Outcome::AlreadyInPlace,
        Action::Skip { reason, .. } => Outcome::Skipped(*reason),
        Action::Fail { error, .. } => Outcome::Failed(error.clone()),
    }
}

fn predicted_dup(handling: &DupHandling) -> DupHow {
    match handling {
        DupHandling::File { .. } => DupHow::Filed,
        DupHandling::AlreadyFiled { .. } => DupHow::AlreadyFiled,
        DupHandling::Delete(_) => DupHow::Deleted,
        DupHandling::Report | DupHandling::KeepSameFile => DupHow::Reported,
    }
}

/// Outcomes when the run stops before execution starts: no I/O happens;
/// transfers and duplicates are reported as not run.
pub fn not_run(plan: &Plan, why: NotRun) -> Vec<Outcome> {
    plan.actions
        .iter()
        .map(|a| match a {
            Action::Transfer { .. } | Action::Duplicate { .. } => Outcome::NotRun(why),
            Action::Fail { src, error } => {
                log::error!("{}: {error}", src.display());
                Outcome::Failed(error.clone())
            }
            other => predicted(other),
        })
        .collect()
}

/// State of the transfer a duplicate depends on, once wave 1 is over.
#[derive(Debug, Clone, Copy)]
enum Dependency {
    /// It made the copy (or the duplicate has no planned dependency).
    Done,
    /// The run stopped before it started.
    Stopped(NotRun),
    /// It ran and failed.
    Failed,
}

/// The words used for a not-run reason in log lines (as in the summary).
fn stop_label(why: NotRun) -> &'static str {
    match why {
        NotRun::Interrupted => "interrupted",
        NotRun::DiskFull => "destination full",
        NotRun::DependencyFailed => "a file they depend on failed",
    }
}

/// Shared state for one execution.
struct Exec<'a> {
    mode: Mode,
    opts: WriteOpts,
    stop: &'a Stop,
    bar: ProgressBar,
    bytes: AtomicU64,
    started: Instant,
    /// Files whose contents were re-read and blake3-checked before their
    /// source was deleted (cross-device moves, deleted duplicates).
    verified: AtomicUsize,
    /// One warning per run for the exFAT check-then-rename fallback
    /// (moves, and copies placed from their temp file).
    warned_unchecked: AtomicBool,
}

/// What [`execute`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Executed {
    /// One outcome per action, in plan order.
    pub outcomes: Vec<Outcome>,
    /// Files blake3-verified before a source was deleted.
    pub verified: usize,
}

/// Executes the plan for real.
pub fn execute(
    plan: &Plan,
    mode: Mode,
    opts: WriteOpts,
    io_pool: &rayon::ThreadPool,
    stop: &Stop,
    progress: &Progress,
) -> Executed {
    // Filed duplicates do real I/O too, so they count on the bar.
    let n_transfers = plan
        .actions
        .iter()
        .filter(|a| {
            matches!(
                a,
                Action::Transfer { .. }
                    | Action::Duplicate {
                        handling: DupHandling::File { .. },
                        ..
                    }
            )
        })
        .count();
    let ex = Exec {
        mode,
        opts,
        stop,
        bar: progress.bar(n_transfers as u64, "transferring"),
        bytes: AtomicU64::new(0),
        started: Instant::now(),
        verified: AtomicUsize::new(0),
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

    // Wave 2: duplicates (their dependency's outcome is final now).
    let dependency = |id: usize| match &outcomes[id] {
        Some(Outcome::Transferred { .. }) => Dependency::Done,
        // A transfer skipped by a stop never failed; the duplicate inherits
        // the stop's reason so it is reported (and counted) the same way.
        Some(Outcome::NotRun(why @ (NotRun::Interrupted | NotRun::DiskFull))) => {
            Dependency::Stopped(*why)
        }
        _ => Dependency::Failed,
    };
    let deps: Vec<Dependency> = plan
        .actions
        .iter()
        .map(|a| match a {
            Action::Duplicate {
                depends_on: Some(id),
                ..
            } => dependency(*id),
            _ => Dependency::Done,
        })
        .collect();
    let wave2: Vec<(usize, Outcome)> = io_pool.install(|| {
        plan.actions
            .par_iter()
            .enumerate()
            .filter(|(_, a)| matches!(a, Action::Duplicate { .. }))
            .map(|(i, a)| (i, ex.duplicate(a, deps[i], &dir_errors)))
            .collect()
    });
    for (i, o) in wave2 {
        outcomes[i] = Some(o);
    }
    ex.bar.finish_and_clear();

    Executed {
        outcomes: outcomes
            .into_iter()
            .map(|o| o.expect("every action has an outcome"))
            .collect(),
        verified: ex.verified.into_inner(),
    }
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
                            let message =
                                format!("{}: sidecar {}: {e}", sc.src.display(), self.mode.verb());
                            log::error!("{message}");
                            sidecar_errors.push(SidecarError {
                                src: sc.src.clone(),
                                message,
                            });
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
        let opts = &self.opts;
        let r = match self.mode {
            Mode::Copy => fsops::copy_file(src, dst, opts)
                .map(|how| self.warn_if_unchecked(how == CopyHow::CopiedUnchecked)),
            Mode::Move => fsops::move_file(src, dst, opts).map(|(how, hash)| {
                self.warn_if_unchecked(how.is_unchecked());
                if hash.is_some() {
                    self.verified.fetch_add(1, Ordering::Relaxed);
                }
            }),
            Mode::HardLink => fsops::hard_link(src, dst, opts),
            Mode::Symlink => fsops::symlink(src, dst, opts),
        };
        if let Err(e) = &r {
            if e.is_disk_full() {
                self.stop.request(NotRun::DiskFull);
            }
        }
        r
    }

    /// Warns once per run that a final name was claimed with
    /// check-then-rename, which isn't atomic against other processes.
    fn warn_if_unchecked(&self, unchecked: bool) {
        if unchecked && !self.warned_unchecked.swap(true, Ordering::SeqCst) {
            log::warn!(
                "destination volume lacks atomic no-clobber rename; using check-then-rename"
            );
        }
    }

    fn tick(&self, size: u64) {
        let total = self.bytes.fetch_add(size, Ordering::Relaxed) + size;
        let secs = self.started.elapsed().as_secs_f64().max(1e-3);
        self.bar
            .set_message(format!("{:.1} MiB/s", total as f64 / secs / 1_048_576.0));
        self.bar.inc(1);
    }

    fn duplicate(
        &self,
        a: &Action,
        dependency: Dependency,
        dir_errors: &HashMap<PathBuf, String>,
    ) -> Outcome {
        let Action::Duplicate {
            src,
            of,
            handling,
            size,
            dev_ino,
            depends_on,
            ..
        } = a
        else {
            unreachable!("only duplicates reach wave 2")
        };
        match dependency {
            Dependency::Done => {}
            Dependency::Stopped(why) => {
                log::warn!(
                    "{}: not run ({}): the transfer to {} did not run",
                    src.display(),
                    stop_label(why),
                    of.display()
                );
                return Outcome::NotRun(why);
            }
            Dependency::Failed => {
                log::error!(
                    "{}: not processed: the transfer to {} failed",
                    src.display(),
                    of.display()
                );
                return Outcome::NotRun(NotRun::DependencyFailed);
            }
        }
        let mut sidecar_errors = Vec::new();
        let how = match handling {
            DupHandling::File { dst, sidecars } => {
                if let Some(why) = self.stop.reason() {
                    return Outcome::NotRun(why);
                }
                if let Some(err) = dst.parent().and_then(|p| dir_errors.get(p)) {
                    return Outcome::Failed(err.clone());
                }
                let verb = self.mode.verb();
                if let Err(e) = self.one(src, dst) {
                    log::error!("{}: duplicate {verb}: {e}", src.display());
                    self.tick(*size);
                    return Outcome::Failed(format!("duplicate {verb}: {e}"));
                }
                for sc in sidecars {
                    if let Err(e) = self.one(&sc.src, &sc.dst) {
                        let message = format!("{}: sidecar {verb}: {e}", sc.src.display());
                        log::error!("{message}");
                        sidecar_errors.push(SidecarError {
                            src: sc.src.clone(),
                            message,
                        });
                    }
                }
                self.tick(*size);
                DupHow::Filed
            }
            DupHandling::Delete(guard) => {
                if let Some(why) = self.stop.reason() {
                    return Outcome::NotRun(why);
                }
                let planned = Planned {
                    size: *size,
                    dev_ino: *dev_ino,
                    guard: *guard,
                    of_written_by_run: depends_on.is_some(),
                    verify: self.opts.verify,
                };
                if let Err(msg) = delete_verified(src, of, &planned) {
                    log::error!("{}: {msg}", src.display());
                    return Outcome::Failed(msg);
                }
                if planned.verify {
                    self.verified.fetch_add(1, Ordering::Relaxed);
                }
                DupHow::Deleted
            }
            DupHandling::AlreadyFiled { .. } => DupHow::AlreadyFiled,
            DupHandling::Report | DupHandling::KeepSameFile => DupHow::Reported,
        };
        let outcome = Outcome::Duplicate {
            how,
            sidecar_errors,
        };
        if let Some(line) = action_line(a, self.mode, &outcome) {
            emit(&line);
        }
        outcome
    }
}

/// What planning recorded about a duplicate that is to be deleted.
struct Planned {
    /// Of both files (they have equal content).
    size: u64,
    /// The source's.
    dev_ino: (u64, u64),
    guard: DeleteGuard,
    /// This run created the kept copy (wave 1): it had no mtime at planning.
    of_written_by_run: bool,
    /// Re-read both files and compare them with the planned hash (off:
    /// `--no-verify`).
    verify: bool,
}

/// Deletes a duplicate source only after re-checking, right now, that the
/// copy it duplicates is a regular file (not a symlink that might point back
/// at the source) with the planned size, a different inode and, unless this
/// run just wrote it, the planned mtime; and that the source itself still
/// has its planned size, inode and mtime. Then, unless `--no-verify`, both
/// files are re-read and must still hash to the content planning found
/// (issue #2): that catches a same-size rewrite whose mtime was restored.
fn delete_verified(src: &Path, of: &Path, p: &Planned) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let of_meta = fs::symlink_metadata(of)
        .map_err(|e| format!("source kept: {} is gone: {e}", of.display()))?;
    let of_same_time = p.of_written_by_run || of_meta.modified().ok() == p.guard.of_mtime;
    if !of_meta.is_file()
        || of_meta.len() != p.size
        || (of_meta.dev(), of_meta.ino()) == p.dev_ino
        || !of_same_time
    {
        return Err(format!(
            "source kept: {} changed since planning",
            of.display()
        ));
    }
    let src_meta = fs::symlink_metadata(src).map_err(|e| format!("delete duplicate: {e}"))?;
    if !src_meta.is_file()
        || src_meta.len() != p.size
        || (src_meta.dev(), src_meta.ino()) != p.dev_ino
        || src_meta.modified().ok() != p.guard.src_mtime
    {
        return Err("source kept: it changed since planning".to_owned());
    }
    if p.verify {
        let Some(want) = p.guard.hash else {
            // Planning always hashes a deletable duplicate; never guess.
            return Err("source kept: no planned hash to verify against".to_owned());
        };
        for (what, path) in [(of.display().to_string(), of), ("source".to_owned(), src)] {
            match fsops::hash_with_buf(path) {
                Ok(h) if h == want => {}
                Ok(_) => {
                    return Err(format!(
                        "source kept: contents of {what} differ from planning (blake3 mismatch)"
                    ));
                }
                Err(e) => return Err(format!("source kept: cannot verify {what}: {e}")),
            }
        }
    }
    fs::remove_file(src).map_err(|e| format!("delete duplicate: {e}"))
}

/// What wave 3 (`--rmdirs`) did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RmdirTotals {
    pub dirs: usize,
    pub junk: usize,
    pub errors: Vec<String>,
}

/// Wave 3: removes source folders the plan expects to be empty, deepest
/// first. Each folder is listed again right before removal and kept unless
/// it holds only junk files (decision Q4), so a failed move upstream simply
/// leaves its folder in place. `remove_dir` itself never deletes contents.
pub fn remove_dirs(rmdirs: &[plan::RmDir], dry_run: bool, stop: &Stop) -> RmdirTotals {
    let mut t = RmdirTotals::default();
    for d in rmdirs {
        if dry_run {
            let mut lines = vec![format!("[dry-run] would rmdir {}", d.path.display())];
            lines.extend(
                d.junk
                    .iter()
                    .map(|j| format!("[dry-run] would   - {}", j.display())),
            );
            emit(&lines.join("\n"));
            t.dirs += 1;
            t.junk += d.junk.len();
            continue;
        }
        if stop.reason().is_some() {
            break;
        }
        match remove_if_junk_only(&d.path) {
            Ok(Some(junk)) => {
                let mut lines = vec![format!("rmdir {}", d.path.display())];
                lines.extend(junk.iter().map(|j| format!("  - {}", j.display())));
                emit(&lines.join("\n"));
                t.dirs += 1;
                t.junk += junk.len();
            }
            Ok(None) => log::debug!("kept {} (not empty)", d.path.display()),
            Err(e) => {
                log::error!("{}: remove folder: {e}", d.path.display());
                t.errors
                    .push(format!("{}: remove folder: {e}", d.path.display()));
            }
        }
    }
    t
}

/// Removes `dir` if it holds nothing but junk files; returns the junk
/// deleted, or `None` if the folder had other content.
fn remove_if_junk_only(dir: &Path) -> std::io::Result<Option<Vec<PathBuf>>> {
    let mut junk = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if !plan::is_removable_junk(&entry) {
            return Ok(None);
        }
        junk.push(entry.path());
    }
    junk.sort();
    for j in &junk {
        fs::remove_file(j)?;
    }
    match fs::remove_dir(dir) {
        Ok(()) => Ok(Some(junk)),
        // Something appeared meanwhile: keep the folder.
        Err(e) if e.kind() == std::io::ErrorKind::DirectoryNotEmpty => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Kind;

    fn sidecar(name: &str) -> PlannedSidecar {
        PlannedSidecar {
            src: PathBuf::from(format!("/in/{name}")),
            dst: PathBuf::from(format!("/out/{name}")),
        }
    }

    fn failed(name: &str) -> SidecarError {
        SidecarError {
            src: PathBuf::from(format!("/in/{name}")),
            message: format!("/in/{name}: sidecar copy: No space left on device"),
        }
    }

    /// F4: a sidecar that failed is shown as failed, never as `+`.
    #[test]
    fn failed_sidecars_are_not_listed_as_written() {
        let transfer = Action::Transfer {
            src: PathBuf::from("/in/a.arw"),
            dst: PathBuf::from("/out/a.arw"),
            kind: Kind::Image,
            dated: true,
            size: 1,
            sidecars: vec![sidecar("a.arw.xmp"), sidecar("a.xmp")],
        };
        let outcome = Outcome::Transferred {
            sidecars: 1,
            sidecar_errors: vec![failed("a.arw.xmp")],
        };
        assert_eq!(
            action_line(&transfer, Mode::Copy, &outcome).unwrap(),
            "copy /in/a.arw -> /out/a.arw\n  \
             ! /in/a.arw.xmp -> /out/a.arw.xmp (failed)\n  \
             + /in/a.xmp -> /out/a.xmp"
        );

        let dup = Action::Duplicate {
            src: PathBuf::from("/in/b.jpg"),
            of: PathBuf::from("/out/a.jpg"),
            handling: DupHandling::File {
                dst: PathBuf::from("/out/duplicates/b.jpg"),
                sidecars: vec![sidecar("b.xmp")],
            },
            size: 1,
            dev_ino: (0, 0),
            depends_on: None,
            sidecars_left: vec![],
        };
        let outcome = Outcome::Duplicate {
            how: DupHow::Filed,
            sidecar_errors: vec![failed("b.xmp")],
        };
        assert_eq!(
            action_line(&dup, Mode::Copy, &outcome).unwrap(),
            "dup-copy /in/b.jpg -> /out/duplicates/b.jpg (duplicate of /out/a.jpg)\n  \
             ! /in/b.xmp -> /out/b.xmp (failed)"
        );
    }

    /// Issue #2 / DoD 5: the kept copy is rewritten after planning with the
    /// same size and its mtime restored, so only the content differs. The
    /// blake3 re-check keeps the source and fails the action (exit 1);
    /// `--no-verify` falls back to the metadata checks and deletes.
    #[test]
    fn rewritten_kept_copy_with_restored_mtime_keeps_the_source() {
        use std::os::unix::fs::MetadataExt;
        for verify in [true, false] {
            let d = tempfile::tempdir().unwrap();
            let src = d.path().join("in.jpg");
            let of = d.path().join("kept.jpg");
            fs::write(&src, b"same bytes").unwrap();
            fs::write(&of, b"same bytes").unwrap();
            let mtime = |p: &Path| fs::metadata(p).unwrap().modified().unwrap();
            let (src_mtime, of_mtime) = (mtime(&src), mtime(&of));
            let m = fs::metadata(&src).unwrap();
            let guard = DeleteGuard {
                src_mtime: Some(src_mtime),
                of_mtime: Some(of_mtime),
                hash: Some(blake3::hash(b"same bytes")),
            };
            // After planning: same size, new bytes, old mtime.
            fs::write(&of, b"SAME BYTES").unwrap();
            fs::File::options()
                .write(true)
                .open(&of)
                .unwrap()
                .set_modified(of_mtime)
                .unwrap();
            let plan = Plan {
                actions: vec![Action::Duplicate {
                    src: src.clone(),
                    of: of.clone(),
                    handling: DupHandling::Delete(guard),
                    depends_on: None,
                    size: m.len(),
                    dev_ino: (m.dev(), m.ino()),
                    sidecars_left: vec![],
                }],
                ..Plan::default()
            };
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(1)
                .build()
                .unwrap();
            let opts = WriteOpts {
                durable: false,
                verify,
            };
            let done = execute(
                &plan,
                Mode::Move,
                opts,
                &pool,
                &Stop::new(),
                &Progress::disabled(),
            );
            let mut summary = crate::report::RunSummary::default();
            summary.add(&plan.actions[0], &done.outcomes[0]);
            if verify {
                match &done.outcomes[0] {
                    Outcome::Failed(msg) => assert!(msg.contains("blake3 mismatch"), "{msg}"),
                    other => panic!("expected a failure, got {other:?}"),
                }
                assert_eq!(fs::read(&src).unwrap(), b"same bytes", "source kept");
                assert_eq!(summary.exit_code(), 1);
                assert_eq!(done.verified, 0);
            } else {
                assert!(
                    matches!(
                        done.outcomes[0],
                        Outcome::Duplicate {
                            how: DupHow::Deleted,
                            ..
                        }
                    ),
                    "{:?}",
                    done.outcomes
                );
                assert!(!src.exists(), "old behaviour: deleted on metadata alone");
            }
        }
    }

    /// A clean duplicate is deleted after both files are re-hashed.
    #[test]
    fn verified_duplicate_is_deleted_and_counted() {
        use std::os::unix::fs::MetadataExt;
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("in.jpg");
        let of = d.path().join("kept.jpg");
        fs::write(&src, b"same bytes").unwrap();
        fs::write(&of, b"same bytes").unwrap();
        let mtime = |p: &Path| fs::metadata(p).unwrap().modified().ok();
        let m = fs::metadata(&src).unwrap();
        let plan = Plan {
            actions: vec![Action::Duplicate {
                src: src.clone(),
                of: of.clone(),
                handling: DupHandling::Delete(DeleteGuard {
                    src_mtime: mtime(&src),
                    of_mtime: mtime(&of),
                    hash: Some(blake3::hash(b"same bytes")),
                }),
                depends_on: None,
                size: m.len(),
                dev_ino: (m.dev(), m.ino()),
                sidecars_left: vec![],
            }],
            ..Plan::default()
        };
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .unwrap();
        let done = execute(
            &plan,
            Mode::Move,
            WriteOpts::default(),
            &pool,
            &Stop::new(),
            &Progress::disabled(),
        );
        assert!(
            matches!(
                done.outcomes[0],
                Outcome::Duplicate {
                    how: DupHow::Deleted,
                    ..
                }
            ),
            "{:?}",
            done.outcomes
        );
        assert_eq!(done.verified, 1);
        assert!(!src.exists() && of.exists());
    }
}
