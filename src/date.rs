//! Capture-date logic: parsing EXIF-style date strings, the filename regex,
//! `--tz`, and [`resolve`], which picks one date per file.
//!
//! Everything here is pure: the zone is injected through [`ResolveOptions`],
//! so tests never depend on the machine's time zone.

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, Utc};
use regex::Regex;

use crate::metadata::RawMeta;
use crate::model::{Captured, DateField, DateSource, Kind, Zone};

/// Default filename pattern. Slightly broader than phockup's: it also matches
/// `20160915_123456.jpg` with no prefix, and limits years to 1900–2099 to
/// avoid false matches on long digit runs.
pub const DEFAULT_REGEX: &str = r"(?:^|\D)(?P<year>(?:19|20)\d{2})(?P<month>\d{2})(?P<day>\d{2})[_-]?(?P<hour>\d{2})(?P<minute>\d{2})(?P<second>\d{2})";

/// The earliest container date accepted. `mvhd = 0` decodes to 1904 and
/// version-1 `mvhd` boxes decode to garbage, so anything at or before the
/// Unix epoch is treated as "no date".
const MIN_MEDIA_DATE_UTC: i64 = 0;

/// A compiled `--regex` with the required named groups.
#[derive(Debug, Clone)]
pub struct FilenameRegex {
    re: Regex,
}

/// Why a `--regex` value was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegexError {
    #[error("invalid regex: {0}")]
    Syntax(String),
    #[error("regex must contain the named groups 'year', 'month' and 'day' (missing '{0}')")]
    MissingGroup(&'static str),
}

impl FilenameRegex {
    /// Compiles a filename pattern.
    ///
    /// # Errors
    /// [`RegexError::Syntax`] if it doesn't compile; [`RegexError::MissingGroup`]
    /// if `year`, `month` or `day` is missing (`hour`/`minute`/`second` are
    /// optional and default to 0).
    pub fn new(pattern: &str) -> Result<Self, RegexError> {
        let re = Regex::new(pattern).map_err(|e| RegexError::Syntax(e.to_string()))?;
        for group in ["year", "month", "day"] {
            if !re.capture_names().flatten().any(|n| n == group) {
                return Err(RegexError::MissingGroup(group));
            }
        }
        Ok(FilenameRegex { re })
    }

    /// Extracts a date from a file name. An impossible date (month 13,
    /// Feb 30, hour 25 …) counts as no match.
    pub fn match_name(&self, name: &str) -> Option<NaiveDateTime> {
        let caps = self.re.captures(name)?;
        let num = |g: &str| -> Option<u32> {
            match caps.name(g) {
                Some(m) => m.as_str().parse().ok(),
                None => Some(0),
            }
        };
        let year = i32::try_from(num("year")?).ok()?;
        NaiveDate::from_ymd_opt(year, num("month")?, num("day")?)?.and_hms_opt(
            num("hour")?,
            num("minute")?,
            num("second")?,
        )
    }
}

impl Default for FilenameRegex {
    fn default() -> Self {
        FilenameRegex::new(DEFAULT_REGEX).expect("default regex is valid")
    }
}

/// Parses a `--tz` value: `UTC`, `Z`, `±HH:MM` or `±HHMM` (hours ≤ 14,
/// minutes < 60).
///
/// # Errors
/// Returns a human-readable message for anything else.
pub fn parse_tz(s: &str) -> Result<FixedOffset, String> {
    let bad = || format!("invalid --tz '{s}': expected UTC, Z, +HH:MM or -HHMM");
    if s.eq_ignore_ascii_case("utc") || s == "Z" {
        return Ok(FixedOffset::east_opt(0).expect("zero offset"));
    }
    parse_offset(s).ok_or_else(bad)
}

/// Parses `±HH:MM` / `±HHMM` into an offset (hours ≤ 14, minutes < 60).
fn parse_offset(s: &str) -> Option<FixedOffset> {
    let (sign, rest) = match s.as_bytes().first()? {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => return None,
    };
    let digits: String = rest.chars().filter(|&c| c != ':').collect();
    let colon_ok = rest.len() == 4 || (rest.len() == 5 && rest.as_bytes()[2] == b':');
    if !colon_ok || digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let hours: i32 = digits[..2].parse().ok()?;
    let minutes: i32 = digits[2..].parse().ok()?;
    if hours > 14 || minutes >= 60 {
        return None;
    }
    FixedOffset::east_opt(sign * (hours * 3600 + minutes * 60))
}

