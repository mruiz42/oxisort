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
    pub delete_duplicates: bool,
    pub rmdirs: bool,
    pub jobs: usize,
    pub io_jobs: usize,
    pub verbosity: Verbosity,
    pub progress: bool,
    /// Absolute path of the `--log` file, if any (excluded from the scan).
    pub log_file: Option<PathBuf>,
}
