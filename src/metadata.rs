//! Reads raw date metadata with nom-exif, with fallbacks when it fails.
//!
//! - kamadak-exif, only for formats it scans cheaply (JPEG, PNG, WebP,
//!   HEIF/AVIF). Never for TIFF/RAW, because kamadak reads those to EOF
//!   (investigation risk 7).
//! - `tiffdate`, a seek-based IFD0 → ExifIFD date reader, for everything
//!   else that starts with a TIFF header (ARW, NEF, DNG, CR2 …). It reads a
//!   few hundred bytes; on a non-TIFF file it stops after 8.
//!
//! Fallbacks run only after nom-exif failed, so files it reads cost the same.
//!
//! Parsing is wrapped in `catch_unwind`: a parser panic on a hostile or
//! broken file becomes a warning for that file, never a crash.

use std::fs::File;
use std::io::{BufReader, Seek, SeekFrom};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

use chrono::{DateTime, FixedOffset, NaiveDateTime};
use nom_exif::{EntryValue, ExifTag, MediaKind, MediaParser, MediaSource, TrackInfoTag};

use crate::date::{normalize_subsec, parse_datestring};

/// One EXIF date: camera wall clock plus optional sub-second digits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExifStamp {
    pub wall: NaiveDateTime,
    pub subsec: Option<String>,
}

/// Raw date candidates read from one file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RawMeta {
    /// `[DateTimeOriginal, CreateDate, ModifyDate]`.
    pub exif: [Option<ExifStamp>; 3],
    /// Container creation date (Apple key with its offset, or mvhd/MKV UTC).
    pub media_create: Option<DateTime<FixedOffset>>,
    /// Why metadata couldn't be read (logged at debug level; not an error).
    pub warnings: Vec<String>,
}

/// MIME types for which the kamadak fallback is cheap.
const FALLBACK_MIMES: &[&str] = &[
    "image/jpeg",
    "image/png",
    "image/webp",
    "image/heif",
    "image/heic",
    "image/avif",
];

/// Reads date metadata from an open file positioned anywhere.
///
/// `parser` is reused across calls (one per worker thread); it is replaced
/// with a fresh one if parsing panics. `mime` is the sniffed content type,
/// which gates the fallback.
pub fn read(parser: &mut MediaParser, path: &Path, mut file: File, mime: Option<&str>) -> RawMeta {
    let mut meta = RawMeta::default();
    if let Err(e) = file.seek(SeekFrom::Start(0)) {
        meta.warnings.push(format!("seek failed: {e}"));
        return meta;
    }
    let nom = catch_unwind(AssertUnwindSafe(|| read_nom(parser, file)));
    let nom_err = match nom {
        Ok(Ok(found)) => {
            meta.exif = found.exif;
            meta.media_create = found.media_create;
            return meta;
        }
        Ok(Err(e)) => e,
        Err(_) => {
            *parser = MediaParser::new();
            "metadata parser panicked".to_owned()
        }
    };
    meta.warnings.push(format!("nom-exif: {nom_err}"));
    let (name, fallback): (&str, fn(&Path) -> Result<_, String>) =
        if mime.is_some_and(|m| FALLBACK_MIMES.contains(&m)) {
            ("kamadak-exif", read_kamadak)
        } else {
            ("tiff", read_tiff)
        };
    match catch_unwind(|| fallback(path)) {
        Ok(Ok(exif)) => meta.exif = exif,
        Ok(Err(e)) => meta.warnings.push(format!("{name}: {e}")),
        Err(_) => meta.warnings.push(format!("{name} panicked")),
    }
    meta
}

fn read_tiff(path: &Path) -> Result<[Option<ExifStamp>; 3], String> {
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    crate::tiffdate::read(&mut file)
}

struct NomFound {
    exif: [Option<ExifStamp>; 3],
    media_create: Option<DateTime<FixedOffset>>,
}

fn read_nom(parser: &mut MediaParser, file: File) -> Result<NomFound, String> {
    let ms = MediaSource::seekable(file).map_err(|e| e.to_string())?;
    match ms.kind() {
        MediaKind::Image => {
            let exif: nom_exif::Exif = parser.parse_exif(ms).map_err(|e| e.to_string())?.into();
            let stamp = |date: ExifTag, sub: ExifTag| {
                let (wall, frac) = nom_datetime(exif.get(date)?)?;
                let subsec = exif
                    .get(sub)
                    .and_then(EntryValue::as_str)
                    .and_then(normalize_subsec)
                    .or(frac);
                Some(ExifStamp { wall, subsec })
            };
            Ok(NomFound {
                exif: [
                    stamp(ExifTag::DateTimeOriginal, ExifTag::SubSecTimeOriginal),
                    stamp(ExifTag::CreateDate, ExifTag::SubSecTimeDigitized),
                    stamp(ExifTag::ModifyDate, ExifTag::SubSecTime),
                ],
                media_create: None,
            })
        }
        MediaKind::Track => {
            let track = parser.parse_track(ms).map_err(|e| e.to_string())?;
            let media_create = match track.get(TrackInfoTag::CreateDate) {
                Some(EntryValue::DateTime(dt)) => Some(*dt),
                _ => None,
            };
            Ok(NomFound {
                exif: Default::default(),
                media_create,
            })
        }
    }
}

