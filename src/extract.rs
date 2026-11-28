//! Phase 2: per-file stat, classification, metadata read and date
//! resolution, in parallel on the CPU pool. Output order equals input order.

use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use chrono::{DateTime, Utc};
use indicatif::ProgressBar;
use nom_exif::MediaParser;
use rayon::prelude::*;

use crate::classify::{self, HEADER_LEN};
use crate::date::{ResolveOptions, resolve};
use crate::exec::Stop;
use crate::metadata::{self, RawMeta};
use crate::model::{Captured, Kind};
use crate::scan::ScanEntry;

/// One input file with everything planning needs.
#[derive(Debug, Clone)]
pub struct Extracted {
    pub entry: ScanEntry,
    pub size: u64,
    pub mtime: Option<DateTime<Utc>>,
    pub dev: u64,
    pub ino: u64,
    pub kind: Kind,
    pub date: Option<Captured>,
}

/// A file that couldn't be read at all (vanished, permission denied …).
#[derive(Debug, Clone)]
pub struct ExtractError {
    pub entry: ScanEntry,
    pub message: String,
}

/// Extracts every entry on `pool`, preserving order.
///
/// `stop` is checked before each file: once it is set, the remaining files
/// are not opened and come back as an [`ExtractError`] ("interrupted"), so a
/// Ctrl-C during a long extraction returns within one file per worker. The
/// caller discards such a result rather than planning from it.
pub fn extract_all(
    entries: Vec<ScanEntry>,
    opts: &ResolveOptions<'_>,
    pool: &rayon::ThreadPool,
    stop: &Stop,
    bar: &ProgressBar,
) -> Vec<Result<Extracted, ExtractError>> {
    pool.install(|| {
        entries
            .into_par_iter()
            .map_init(MediaParser::new, |parser, entry| {
                if stop.reason().is_some() {
                    return Err(ExtractError {
                        entry,
                        message: "interrupted".to_owned(),
                    });
                }
                let r = extract_one(parser, entry, opts);
                bar.inc(1);
                r
            })
            .collect()
    })
}

/// Extracts one file. Metadata problems are not errors (the file just has
/// fewer date candidates); only failing to stat or open it is.
pub fn extract_one(
    parser: &mut MediaParser,
    entry: ScanEntry,
    opts: &ResolveOptions<'_>,
) -> Result<Extracted, ExtractError> {
    let fail = |entry: ScanEntry, what: &str, e: io::Error| ExtractError {
        message: format!("{what}: {e}"),
        entry,
    };
    let meta = match std::fs::symlink_metadata(&entry.path) {
        Ok(m) if m.is_file() => m,
        Ok(_) => {
            let e = io::Error::new(io::ErrorKind::InvalidInput, "no longer a regular file");
            return Err(fail(entry, "stat", e));
        }
        Err(e) => return Err(fail(entry, "stat", e)),
    };
    let mut file = match File::open(&entry.path) {
        Ok(f) => f,
        Err(e) => return Err(fail(entry, "open", e)),
    };
    let header = match read_header(&mut file) {
        Ok(h) => h,
        Err(e) => return Err(fail(entry, "read", e)),
    };
    let path: &Path = &entry.path;
    let ext = path.extension().and_then(|e| e.to_str());
    let class = classify::classify(&header, ext);
    let raw = if class.kind == Kind::Other {
        RawMeta::default()
    } else {
        metadata::read(parser, path, file, class.mime)
    };
    for w in &raw.warnings {
        log::debug!("metadata {}: {w}", path.display());
    }
    let mtime = meta.modified().ok().map(DateTime::<Utc>::from);
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let date = resolve(&raw, class.kind, &name, mtime, opts);
    Ok(Extracted {
        size: meta.len(),
        mtime,
        dev: meta.dev(),
        ino: meta.ino(),
        kind: class.kind,
        date,
        entry,
    })
}

/// Reads up to [`HEADER_LEN`] bytes (fewer for tiny files).
fn read_header(file: &mut File) -> io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(HEADER_LEN);
    file.take(HEADER_LEN as u64).read_to_end(&mut buf)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stopped_extraction_opens_nothing() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("a.jpg");
        std::fs::write(&path, b"not really a jpeg").unwrap();
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .unwrap();
        let stop = Stop::new();
        stop.request(crate::model::NotRun::Interrupted);
        let regex = crate::date::FilenameRegex::default();
        let opts = ResolveOptions {
            date_fields: &[],
            regex: &regex,
            timestamp: false,
            zone: crate::model::Zone::System,
        };
        let entry = ScanEntry {
            path,
            sidecars: vec![],
        };
        let got = extract_all(vec![entry], &opts, &pool, &stop, &ProgressBar::hidden());
        assert!(
            matches!(&got[..], [Err(e)] if e.message == "interrupted"),
            "{got:?}"
        );
    }
}
