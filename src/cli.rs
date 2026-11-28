//! Command-line interface: clap definitions and validation into [`Config`].
//!
//! All validation happens here, before any change to the filesystem. Every
//! failure is a [`UsageError`], which `main` reports with exit code 2.

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use chrono::NaiveDate;
use clap::Parser;

use crate::date::{DEFAULT_REGEX, FilenameRegex, parse_tz};
use crate::dateformat::DirFormat;
use crate::model::{Config, DateField, DupPolicy, Duplicates, Kind, Mode, Verbosity, Zone};

const DATE_FIELD_HELP: &str = "Date sources, in priority order (space or comma separated, \
case-insensitive): DateTimeOriginal, CreateDate (alias DateTimeDigitized), ModifyDate \
(alias DateTime), MediaCreateDate (the video container date). Listing only EXIF fields \
sends videos to the filename/--timestamp fallbacks";

/// Sort photos and videos into date-based folders.
///
/// Files are named YYYYMMDD-HHMMSS[subsec].ext from their EXIF or video
/// capture date and placed under OUTPUT/YYYY/MM/DD. Files without a date
/// go to OUTPUT/unknown with their original name. Existing files are never
/// overwritten.
#[derive(Debug, Parser)]
#[command(name = "oxisort", version, about, long_about)]
pub struct Args {
    /// Folder to read photos and videos from.
    pub input: PathBuf,

    /// Folder to organize into (created if missing).
    pub output: PathBuf,

    /// Move files instead of copying them.
    #[arg(short = 'm', long = "move", group = "mode")]
    pub move_files: bool,

    /// Hard-link files instead of copying them (same volume only).
    #[arg(short = 'l', long, group = "mode")]
    pub link: bool,

    /// Create symbolic links (absolute) instead of copying.
    #[arg(short = 's', long, group = "mode")]
    pub symlink: bool,

    /// Show what would happen without changing anything.
    #[arg(short = 'n', long)]
    pub dry_run: bool,

    /// Folder layout. Tokens: YYYY YY MM M(July) m(Jul) DDD(day of year) DD
    /// U(week, Sunday first) W(week, Monday first). Other text is literal.
    #[arg(short = 'd', long, value_name = "FMT", default_value = "YYYY/MM/DD")]
    pub date_format: String,

    /// Keep original file names (still placed in dated folders).
    #[arg(short = 'o', long)]
    pub original_names: bool,

    /// Extra folder(s) between OUTPUT and the date folders.
    #[arg(long, value_name = "PATH")]
    pub prefix: Option<String>,

    /// Extra folder(s) below the date folders.
    #[arg(long, value_name = "PATH")]
    pub suffix: Option<String>,

    /// Folder for files without a date.
    #[arg(long, value_name = "NAME", default_value = "unknown")]
    pub no_date_dir: String,