/// Parses an EXIF-style date string into wall-clock time plus the raw
/// fractional digits.
///
/// Accepts `:` or `-` between date parts, a space or `T` before the time, an
/// optional `.digits` fraction, and an optional `Z` / `±HH:MM` / `±HHMM`
/// suffix. The suffix is parsed but **not applied**: the result is the wall
/// clock as written (this is what fixes phockup's MKV `Z` failure).
/// Empty, all-separator and `0000…` values are rejected.
pub fn parse_datestring(raw: &str) -> Option<(NaiveDateTime, Option<String>)> {
    let s = raw.trim_matches(|c: char| c == ' ' || c == '\0');
    if s.is_empty() || s.starts_with("0000") || s.chars().all(|c| c == ':' || c == ' ') {
        return None;
    }
    let s = strip_zone_suffix(s);
    let (main, frac) = match s.split_once('.') {
        Some((m, f)) => (m, normalize_subsec(f)),
        None => (s, None),
    };
    // Normalise separators so one strftime pattern covers every spelling.
    let norm: String = main
        .char_indices()
        .map(|(i, c)| match (i, c) {
            (4 | 7, '-') => ':',
            (10, 'T') => ' ',
            _ => c,
        })
        .collect();
    let wall = NaiveDateTime::parse_from_str(&norm, "%Y:%m:%d %H:%M:%S").ok()?;
    Some((wall, frac))
}

/// Removes a trailing `Z`, `±HH:MM` or `±HHMM` from the time part.
fn strip_zone_suffix(s: &str) -> &str {
    if let Some(rest) = s.strip_suffix(['Z', 'z']) {
        return rest;
    }
    // Only look for a sign in the time part, so date dashes aren't mistaken
    // for a negative offset.
    if s.len() > 11 {
        let time_part = &s[11..];
        if let Some(pos) = time_part.rfind(['+', '-']) {
            if parse_offset(&time_part[pos..]).is_some() {
                return &s[..11 + pos];
            }
        }
    }
    s
}

/// Keeps the leading ASCII digits of a sub-second value; empty means none.
pub fn normalize_subsec(s: &str) -> Option<String> {
    let digits: String = s
        .trim_start_matches(' ')
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    (!digits.is_empty()).then_some(digits)
}

/// Everything [`resolve`] needs besides the file's own data.
#[derive(Debug, Clone)]
pub struct ResolveOptions<'a> {
    pub date_fields: &'a [DateField],
    pub regex: &'a FilenameRegex,
    pub timestamp: bool,
    pub zone: Zone,
}

/// Picks the capture date for one file.
///
/// 1. Each `--date-field` in order (EXIF wall clock; or the container date:
///    an Apple key with a non-zero offset keeps its wall clock, a UTC date is
///    converted with the zone).
/// 2. For media files: the filename regex.
/// 3. For media files with `--timestamp`: mtime converted with the zone.
///
/// Non-media files (`Kind::Other`) only get a date from metadata, which in
/// practice means never.
pub fn resolve(
    raw: &RawMeta,
    kind: Kind,
    file_name: &str,
    mtime: Option<DateTime<Utc>>,
    opts: &ResolveOptions<'_>,
) -> Option<Captured> {
    for &field in opts.date_fields {
        if let Some(found) = from_field(raw, field, opts.zone) {
            return Some(found);
        }
    }
    if kind == Kind::Other {
        return None;
    }
    if let Some(wall) = opts.regex.match_name(file_name) {
        return Some(Captured {
            wall,
            subsec: None,
            source: DateSource::Filename,
        });
    }
    if opts.timestamp {
        if let Some(m) = mtime {
            return Some(Captured {
                wall: opts.zone.wall_from_utc_dt(m),
                subsec: None,
                source: DateSource::Mtime,
            });
        }
    }
    None
}

