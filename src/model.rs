//! Shared data types passed between the pipeline phases
//! (cli → scan → extract → plan → exec → report).
//!
//! Keeping them in one module keeps the phases decoupled: each phase depends
//! on these plain types, not on another phase's internals.

use std::path::PathBuf;

use chrono::{FixedOffset, NaiveDate, NaiveDateTime, TimeZone, Utc};

use crate::date::FilenameRegex;
use crate::dateformat::DirFormat;

/// How a file reaches the output tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Copy,
    Move,
    HardLink,
    Symlink,
}

impl Mode {
    /// The verb used in per-file log lines (`copy a -> b`).
    pub fn verb(self) -> &'static str {
        match self {
            Mode::Copy => "copy",
            Mode::Move => "move",
            Mode::HardLink => "link",
            Mode::Symlink => "symlink",
        }
    }
}

/// Media classification, decided by content first and extension second.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Image,
    Video,
    Other,
}

/// The zone used to turn UTC-sourced instants (mvhd/MKV dates, mtime) into a
/// wall-clock time. Camera EXIF and Apple video keys already carry wall clock
/// and never go through this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zone {
    /// The zone of the machine running oxisort (chrono `Local`, honours `TZ`).
    System,
    /// A fixed offset from `--tz`.
    Fixed(FixedOffset),
}

impl Zone {
    /// Converts a UTC instant to wall-clock time in this zone.
    ///
    /// `System` is DST-correct for the given date, because chrono resolves
    /// the local offset at that instant, not at "now".
    pub fn wall_from_utc(self, utc: NaiveDateTime) -> NaiveDateTime {
        match self {
            Zone::System => chrono::Local.from_utc_datetime(&utc).naive_local(),
            Zone::Fixed(off) => off.from_utc_datetime(&utc).naive_local(),
        }
    }

    /// Convenience for `SystemTime`-like inputs already converted to UTC.
    pub fn wall_from_utc_dt(self, utc: chrono::DateTime<Utc>) -> NaiveDateTime {
        self.wall_from_utc(utc.naive_utc())
    }
}

/// Date sources selectable with `--date-field`, in user order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateField {
    DateTimeOriginal,
    CreateDate,
    ModifyDate,
    MediaCreateDate,
}

impl DateField {
    /// Every field, in the default priority order.
    pub const DEFAULT: [DateField; 4] = [
        DateField::DateTimeOriginal,
        DateField::CreateDate,
        DateField::ModifyDate,
        DateField::MediaCreateDate,
    ];

    /// Index into [`RawMeta::exif`]-style arrays for the EXIF fields.
    pub fn exif_index(self) -> Option<usize> {
        match self {
            DateField::DateTimeOriginal => Some(0),
            DateField::CreateDate => Some(1),
            DateField::ModifyDate => Some(2),
            DateField::MediaCreateDate => None,
        }
    }
}

/// Where a resolved capture date came from (debug output and tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateSource {
    ExifDateTimeOriginal,
    ExifCreateDate,
    ExifModifyDate,
    /// Apple `com.apple.quicktime.creationdate`: local time as stored.
    VideoLocal,
    /// `mvhd` / MKV date: UTC, converted with [`Zone`].
    VideoUtc,
    Filename,
    Mtime,
}

/// A resolved capture date: wall-clock time plus optional sub-second digits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Captured {
    pub wall: NaiveDateTime,
    /// Digits only, appended to the name verbatim (no padding).
    pub subsec: Option<String>,
    pub source: DateSource,
}

/// Console verbosity. `--log` has its own, fixed level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verbosity {
    Quiet,
    Normal,
    /// `-v` = 1 (debug), `-vv` = 2 (trace).
    Verbose(u8),
}

/// The fully validated run configuration. Built only by [`crate::cli`]; every
/// path in it is absolute.
#[derive(Debug, Clone)]
pub struct Config {
    /// Canonical input root.
    pub input: PathBuf,
    /// Canonical output root (may not exist yet in dry-run).
    pub output: PathBuf,
    pub mode: Mode,
    pub dry_run: bool,
    pub dir_format: DirFormat,
    pub prefix: Option<PathBuf>,
    pub suffix: Option<PathBuf>,
    pub no_date_dir: PathBuf,
    pub original_names: bool,
    pub date_fields: Vec<DateField>,
    pub regex: FilenameRegex,
    pub timestamp: bool,
    pub zone: Zone,
    pub file_type: Option<Kind>,
    pub from: Option<NaiveDate>,
    pub to: Option<NaiveDate>,
    pub skip_unknown: bool,
    /// `0` = only files directly in the input folder.
    pub max_depth: Option<usize>,
    pub duplicates: Duplicates,
    pub rmdirs: bool,
    pub jobs: usize,
    pub io_jobs: usize,
    pub verbosity: Verbosity,
    pub progress: bool,
    /// Absolute path of the `--log` file, if any (excluded from the scan).
    pub log_file: Option<PathBuf>,
}