    #[arg(
        short = 'f',
        long,
        value_name = "LIST",
        help = DATE_FIELD_HELP,
        default_value = "DateTimeOriginal CreateDate ModifyDate MediaCreateDate"
    )]
    pub date_field: String,

    /// Filename date pattern (named groups year, month, day; optional hour,
    /// minute, second).
    #[arg(short = 'r', long, value_name = "RE", default_value = DEFAULT_REGEX)]
    pub regex: String,

    /// Use the file modification time when nothing else gives a date.
    #[arg(short = 't', long)]
    pub timestamp: bool,

    /// Zone for UTC-only dates (video mvhd/MKV, mtime): UTC, Z, ±HH:MM or
    /// ±HHMM. Default: the system zone. Camera EXIF times are never shifted.
    // Negative offsets start with '-', so let clap take them as values.
    #[arg(long, value_name = "ZONE", allow_hyphen_values = true)]
    pub tz: Option<String>,

    /// Only process images or only videos (other files are skipped).
    #[arg(long, value_name = "TYPE")]
    pub file_type: Option<FileType>,

    /// Only files dated on or after this day (YYYY-MM-DD).
    #[arg(long, alias = "from-date", value_name = "DATE")]
    pub from: Option<String>,

    /// Only files dated on or before this day (YYYY-MM-DD).
    #[arg(long, alias = "to-date", value_name = "DATE")]
    pub to: Option<String>,

    /// Skip files without a date instead of putting them in the no-date folder.
    #[arg(long)]
    pub skip_unknown: bool,

    /// Folder depth to descend (0 = only files directly in INPUT).
    #[arg(long, value_name = "N")]
    pub max_depth: Option<u16>,

    /// Folder under OUTPUT that receives duplicates, with their path
    /// relative to INPUT kept [default: duplicates]. Moved with --move,
    /// copied/linked/symlinked otherwise.
    #[arg(long, value_name = "NAME", conflicts_with_all = ["skip_duplicates", "delete_duplicates"])]
    pub duplicates_dir: Option<String>,

    /// Leave duplicates where they are and only report them.
    #[arg(long, conflicts_with = "delete_duplicates")]
    pub skip_duplicates: bool,

    /// With --move: delete a source file that is a verified duplicate
    /// instead of filing it.
    // `requires` alone is met by -l/-s too (they share the `mode` group),
    // hence the explicit conflicts.
    #[arg(long, requires = "move_files", conflicts_with_all = ["link", "symlink"])]
    pub delete_duplicates: bool,

    /// With --move: remove source folders left empty (ignoring .DS_Store etc.).
    // `requires` alone is met by -l/-s too (they share the `mode` group),
    // hence the explicit conflicts.
    #[arg(long, requires = "move_files", conflicts_with_all = ["link", "symlink"])]
    pub rmdirs: bool,

    /// fsync every file and its folder before moving on: survives a power
    /// cut, but is much slower (3-15x for photo-sized copies).
    #[arg(long)]
    pub durable: bool,

    /// Threads for metadata reading [default: number of CPUs].
    #[arg(short = 'j', long, value_name = "N", value_parser = clap::value_parser!(u16).range(1..=512))]
    pub jobs: Option<u16>,

    /// Parallel file transfers and duplicate hashing (full-file reads). Use 1
    /// for spinning disks (HDD) and SD cards.
    #[arg(long, value_name = "N", default_value_t = 4, value_parser = clap::value_parser!(u16).range(1..=256))]
    pub io_jobs: u16,

    /// Show a progress bar instead of per-file lines.
    #[arg(long, conflicts_with_all = ["quiet", "verbose"])]
    pub progress: bool,

    /// No per-file lines and no summary (warnings and errors still shown).
    #[arg(short = 'q', long, conflicts_with = "verbose")]
    pub quiet: bool,

    /// More output (-v debug, -vv trace).
    #[arg(short = 'v', long, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Append a timestamped log of every action and the summary to FILE.
    #[arg(long, value_name = "FILE")]
    pub log: Option<PathBuf>,
}

/// `--file-type` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum FileType {
    Image,
    Video,
}

/// A configuration error: reported on one line, exit code 2.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct UsageError(pub String);

fn usage(msg: impl Into<String>) -> UsageError {
    UsageError(msg.into())
}

