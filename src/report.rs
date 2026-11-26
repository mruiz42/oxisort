//! Phase 6: count outcomes, render the summary, map to an exit code.

use std::fmt::Write as _;
use std::time::Duration;

use crate::exec::Stop;
use crate::model::{Action, DupHow, DupPolicy, Kind, Mode, NotRun, Outcome, SkipReason};
use crate::scan::ScanCounts;

/// How many error messages the summary lists before "and N more".
const MAX_LISTED_ERRORS: usize = 20;

/// Totals for one run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunSummary {
    pub dry_run: bool,
    pub images: usize,
    pub videos: usize,
    pub others: usize,
    /// Transfers into the no-date folder (included in the three above).
    pub no_date: usize,
    pub sidecars: usize,
    pub duplicates: usize,
    pub duplicates_deleted: usize,
    pub duplicates_filed: usize,
    pub duplicates_already_filed: usize,
    /// How duplicates were handled, for the summary wording.
    pub dup_policy: DupPolicy,
    /// The duplicates folder's name (as given to `--duplicates-dir`).
    pub dup_dir: String,
    pub in_place: usize,
    pub skipped_type: usize,
    pub skipped_range: usize,
    pub skipped_unknown: usize,
    pub scan: ScanCounts,
    pub shared_sidecars: usize,
    pub dirs_removed: usize,
    pub junk_removed: usize,
    /// `path: message` for every failure (files, sidecars, scan errors).
    pub errors: Vec<String>,
    pub not_run: usize,
    pub interrupted: bool,
    pub disk_full: bool,
}

impl RunSummary {
    /// Folds one action and its outcome into the totals.
    pub fn add(&mut self, action: &Action, outcome: &Outcome) {
        match outcome {
            Outcome::Transferred {
                sidecars,
                sidecar_errors,
            } => {
                if let Action::Transfer { kind, dated, .. } = action {
                    match kind {
                        Kind::Image => self.images += 1,
                        Kind::Video => self.videos += 1,
                        Kind::Other => self.others += 1,
                    }
                    if !dated {
                        self.no_date += 1;
                    }
                }
                self.sidecars += sidecars;
                self.errors
                    .extend(sidecar_errors.iter().map(|e| e.message.clone()));
            }
            Outcome::Duplicate {
                how,
                sidecar_errors,
            } => {
                self.duplicates += 1;
                match how {
                    DupHow::Deleted => self.duplicates_deleted += 1,
                    DupHow::Filed => self.duplicates_filed += 1,
                    DupHow::AlreadyFiled => self.duplicates_already_filed += 1,
                    DupHow::Reported => {}
                }
                self.errors
                    .extend(sidecar_errors.iter().map(|e| e.message.clone()));
            }
            Outcome::AlreadyInPlace => self.in_place += 1,
            Outcome::Skipped(r) => match r {
                SkipReason::FileType => self.skipped_type += 1,
                SkipReason::DateRange => self.skipped_range += 1,
                SkipReason::NoDate => self.skipped_unknown += 1,
            },
            Outcome::Failed(msg) => self
                .errors
                .push(format!("{}: {msg}", action.src().display())),
            Outcome::NotRun(reason) => {
                self.not_run += 1;
                match reason {
                    NotRun::Interrupted => self.interrupted = true,
                    NotRun::DiskFull => self.disk_full = true,
                    NotRun::DependencyFailed => {}
                }
            }
        }
    }

    pub fn transferred(&self) -> usize {
        self.images + self.videos + self.others
    }

    /// 130 after SIGINT, 1 if anything failed or didn't run, else 0.
    pub fn exit_code(&self) -> u8 {
        if self.interrupted {
            130
        } else if !self.errors.is_empty() || self.not_run > 0 || self.disk_full {
            1
        } else {
            0
        }
    }

    /// `N (…)` for the duplicates line, worded by policy and mode.
    fn duplicates_detail(&self, mode: Mode) -> String {
        let n = self.duplicates;
        match self.dup_policy {
            DupPolicy::Skip => format!("{n} (left in place)"),
            DupPolicy::Delete => format!("{n} (deleted {})", self.duplicates_deleted),
            DupPolicy::File => {
                let verb = match mode {
                    Mode::Copy => "copied to",
                    Mode::Move => "moved to",
                    Mode::HardLink => "linked into",
                    Mode::Symlink => "symlinked into",
                };
                let mut s = format!("{n} ({verb} {}/: {}", self.dup_dir, self.duplicates_filed);
                if self.duplicates_already_filed > 0 {
                    s.push_str(&format!(
                        ", already there: {}",
                        self.duplicates_already_filed
                    ));
                }
                s.push(')');
                s
            }
        }
    }