/// Converts a nom-exif date value to wall clock, dropping any offset.
fn nom_datetime(v: &EntryValue) -> Option<(NaiveDateTime, Option<String>)> {
    match v {
        EntryValue::NaiveDateTime(n) => Some((*n, None)),
        EntryValue::DateTime(dt) => Some((dt.naive_local(), None)),
        EntryValue::Text(s) => parse_datestring(s),
        _ => None,
    }
}

fn read_kamadak(path: &Path) -> Result<[Option<ExifStamp>; 3], String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let exif = exif::Reader::new()
        .read_from_container(&mut BufReader::new(file))
        .map_err(|e| e.to_string())?;
    let ascii = |tag: exif::Tag| -> Option<String> {
        let field = exif.get_field(tag, exif::In::PRIMARY)?;
        match &field.value {
            exif::Value::Ascii(parts) => parts
                .first()
                .map(|b| String::from_utf8_lossy(b).into_owned()),
            _ => None,
        }
    };
    let stamp = |date: exif::Tag, sub: exif::Tag| {
        let (wall, frac) = parse_datestring(&ascii(date)?)?;
        let subsec = ascii(sub).as_deref().and_then(normalize_subsec).or(frac);
        Some(ExifStamp { wall, subsec })
    };
    Ok([
        stamp(exif::Tag::DateTimeOriginal, exif::Tag::SubSecTimeOriginal),
        stamp(exif::Tag::DateTimeDigitized, exif::Tag::SubSecTimeDigitized),
        stamp(exif::Tag::DateTime, exif::Tag::SubSecTime),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    fn read_fixture(name: &str, mime: Option<&str>) -> RawMeta {
        let path = fixture(name);
        let file = File::open(&path).unwrap();
        read(&mut MediaParser::new(), &path, file, mime)
    }

    #[test]
    fn jpeg_dto_with_subsec_keeps_wall_clock() {
        let m = read_fixture("dto.jpg", Some("image/jpeg"));
        let dto = m.exif[0].as_ref().expect("DateTimeOriginal");
        assert_eq!(dto.wall.to_string(), "2017-01-02 03:04:05");
        assert_eq!(dto.subsec.as_deref(), Some("123"));
        assert_eq!(
            m.exif[1].as_ref().unwrap().wall.to_string(),
            "2017-01-02 03:04:06"
        );
    }

    #[test]
    fn apple_key_and_mvhd() {
        let apple = read_fixture("apple.mov", None).media_create.unwrap();
        assert_eq!(apple.to_rfc3339(), "2021-06-07T08:09:10+02:00");
        let mvhd = read_fixture("mvhd_only.mp4", None).media_create.unwrap();
        assert_eq!(mvhd.to_rfc3339(), "2022-07-08T02:10:11+00:00");
    }

    #[test]
    fn container_formats() {
        for name in ["a.heic", "a.png", "a.tiff", "fake.arw", "a.webp"] {
            let m = read_fixture(name, None);
            assert!(m.exif[0].is_some(), "{name}: {:?}", m.warnings);
        }
        let mkv = read_fixture("a.mkv", None).media_create.unwrap();
        assert_eq!(mkv.to_rfc3339(), "2020-05-06T07:08:09+00:00");
        // mvhd = 0 is reported by nom-exif as 1904; date::resolve drops it.
        let zero = read_fixture("nodate.mp4", None).media_create;
        assert!(zero.is_none_or(|d| d.timestamp() < 0), "{zero:?}");
    }

    /// A7C II layout: an IFD0 entry's data ends past nom-exif's buffer, so
    /// nom-exif fails the whole parse; the date must still come out.
    #[test]
    fn tiff_raw_with_far_ifd0_data_still_has_its_date() {
        let m = read_fixture("sr2_far_data.arw", Some("image/tiff"));
        assert!(
            m.warnings.iter().any(|w| w.contains("Incomplete")),
            "the fixture must reproduce the nom-exif failure: {:?}",
            m.warnings
        );
        let dto = m.exif[0].as_ref().expect("DateTimeOriginal");
        assert_eq!(dto.wall.to_string(), "2026-01-02 06:11:39");
        assert_eq!(dto.subsec.as_deref(), Some("705"));
        assert_eq!(
            m.exif[1].as_ref().unwrap().wall.to_string(),
            "2026-01-02 06:11:39"
        );
        assert_eq!(
            m.exif[2].as_ref().unwrap().wall.to_string(),
            "2026-01-02 06:11:40"
        );
    }

    #[test]
    fn kamadak_fallback_reads_jpeg() {
        let exif = read_kamadak(&fixture("dto.jpg")).unwrap();
        assert_eq!(exif[0].as_ref().unwrap().subsec.as_deref(), Some("123"));
    }

    #[test]
    fn unsupported_content_is_a_warning() {
        let m = read_fixture("notes.txt", None);
        assert_eq!(m.exif, [None, None, None]);
        assert!(!m.warnings.is_empty());
    }
}