impl Args {
    /// Validates the arguments and resolves paths into a [`Config`].
    ///
    /// Reads the filesystem (input must exist; output's nearest existing
    /// ancestor is canonicalized) but never changes it.
    ///
    /// # Errors
    /// A [`UsageError`] describing the first invalid value.
    pub fn into_config(self) -> Result<Config, UsageError> {
        let mode = if self.move_files {
            Mode::Move
        } else if self.link {
            Mode::HardLink
        } else if self.symlink {
            Mode::Symlink
        } else {
            Mode::Copy
        };
        let dir_format = DirFormat::parse(&self.date_format)
            .map_err(|e| usage(format!("--date-format: {e}")))?;
        let prefix = self
            .prefix
            .as_deref()
            .map(|p| rel_path("--prefix", p))
            .transpose()?;
        let suffix = self
            .suffix
            .as_deref()
            .map(|p| rel_path("--suffix", p))
            .transpose()?;
        let no_date_dir = rel_path("--no-date-dir", &self.no_date_dir)?;
        let date_fields = parse_date_fields(&self.date_field)?;
        let regex = FilenameRegex::new(&self.regex).map_err(|e| usage(format!("--regex: {e}")))?;
        let zone = match &self.tz {
            Some(tz) => Zone::Fixed(parse_tz(tz).map_err(usage)?),
            None => Zone::System,
        };
        let from = self
            .from
            .as_deref()
            .map(|d| parse_day("--from", d))
            .transpose()?;
        let to = self
            .to
            .as_deref()
            .map(|d| parse_day("--to", d))
            .transpose()?;
        if let (Some(f), Some(t)) = (from, to) {
            if f > t {
                return Err(usage(format!("--from {f} is after --to {t}")));
            }
        }
        let verbosity = match (self.quiet, self.verbose) {
            (true, _) => Verbosity::Quiet,
            (false, 0) => Verbosity::Normal,
            (false, n) => Verbosity::Verbose(n.min(2)),
        };
        let jobs = self.jobs.map_or_else(default_jobs, usize::from);

        let input = canonical_input(&self.input)?;
        let output = resolve_output(&self.output)?;
        check_not_same_folder(&input, &output, mode)?;
        if mode == Mode::HardLink {
            let in_dev = dev_of(&input)?;
            let out_dev = dev_of(&nearest_existing(&output))?;
            check_link_volumes(in_dev, out_dev)?;
        }
        let log_file = self.log.as_deref().map(absolute).transpose()?;
        let policy = if self.delete_duplicates {
            DupPolicy::Delete
        } else if self.skip_duplicates {
            DupPolicy::Skip
        } else {
            DupPolicy::File
        };
        // The default is applied here, not by clap, so it can never trip
        // the conflict with --skip-duplicates / --delete-duplicates.
        let dup_name = dup_dir_name(self.duplicates_dir.as_deref().unwrap_or(DEFAULT_DUP_DIR))?;
        check_dup_name_free(&dup_name, prefix.as_deref(), &no_date_dir)?;
        let dup_dir = output.join(&dup_name);
        check_dup_dir(&dup_dir, &input)?;

        Ok(Config {
            input,
            output,
            mode,
            dry_run: self.dry_run,
            dir_format,
            prefix,
            suffix,
            no_date_dir,
            original_names: self.original_names,
            date_fields,
            regex,
            timestamp: self.timestamp,
            zone,
            file_type: self.file_type.map(|t| match t {
                FileType::Image => Kind::Image,
                FileType::Video => Kind::Video,
            }),
            from,
            to,
            skip_unknown: self.skip_unknown,
            // walkdir depth: files directly in INPUT are depth 1.
            max_depth: self.max_depth.map(usize::from),
            duplicates: Duplicates {
                policy,
                dir: dup_dir,
            },
            rmdirs: self.rmdirs,
            durable: self.durable,
            jobs,
            io_jobs: usize::from(self.io_jobs),
            verbosity,
            progress: self.progress,
            log_file,
        })
    }
}

/// Parses and validates arguments in one step (used by `main` and tests).
///
/// # Errors
/// clap errors (including `--help`/`--version`, which clap reports as
/// "errors" with exit code 0) are returned as `clap::Error`.
pub fn parse_from<I, T>(args: I) -> Result<Args, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    Args::try_parse_from(args)
}

fn default_jobs() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get().min(512))
}

/// Validates a relative path option: no absolute paths, `..`, empty
/// components or NUL. A trailing `/` is tolerated.
fn rel_path(flag: &str, raw: &str) -> Result<PathBuf, UsageError> {
    let trimmed = raw.trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(usage(format!("{flag} must not be empty")));
    }
    if trimmed.contains('\0') {
        return Err(usage(format!("{flag} must not contain NUL")));
    }
    if trimmed.starts_with('/') {
        return Err(usage(format!("{flag} must be a relative path")));
    }
    if trimmed.split('/').any(str::is_empty) {
        return Err(usage(format!("{flag} has an empty folder name")));
    }
    let path = PathBuf::from(trimmed);
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(usage(format!("{flag} must not contain '..'")));
    }
    Ok(path)
}

/// Default `--duplicates-dir`.
const DEFAULT_DUP_DIR: &str = "duplicates";