    /// The human-readable summary (several lines).
    pub fn render(&self, mode: Mode, elapsed: Duration, files_seen: usize) -> String {
        let secs = elapsed.as_secs_f64();
        let rate = if secs > 0.0 {
            files_seen as f64 / secs
        } else {
            0.0
        };
        let verb = if self.dry_run { "dry run" } else { mode.verb() };
        // A stopped run did not finish, so the header must not say it did.
        // A full destination wins over Ctrl-C: it is the reason the user
        // has to act on (free space), and it may have triggered the stop.
        let ended = if self.disk_full {
            "stopped: destination full after"
        } else if self.interrupted {
            "interrupted after"
        } else {
            "finished in"
        };
        let mut s = String::new();
        let _ = writeln!(
            s,
            "oxisort: {verb} {ended} {secs:.2} s ({files_seen} files, {rate:.0} files/s)"
        );
        let _ = writeln!(
            s,
            "  transferred:      {} (images {}, videos {}, other {}), sidecars {}",
            self.transferred(),
            self.images,
            self.videos,
            self.others,
            self.sidecars
        );
        let _ = writeln!(s, "  no date:          {}", self.no_date);
        let _ = writeln!(s, "  duplicates:       {}", self.duplicates_detail(mode));
        let _ = writeln!(s, "  already in place: {}", self.in_place);
        let c = &self.scan;
        let _ = writeln!(
            s,
            "  skipped:          type {}, date range {}, unknown {}, ignored {}, symlinks {}, \
             system folders {}, orphan sidecars {}, shared sidecars {}",
            self.skipped_type,
            self.skipped_range,
            self.skipped_unknown,
            c.ignored_junk + c.special_files,
            c.symlinks,
            c.skipped_dirs,
            c.orphan_sidecars,
            self.shared_sidecars
        );
        if self.dirs_removed > 0 || self.junk_removed > 0 {
            let _ = writeln!(
                s,
                "  folders removed:  {} (junk files removed {})",
                self.dirs_removed, self.junk_removed
            );
        }
        let _ = writeln!(s, "  errors:           {}", self.errors.len());
        for e in self.errors.iter().take(MAX_LISTED_ERRORS) {
            let _ = writeln!(s, "    {e}");
        }
        if self.errors.len() > MAX_LISTED_ERRORS {
            let _ = writeln!(
                s,
                "    … and {} more",
                self.errors.len() - MAX_LISTED_ERRORS
            );
        }
        if self.not_run > 0 {
            let why = if self.interrupted {
                "interrupted"
            } else if self.disk_full {
                "destination full"
            } else {
                "a file they depend on failed"
            };
            let _ = writeln!(s, "  not run:          {} ({why})", self.not_run);
        }
        if self.dry_run {
            let _ = writeln!(s, "  (dry run, nothing changed)");
        }
        s.trim_end().to_owned()
    }
}

