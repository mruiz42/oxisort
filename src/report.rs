//! Phase 6: count outcomes, render the summary, map to an exit code.

use std::fmt::Write as _;
use std::time::Duration;

use crate::model::{Action, Kind, Mode, NotRun, Outcome, SkipReason};
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
                self.errors.extend(sidecar_errors.iter().cloned());
            }
            Outcome::Duplicate { deleted } => {
                self.duplicates += 1;
                if *deleted {
                    self.duplicates_deleted += 1;
                }
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

    /// The human-readable summary (several lines).
    pub fn render(&self, mode: Mode, elapsed: Duration, files_seen: usize) -> String {
        let secs = elapsed.as_secs_f64();
        let rate = if secs > 0.0 {
            files_seen as f64 / secs
        } else {
            0.0
        };
        let verb = if self.dry_run { "dry run" } else { mode.verb() };
        let mut s = String::new();
        let _ = writeln!(
            s,
            "oxisort: {verb} finished in {secs:.2} s ({files_seen} files, {rate:.0} files/s)"
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
        let _ = writeln!(
            s,
            "  duplicates:       {} (deleted {})",
            self.duplicates, self.duplicates_deleted
        );
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

#[cfg(test)]
mod tests {
    use super::*;
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
                sidecar_errors: vec!["x: y".into()],
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
}