/// `--duplicates-dir` must be exactly one normal folder name, so the
/// duplicates folder is always a direct child of OUTPUT.
fn dup_dir_name(raw: &str) -> Result<String, UsageError> {
    let bad =
        raw.is_empty() || raw == "." || raw == ".." || raw.contains('/') || raw.contains('\0');
    if bad {
        return Err(usage(format!(
            "--duplicates-dir: '{raw}' must be a single folder name (no '/', not '.' or '..')"
        )));
    }
    Ok(raw.to_owned())
}

/// The duplicates folder and the main tree must not overlap: a first
/// output component equal to it (case-folded, as APFS compares) is refused.
/// Date folders rendered from `--date-format` are checked at plan time.
fn check_dup_name_free(
    name: &str,
    prefix: Option<&Path>,
    no_date_dir: &Path,
) -> Result<(), UsageError> {
    let (flag, path) = match prefix {
        Some(p) => ("--prefix", p),
        None => ("--no-date-dir", no_date_dir),
    };
    let first = path.components().next().map(|c| c.as_os_str());
    if first.is_some_and(|f| f.to_string_lossy().to_lowercase() == name.to_lowercase()) {
        return Err(usage(format!(
            "{flag} {} would put files inside the duplicates folder '{name}' \
             (choose another --duplicates-dir)",
            path.display()
        )));
    }
    Ok(())
}

/// The duplicates folder, if it exists, must be a real folder (a symlink
/// could redirect writes elsewhere), and INPUT must not lie inside it
/// (compared by identity, so case or normalisation can't hide it).
fn check_dup_dir(dir: &Path, input: &Path) -> Result<(), UsageError> {
    let meta = match fs::symlink_metadata(dir) {
        Ok(m) => m,
        Err(_) => return Ok(()), // created on first use
    };
    if !meta.is_dir() {
        return Err(usage(format!(
            "duplicates folder {} exists and is not a folder (or is a symlink)",
            dir.display()
        )));
    }
    let id = (meta.dev(), meta.ino());
    let inside = input
        .ancestors()
        .any(|a| fs::symlink_metadata(a).is_ok_and(|m| (m.dev(), m.ino()) == id));
    if inside {
        return Err(usage(format!(
            "input {} is inside the duplicates folder {}",
            input.display(),
            dir.display()
        )));
    }
    Ok(())
}

/// Parses `--date-field`: space or comma separated, case-insensitive,
/// aliases resolved, duplicates removed (first occurrence wins).
fn parse_date_fields(raw: &str) -> Result<Vec<DateField>, UsageError> {
    let mut fields = Vec::new();
    for name in raw.split([' ', ',']).filter(|s| !s.is_empty()) {
        let field = match name.to_ascii_lowercase().as_str() {
            "datetimeoriginal" => DateField::DateTimeOriginal,
            "createdate" | "datetimedigitized" => DateField::CreateDate,
            "modifydate" | "datetime" => DateField::ModifyDate,
            "mediacreatedate" => DateField::MediaCreateDate,
            _ => {
                return Err(usage(format!(
                    "--date-field: unknown field '{name}' (valid: DateTimeOriginal, \
                     CreateDate, DateTimeDigitized, ModifyDate, DateTime, MediaCreateDate)"
                )));
            }
        };
        if !fields.contains(&field) {
            fields.push(field);
        }
    }
    if fields.is_empty() {
        return Err(usage("--date-field: no fields given"));
    }
    Ok(fields)
}

fn parse_day(flag: &str, raw: &str) -> Result<NaiveDate, UsageError> {
    NaiveDate::parse_from_str(raw, "%Y-%m-%d").map_err(|_| {
        usage(format!(
            "{flag}: '{raw}' is not a date (expected YYYY-MM-DD)"
        ))
    })
}

fn canonical_input(path: &Path) -> Result<PathBuf, UsageError> {
    let canon =
        fs::canonicalize(path).map_err(|e| usage(format!("input {}: {e}", path.display())))?;
    if !canon.is_dir() {
        return Err(usage(format!("input {} is not a folder", path.display())));
    }
    fs::read_dir(&canon).map_err(|e| usage(format!("input {}: {e}", path.display())))?;
    Ok(canon)
}