fn from_field(raw: &RawMeta, field: DateField, zone: Zone) -> Option<Captured> {
    match field.exif_index() {
        Some(i) => {
            let stamp = raw.exif[i].as_ref()?;
            let source = match field {
                DateField::DateTimeOriginal => DateSource::ExifDateTimeOriginal,
                DateField::CreateDate => DateSource::ExifCreateDate,
                _ => DateSource::ExifModifyDate,
            };
            Some(Captured {
                wall: stamp.wall,
                subsec: stamp.subsec.clone(),
                source,
            })
        }
        None => {
            let dt = raw.media_create?;
            if dt.timestamp() <= MIN_MEDIA_DATE_UTC {
                return None;
            }
            // nom-exif merges the Apple key into the same tag as mvhd; only
            // the offset tells them apart (plan risk R4: a +00:00 Apple key is
            // treated as UTC).
            let (wall, source) = if dt.offset().local_minus_utc() != 0 {
                (dt.naive_local(), DateSource::VideoLocal)
            } else {
                (zone.wall_from_utc(dt.naive_utc()), DateSource::VideoUtc)
            };
            Some(Captured {
                wall,
                subsec: None,
                source,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::ExifStamp;

    fn dt(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap()
    }

    type Expected<'a> = Option<(NaiveDateTime, Option<&'a str>)>;

    #[test]
    fn datestring_variants() {
        let w = dt("2017-01-02 03:04:05");
        let cases: [(&str, Expected); 14] = [
            ("2017:01:02 03:04:05", Some((w, None))),
            ("2017-01-02 03:04:05", Some((w, None))),
            ("2017-01-02T03:04:05", Some((w, None))),
            ("2017:01:02 03:04:05.123", Some((w, Some("123")))),
            ("2017:01:02 03:04:05Z", Some((w, None))),
            (
                "2020:05:06 03:04:05z",
                Some((dt("2020-05-06 03:04:05"), None)),
            ),
            ("2017:01:02 03:04:05+02:00", Some((w, None))),
            ("2017:01:02 03:04:05-0530", Some((w, None))),
            ("2017:01:02 03:04:05.5+02:00", Some((w, Some("5")))),
            ("  2017:01:02 03:04:05\0\0", Some((w, None))),
            ("0000:00:00 00:00:00", None),
            ("    :  :     :  :  ", None),
            ("", None),
            ("2017:13:02 03:04:05", None),
        ];
        for (input, want) in cases {
            let got = parse_datestring(input);
            let want = want.map(|(w, f)| (w, f.map(str::to_owned)));
            assert_eq!(got, want, "input {input:?}");
        }
    }

    #[test]
    fn subsec_normalization() {
        assert_eq!(normalize_subsec("123"), Some("123".into()));
        assert_eq!(normalize_subsec("05 "), Some("05".into()));
        assert_eq!(normalize_subsec("12+02:00"), Some("12".into()));
        assert_eq!(normalize_subsec(""), None);
        assert_eq!(normalize_subsec("abc"), None);
    }

    #[test]
    fn tz_values() {
        assert_eq!(parse_tz("UTC").unwrap().local_minus_utc(), 0);
        assert_eq!(parse_tz("Z").unwrap().local_minus_utc(), 0);
        assert_eq!(parse_tz("+02:00").unwrap().local_minus_utc(), 7200);
        assert_eq!(parse_tz("-0530").unwrap().local_minus_utc(), -19800);
        assert_eq!(parse_tz("+14:00").unwrap().local_minus_utc(), 50400);
        for bad in [
            "+15:00", "+02:60", "0200", "+2:00", "+02:0", "EST", "+02-00", "",
        ] {
            assert!(parse_tz(bad).is_err(), "{bad} should fail");
        }
    }

    #[test]
    fn filename_regex_default() {
        let re = FilenameRegex::default();
        let want = Some(dt("2016-09-15 12:34:56"));
        assert_eq!(re.match_name("IMG_20160915_123456.jpg"), want);
        assert_eq!(re.match_name("20160915_123456.jpg"), want);
        assert_eq!(re.match_name("VID-20160915-123456.mp4"), want);
        assert_eq!(re.match_name("20160915123456.jpg"), want);
        assert_eq!(re.match_name("IMG_20161315_123456.jpg"), None, "month 13");
        assert_eq!(re.match_name("IMG_18000915_123456.jpg"), None, "year range");
        assert_eq!(re.match_name("IMG_1234.jpg"), None);
    }

    #[test]
    fn filename_regex_custom() {
        let re = FilenameRegex::new(r"(?P<day>\d{2})\.(?P<month>\d{2})\.(?P<year>\d{4})").unwrap();
        assert_eq!(
            re.match_name("x 15.09.2016.jpg"),
            Some(dt("2016-09-15 00:00:00"))
        );
        assert!(matches!(
            FilenameRegex::new(r"(?P<year>\d{4})(?P<month>\d{2})"),
            Err(RegexError::MissingGroup("day"))
        ));
        assert!(matches!(
            FilenameRegex::new("("),
            Err(RegexError::Syntax(_))
        ));
    }

    fn stamp(s: &str) -> Option<ExifStamp> {
        Some(ExifStamp {
            wall: dt(s),
            subsec: None,
        })
    }

    fn opts<'a>(fields: &'a [DateField], re: &'a FilenameRegex, zone: Zone) -> ResolveOptions<'a> {
        ResolveOptions {
            date_fields: fields,
            regex: re,
            timestamp: false,
            zone,
        }
    }

    fn plus(h: i32) -> Zone {
        Zone::Fixed(FixedOffset::east_opt(h * 3600).unwrap())
    }

    fn media(s: &str) -> Option<DateTime<FixedOffset>> {
        Some(DateTime::parse_from_rfc3339(s).unwrap())
    }

    #[test]
    fn resolve_field_order() {
        let re = FilenameRegex::default();
        let raw = RawMeta {
            exif: [
                stamp("2017-01-02 03:04:05"),
                stamp("2017-01-02 03:04:06"),
                None,
            ],
            ..RawMeta::default()
        };
        let all = DateField::DEFAULT;
        let got = resolve(&raw, Kind::Image, "x.jpg", None, &opts(&all, &re, plus(0))).unwrap();
        assert_eq!(
            (got.wall, got.source),
            (dt("2017-01-02 03:04:05"), DateSource::ExifDateTimeOriginal)
        );

        let cd_first = [DateField::CreateDate, DateField::DateTimeOriginal];
        let got = resolve(
            &raw,
            Kind::Image,
            "x.jpg",
            None,
            &opts(&cd_first, &re, plus(0)),
        )
        .unwrap();
        assert_eq!(got.source, DateSource::ExifCreateDate);

        let modify_only = [DateField::ModifyDate];
        assert_eq!(
            resolve(
                &raw,
                Kind::Image,
                "x.jpg",
                None,
                &opts(&modify_only, &re, plus(0))
            ),
            None
        );
    }

    #[test]
    fn resolve_video_zones() {
        let re = FilenameRegex::default();
        let all = DateField::DEFAULT;
        // Apple key keeps its own wall clock whatever --tz says.
        let apple = RawMeta {
            media_create: media("2021-06-07T08:09:10+02:00"),
            ..RawMeta::default()
        };
        let got = resolve(
            &apple,
            Kind::Video,
            "a.mov",
            None,
            &opts(&all, &re, plus(-7)),
        )
        .unwrap();
        assert_eq!(
            (got.wall, got.source),
            (dt("2021-06-07 08:09:10"), DateSource::VideoLocal)
        );

        // mvhd UTC is converted with the zone.
        let mvhd = RawMeta {
            media_create: media("2021-06-07T06:09:10+00:00"),
            ..RawMeta::default()
        };
        let got = resolve(&mvhd, Kind::Video, "a.mp4", None, &opts(&all, &re, plus(2))).unwrap();
        assert_eq!(
            (got.wall, got.source),
            (dt("2021-06-07 08:09:10"), DateSource::VideoUtc)
        );

        // mvhd = 0 decodes to 1904: no date.
        let zero = RawMeta {
            media_create: media("1904-01-01T00:00:00+00:00"),
            ..RawMeta::default()
        };
        assert_eq!(
            resolve(&zero, Kind::Video, "a.mp4", None, &opts(&all, &re, plus(0))),
            None
        );
    }

    #[test]
    fn resolve_fallbacks() {
        let re = FilenameRegex::default();
        let all = DateField::DEFAULT;
        let empty = RawMeta::default();
        let name = "IMG_20160915_123456.jpg";
        let got = resolve(&empty, Kind::Image, name, None, &opts(&all, &re, plus(0))).unwrap();
        assert_eq!(got.source, DateSource::Filename);
        // Non-media never uses the filename or mtime.
        assert_eq!(
            resolve(&empty, Kind::Other, name, None, &opts(&all, &re, plus(0))),
            None
        );

        let mtime = DateTime::parse_from_rfc3339("2015-01-02T03:04:05Z")
            .unwrap()
            .to_utc();
        let mut o = opts(&all, &re, plus(1));
        assert_eq!(resolve(&empty, Kind::Image, "x.jpg", Some(mtime), &o), None);
        o.timestamp = true;
        let got = resolve(&empty, Kind::Image, "x.jpg", Some(mtime), &o).unwrap();
        assert_eq!(
            (got.wall, got.source),
            (dt("2015-01-02 04:04:05"), DateSource::Mtime)
        );
    }
}