/// What happens to an input whose content already exists (plan A1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DupPolicy {
    /// File it under `OUT/<dupdir>/<path relative to INPUT>`, in the run's
    /// own mode (the default; decision A1-Q1).
    #[default]
    File,
    /// Report only; the source and its sidecars stay (`--skip-duplicates`).
    Skip,
    /// Delete the verified source (`--move --delete-duplicates`).
    Delete,
}

/// Duplicate handling for the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Duplicates {
    pub policy: DupPolicy,
    /// `OUT/<--duplicates-dir>`. Always set: the name is reserved under OUT
    /// for every policy (never scanned, never a keeper, never a target).
    pub dir: PathBuf,
}

/// How the plan handles one duplicate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DupHandling {
    /// Put it (and its sidecars) into the duplicates folder at `dst`.
    File {
        dst: PathBuf,
        sidecars: Vec<PlannedSidecar>,
    },
    /// A copy is already in the duplicates folder at `at`: nothing to do.
    AlreadyFiled { at: PathBuf },
    /// Report only.
    Report,
    /// Delete the source after re-verifying it (`--delete-duplicates`).
    Delete,
    /// `--delete-duplicates`, but the source is the same inode as the file
    /// it duplicates (a hard link): deleting it would lose nothing on disk
    /// yet remove a name the user made, so it is kept and reported.
    KeepSameFile,
}

/// Why a file was deliberately not processed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// `--file-type` excluded it (or it isn't media at all).
    FileType,
    /// Outside `--from`/`--to`.
    DateRange,
    /// No date and `--skip-unknown`.
    NoDate,
}

impl SkipReason {
    pub fn label(self) -> &'static str {
        match self {
            SkipReason::FileType => "file type",
            SkipReason::DateRange => "date range",
            SkipReason::NoDate => "no date",
        }
    }
}

/// A sidecar's planned move alongside its media file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedSidecar {
    pub src: PathBuf,
    pub dst: PathBuf,
}

/// What the plan decided for one input file. Indices into the plan's action
/// list serve as task ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Copy/move/link the file (and its sidecars) to `dst`.
    Transfer {
        src: PathBuf,
        dst: PathBuf,
        kind: Kind,
        /// False when it goes to the no-date folder.
        dated: bool,
        /// Source size at extraction (progress throughput only).
        size: u64,
        sidecars: Vec<PlannedSidecar>,
    },
    /// Identical content already exists (or is planned) at `of`.
    Duplicate {
        src: PathBuf,
        of: PathBuf,
        handling: DupHandling,
        /// The transfer that will create `of`, if it isn't there yet.
        depends_on: Option<usize>,
        size: u64,
        /// The source's `(dev, ino)`, re-checked before deleting.
        dev_ino: (u64, u64),
        /// Sidecars that stay where they are (empty when filed: they go
        /// with the duplicate).
        sidecars_left: Vec<PathBuf>,
    },
    /// The file already is its own target (organizing in place).
    AlreadyInPlace { src: PathBuf },
    /// Filtered out.
    Skip { src: PathBuf, reason: SkipReason },
    /// Could not be planned (unreadable, too many collisions …).
    Fail { src: PathBuf, error: String },
}

impl Action {
    pub fn src(&self) -> &std::path::Path {
        match self {
            Action::Transfer { src, .. }
            | Action::Duplicate { src, .. }
            | Action::AlreadyInPlace { src }
            | Action::Skip { src, .. }
            | Action::Fail { src, .. } => src,
        }
    }
}

/// Why a planned task did not run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotRun {
    Interrupted,
    DiskFull,
    /// The transfer it depended on failed.
    DependencyFailed,
}

/// What was done with a duplicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DupHow {
    Filed,
    AlreadyFiled,
    Reported,
    Deleted,
}

/// What actually happened to one action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// `sidecar_errors` holds one message per sidecar that failed; the
    /// media file itself made it.
    Transferred {
        sidecars: usize,
        sidecar_errors: Vec<String>,
    },
    /// `sidecar_errors`: one message per sidecar that could not be filed
    /// along with its duplicate.
    Duplicate {
        how: DupHow,
        sidecar_errors: Vec<String>,
    },
    AlreadyInPlace,
    Skipped(SkipReason),
    Failed(String),
    NotRun(NotRun),
}