/// Canonicalizes the output through its nearest existing ancestor, so a
/// not-yet-created output (dry-run) still gets a stable absolute path.
fn resolve_output(path: &Path) -> Result<PathBuf, UsageError> {
    let abs = absolute(path)?;
    if let Ok(meta) = fs::metadata(&abs) {
        if !meta.is_dir() {
            return Err(usage(format!(
                "output {} exists and is not a folder",
                path.display()
            )));
        }
    }
    let base = nearest_existing(&abs);
    let canon_base =
        fs::canonicalize(&base).map_err(|e| usage(format!("output {}: {e}", path.display())))?;
    let rest = abs.strip_prefix(&base).unwrap_or(Path::new(""));
    if rest.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(usage(format!(
            "output {}: '..' below a folder that doesn't exist yet",
            path.display()
        )));
    }
    Ok(canon_base.join(rest))
}

/// The path itself if it exists, else its closest existing ancestor.
fn nearest_existing(path: &Path) -> PathBuf {
    let mut cur = path;
    loop {
        if fs::symlink_metadata(cur).is_ok() {
            return cur.to_path_buf();
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => return PathBuf::from("/"),
        }
    }
}

fn absolute(path: &Path) -> Result<PathBuf, UsageError> {
    std::path::absolute(path).map_err(|e| usage(format!("{}: {e}", path.display())))
}

fn dev_of(path: &Path) -> Result<u64, UsageError> {
    fs::metadata(path)
        .map(|m| m.dev())
        .map_err(|e| usage(format!("{}: {e}", path.display())))
}

/// Copy, link and symlink modes add files to OUTPUT and keep the sources, so
/// OUTPUT == INPUT would grow the input with a second, sorted copy of itself
/// (release test F7). Folders are compared by `(dev, ino)`, so any spelling
/// of the same folder (symlink, case, Unicode form, `.`/`..`, bind mount) is
/// caught. `-m` organizes in place and stays allowed. A missing OUTPUT can't
/// be INPUT.
fn check_not_same_folder(input: &Path, output: &Path, mode: Mode) -> Result<(), UsageError> {
    if mode == Mode::Move {
        return Ok(());
    }
    let Ok(out_meta) = fs::metadata(output) else {
        return Ok(());
    };
    let in_meta =
        fs::metadata(input).map_err(|e| usage(format!("input {}: {e}", input.display())))?;
    if (out_meta.dev(), out_meta.ino()) == (in_meta.dev(), in_meta.ino()) {
        let verb = match mode {
            Mode::HardLink => "link",
            Mode::Symlink => "symlink",
            _ => "copy",
        };
        return Err(usage(format!(
            "output {} is the input folder: {verb} mode would add files to it \
             (use -m to organize a folder in place)",
            output.display()
        )));
    }
    Ok(())
}