/// Folds the run's final stop state into the summary.
///
/// A stop that hit after the last task started (an `ENOSPC` on the final
/// transfer, a Ctrl-C during `--rmdirs`) leaves no `NotRun` outcome behind,
/// but the run was still cut short, so the header and exit code must say
/// so. `Stop::reason` also picks up SIGINT.
pub fn apply_stop(summary: &mut RunSummary, stop: &Stop) {
    match stop.reason() {
        Some(NotRun::Interrupted) => summary.interrupted = true,
        Some(NotRun::DiskFull) => summary.disk_full = true,
        Some(NotRun::DependencyFailed) | None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SidecarError;
    use std::path::PathBuf;

    fn transfer(kind: Kind, dated: bool) -> Action {
        Action::Transfer {
            src: PathBuf::from("/in/a"),
            dst: PathBuf::from("/out/a"),
            kind,
            dated,
            size: 1,
            sidecars: vec![],
        }
    }

    #[test]
    fn exit_codes() {
        let mut s = RunSummary::default();
        let ok = Outcome::Transferred {
            sidecars: 1,
            sidecar_errors: vec![],
        };
        s.add(&transfer(Kind::Image, true), &ok);
        s.add(&transfer(Kind::Other, false), &ok);
        assert_eq!((s.images, s.others, s.no_date, s.sidecars), (1, 1, 1, 2));
        assert_eq!(s.exit_code(), 0);

        let mut failed = s.clone();
        failed.add(
            &transfer(Kind::Image, true),
            &Outcome::Failed("boom".into()),
        );
        assert_eq!(failed.exit_code(), 1);
        assert_eq!(failed.errors, ["/in/a: boom"]);

        let mut dep = s.clone();
        dep.add(
            &transfer(Kind::Image, true),
            &Outcome::NotRun(NotRun::DependencyFailed),
        );
        assert_eq!(dep.exit_code(), 1);

        let mut int = s.clone();
        int.add(
            &transfer(Kind::Image, true),
            &Outcome::NotRun(NotRun::Interrupted),
        );
        assert_eq!(int.exit_code(), 130);

        let mut side = s;
        side.add(
            &transfer(Kind::Image, true),
            &Outcome::Transferred {
                sidecars: 0,
                sidecar_errors: vec![SidecarError {
                    src: PathBuf::from("x"),
                    message: "x: y".into(),
                }],
            },
        );
        assert_eq!(side.exit_code(), 1);
    }

    #[test]
    fn render_mentions_dry_run_and_errors() {
        let mut s = RunSummary {
            dry_run: true,
            ..RunSummary::default()
        };
        for i in 0..25 {
            s.errors.push(format!("e{i}"));
        }
        let text = s.render(Mode::Copy, Duration::from_millis(500), 10);
        assert!(text.contains("dry run finished"));
        assert!(text.contains("(dry run, nothing changed)"));
        assert!(text.contains("… and 5 more"));
    }

    fn dup_line(s: &RunSummary, mode: Mode) -> String {
        let text = s.render(mode, Duration::from_millis(1), 1);
        text.lines()
            .find(|l| l.starts_with("  duplicates:"))
            .unwrap()
            .to_owned()
    }

    /// U23.
    #[test]
    fn duplicate_summary_wording() {
        let mut s = RunSummary {
            dup_dir: "duplicates".into(),
            ..RunSummary::default()
        };
        let dup = |how| Outcome::Duplicate {
            how,
            sidecar_errors: vec![],
        };
        let a = transfer(Kind::Image, true);
        s.add(&a, &dup(DupHow::Filed));
        s.add(&a, &dup(DupHow::Filed));
        for (mode, verb) in [
            (Mode::Copy, "copied to"),
            (Mode::Move, "moved to"),
            (Mode::HardLink, "linked into"),
            (Mode::Symlink, "symlinked into"),
        ] {
            assert_eq!(
                dup_line(&s, mode),
                format!("  duplicates:       2 ({verb} duplicates/: 2)")
            );
        }
        s.add(&a, &dup(DupHow::AlreadyFiled));
        assert_eq!(
            dup_line(&s, Mode::Copy),
            "  duplicates:       3 (copied to duplicates/: 2, already there: 1)"
        );

        let mut skip = RunSummary {
            dup_policy: DupPolicy::Skip,
            ..RunSummary::default()
        };
        skip.add(&a, &dup(DupHow::Reported));
        assert_eq!(
            dup_line(&skip, Mode::Copy),
            "  duplicates:       1 (left in place)"
        );

        let mut del = RunSummary {
            dup_policy: DupPolicy::Delete,
            ..RunSummary::default()
        };
        del.add(&a, &dup(DupHow::Deleted));
        del.add(&a, &dup(DupHow::Reported));
        assert_eq!(
            dup_line(&del, Mode::Move),
            "  duplicates:       2 (deleted 1)"
        );

        // A sidecar that failed to follow its filed duplicate is an error.
        let mut e = RunSummary::default();
        e.add(
            &a,
            &Outcome::Duplicate {
                how: DupHow::Filed,
                sidecar_errors: vec![SidecarError {
                    src: PathBuf::from("x.xmp"),
                    message: "x.xmp: sidecar copy: boom".into(),
                }],
            },
        );
        assert_eq!(e.exit_code(), 1);
    }

    fn header(s: &RunSummary, mode: Mode) -> String {
        let text = s.render(mode, Duration::from_millis(2020), 4);
        text.lines().next().unwrap().to_owned()
    }

    #[test]
    fn disk_full_header() {
        let mut s = RunSummary::default();
        assert_eq!(
            header(&s, Mode::Copy),
            "oxisort: copy finished in 2.02 s (4 files, 2 files/s)"
        );
        s.interrupted = true;
        assert_eq!(
            header(&s, Mode::Move),
            "oxisort: move interrupted after 2.02 s (4 files, 2 files/s)"
        );
        s.disk_full = true;
        assert_eq!(
            header(&s, Mode::Copy),
            "oxisort: copy stopped: destination full after 2.02 s (4 files, 2 files/s)",
            "disk full takes precedence over interrupted"
        );
        let dry = RunSummary {
            dry_run: true,
            ..RunSummary::default()
        };
        assert!(header(&dry, Mode::Copy).starts_with("oxisort: dry run finished in "));
    }

    #[test]
    fn apply_stop_sets_disk_full_without_not_run() {
        let mut s = RunSummary::default();
        apply_stop(&mut s, &Stop::new());
        assert!(!s.disk_full && !s.interrupted);

        let stop = Stop::new();
        stop.request(NotRun::DiskFull);
        apply_stop(&mut s, &stop);
        assert!(s.disk_full && !s.interrupted);
        assert_eq!(s.not_run, 0);
        assert_eq!(s.exit_code(), 1);
        assert!(
            header(&s, Mode::Copy).starts_with("oxisort: copy stopped: destination full after")
        );

        let mut i = RunSummary::default();
        let stop = Stop::new();
        stop.request(NotRun::Interrupted);
        apply_stop(&mut i, &stop);
        assert!(i.interrupted && !i.disk_full);
        assert_eq!(i.exit_code(), 130);
    }
}