/// Hard links can't cross volumes; reject that before touching anything.
pub(crate) fn check_link_volumes(input_dev: u64, output_dev: u64) -> Result<(), UsageError> {
    if input_dev == output_dev {
        Ok(())
    } else {
        Err(usage(
            "--link: hard links can't cross volumes (input and output are on different volumes)",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(extra: &[&str]) -> Result<Args, clap::Error> {
        let mut v = vec!["oxisort"];
        v.extend_from_slice(extra);
        v.extend_from_slice(&["in", "out"]);
        parse_from(v)
    }

    fn kind(extra: &[&str]) -> clap::error::ErrorKind {
        args(extra).unwrap_err().kind()
    }

    #[test]
    fn modes_conflict() {
        use clap::error::ErrorKind::ArgumentConflict;
        assert_eq!(kind(&["--move", "--link"]), ArgumentConflict);
        assert_eq!(kind(&["-l", "-s"]), ArgumentConflict);
        assert_eq!(kind(&["--progress", "--quiet"]), ArgumentConflict);
        assert_eq!(kind(&["--progress", "-v"]), ArgumentConflict);
        assert_eq!(kind(&["-q", "-v"]), ArgumentConflict);
    }

    #[test]
    fn move_only_flags_require_move() {
        use clap::error::ErrorKind::MissingRequiredArgument;
        assert_eq!(kind(&["--delete-duplicates"]), MissingRequiredArgument);
        assert_eq!(kind(&["--rmdirs"]), MissingRequiredArgument);
        // The modes share one clap group, and a `requires` on a group
        // member is met by any member, so -l/-s need their own refusal (F3).
        use clap::error::ErrorKind::ArgumentConflict;
        for mode in ["-l", "-s"] {
            assert_eq!(kind(&[mode, "--delete-duplicates"]), ArgumentConflict);
            assert_eq!(kind(&[mode, "--rmdirs"]), ArgumentConflict);
        }
        assert!(args(&["--move", "--rmdirs", "--delete-duplicates"]).is_ok());
    }

    /// U22: the three duplicate policies exclude each other, and every
    /// guard is a usage error (exit 2 in `main`).
    #[test]
    fn duplicate_flags() {
        use clap::error::ErrorKind::ArgumentConflict;
        assert_eq!(
            kind(&["--duplicates-dir", "d", "--skip-duplicates"]),
            ArgumentConflict
        );
        assert_eq!(
            kind(&["-m", "--duplicates-dir", "d", "--delete-duplicates"]),
            ArgumentConflict
        );
        assert_eq!(
            kind(&["-m", "--skip-duplicates", "--delete-duplicates"]),
            ArgumentConflict
        );
        assert!(args(&["--skip-duplicates"]).is_ok());
        assert!(args(&["--duplicates-dir", "dups"]).is_ok());

        for bad in ["", ".", "..", "a/b", "/x"] {
            assert!(dup_dir_name(bad).is_err(), "{bad:?}");
        }
        assert_eq!(dup_dir_name("Dups 2024").unwrap(), "Dups 2024");

        let p = |s: &str| PathBuf::from(s);
        assert!(
            check_dup_name_free("duplicates", Some(&p("Duplicates/x")), &p("unknown")).is_err()
        );
        assert!(check_dup_name_free("dups", None, &p("DUPS")).is_err());
        assert!(check_dup_name_free("dups", Some(&p("photos")), &p("dups")).is_ok());
        assert!(check_dup_name_free("duplicates", None, &p("unknown")).is_ok());

        let d = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(d.path()).unwrap();
        let dups = root.join("duplicates");
        let input = root.join("in");
        fs::create_dir(&input).unwrap();
        assert!(check_dup_dir(&dups, &input).is_ok(), "missing is fine");
        fs::write(&dups, b"file").unwrap();
        assert!(
            check_dup_dir(&dups, &input)
                .unwrap_err()
                .0
                .contains("not a folder")
        );
        fs::remove_file(&dups).unwrap();
        std::os::unix::fs::symlink(&input, &dups).unwrap();
        assert!(
            check_dup_dir(&dups, &input)
                .unwrap_err()
                .0
                .contains("symlink")
        );
        fs::remove_file(&dups).unwrap();
        fs::create_dir_all(dups.join("sub")).unwrap();
        assert!(check_dup_dir(&dups, &input).is_ok());
        // Another spelling, same folder, where the volume is case-insensitive.
        let inner = match root.join("Duplicates/sub") {
            p if p.exists() => p,
            _ => dups.join("sub"),
        };
        assert!(
            check_dup_dir(&dups, &inner)
                .unwrap_err()
                .0
                .contains("inside")
        );
    }

    #[test]
    fn numeric_ranges() {
        assert!(args(&["--jobs", "0"]).is_err());
        assert!(args(&["--jobs", "513"]).is_err());
        assert!(args(&["--io-jobs", "257"]).is_err());
        assert!(args(&["--io-jobs", "1", "-j", "512"]).is_ok());
        assert!(args(&["--max-depth", "70000"]).is_err());
        assert_eq!(args(&[]).unwrap().io_jobs, 4);
    }

    #[test]
    fn negative_tz_is_a_value() {
        assert_eq!(
            args(&["--tz", "-08:00"]).unwrap().tz.as_deref(),
            Some("-08:00")
        );
    }

    #[test]
    fn aliases() {
        let a = args(&["--from-date", "2020-01-01", "--to-date", "2020-12-31"]).unwrap();
        assert_eq!(a.from.as_deref(), Some("2020-01-01"));
        assert_eq!(a.to.as_deref(), Some("2020-12-31"));
    }

    #[test]
    fn date_fields() {
        use DateField::*;
        assert_eq!(
            parse_date_fields("DateTimeOriginal CreateDate ModifyDate MediaCreateDate").unwrap(),
            DateField::DEFAULT
        );
        assert_eq!(
            parse_date_fields("datetimedigitized,DateTime createdate").unwrap(),
            vec![CreateDate, ModifyDate]
        );
        assert!(
            parse_date_fields("FileModifyDate")
                .unwrap_err()
                .0
                .contains("valid:")
        );
        assert!(parse_date_fields(" , ").is_err());
    }

    #[test]
    fn relative_paths() {
        assert_eq!(rel_path("--prefix", "a/b/").unwrap(), PathBuf::from("a/b"));
        assert!(rel_path("--prefix", "/abs").is_err());
        assert!(rel_path("--prefix", "a/../b").is_err());
        assert!(rel_path("--prefix", "a//b").is_err());
        assert!(rel_path("--prefix", "").is_err());
        assert!(rel_path("--prefix", "a\0b").is_err());
    }

    #[test]
    fn config_validation() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("in");
        fs::create_dir(&input).unwrap();
        let build = |extra: &[&str]| {
            let mut v = vec!["oxisort".to_owned()];
            v.extend(extra.iter().map(|s| s.to_string()));
            v.push(input.display().to_string());
            v.push(dir.path().join("new/out").display().to_string());
            parse_from(v).unwrap().into_config()
        };
        let cfg = build(&["--tz", "+02:00"]).unwrap();
        assert_eq!(
            cfg.output,
            fs::canonicalize(dir.path()).unwrap().join("new/out")
        );
        assert!(build(&["--tz", "+25:00"]).is_err());
        assert!(build(&["--from", "2020-02-30"]).is_err());
        assert!(build(&["--from", "2021-01-01", "--to", "2020-01-01"]).is_err());
        assert!(build(&["--from", "2020-01-01", "--to", "2020-01-01"]).is_ok());
        assert!(build(&["--regex", "(?P<year>\\d{4})"]).is_err());
        assert!(build(&["-d", "YYYY/../MM"]).is_err());
        assert!(build(&["--no-date-dir", "../x"]).is_err());

        let file = dir.path().join("file");
        fs::write(&file, b"x").unwrap();
        let not_dir = parse_from(["oxisort", file.to_str().unwrap(), "out"])
            .unwrap()
            .into_config();
        assert!(not_dir.unwrap_err().0.contains("not a folder"));
        let out_is_file = parse_from(["oxisort", input.to_str().unwrap(), file.to_str().unwrap()])
            .unwrap()
            .into_config();
        assert!(out_is_file.unwrap_err().0.contains("not a folder"));
    }

    /// F7: OUTPUT == INPUT is refused for copy, link and symlink, by
    /// identity, and allowed for move or a missing OUTPUT.
    #[test]
    fn same_folder_refused_except_for_move() {
        let d = tempfile::tempdir().unwrap();
        let input = fs::canonicalize(d.path()).unwrap().join("in");
        fs::create_dir(&input).unwrap();
        let alias = d.path().join("alias");
        std::os::unix::fs::symlink(&input, &alias).unwrap();
        for mode in [Mode::Copy, Mode::HardLink, Mode::Symlink] {
            for out in [&input, &alias, &input.join(".")] {
                let err = check_not_same_folder(&input, out, mode).unwrap_err();
                assert!(err.0.contains("is the input folder"), "{mode:?} {out:?}");
            }
        }
        assert!(check_not_same_folder(&input, &input, Mode::Move).is_ok());
        assert!(check_not_same_folder(&input, &input.join("new"), Mode::Copy).is_ok());
        let other = d.path().join("other");
        fs::create_dir(&other).unwrap();
        assert!(check_not_same_folder(&input, &other, Mode::Copy).is_ok());
    }

    #[test]
    fn link_preflight() {
        assert!(check_link_volumes(1, 1).is_ok());
        assert!(
            check_link_volumes(1, 2)
                .unwrap_err()
                .0
                .contains("can't cross volumes")
        );
    }
}
