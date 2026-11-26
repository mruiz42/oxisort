//! A minimal TIFF date reader: IFD0 → ExifIFD, the three date tags and
//! their sub-seconds, nothing else.
//!
//! It is the fallback for TIFF-based files (ARW, NEF, DNG, CR2, TIFF …) when
//! nom-exif fails. nom-exif walks *every* IFD0 entry and gives up on the
//! whole file as soon as one entry's data lies past what it has buffered
//! (seen on the Sony A7C II, whose IFD0 points at a large block far into the
//! file). This reader follows only the pointers it needs and reads a few
//! hundred bytes by seeking, so a 70 MB RAW costs the same as a small one.
//! kamadak-exif is not used here because it reads TIFF/RAW to EOF.

use std::io::{Read, Seek, SeekFrom};

use crate::date::{normalize_subsec, parse_datestring};
use crate::metadata::ExifStamp;

const MODIFY_DATE: u16 = 0x0132;
const EXIF_IFD: u16 = 0x8769;
const DATE_TIME_ORIGINAL: u16 = 0x9003;
const CREATE_DATE: u16 = 0x9004;
const SUBSEC_TIME: u16 = 0x9290;
const SUBSEC_ORIGINAL: u16 = 0x9291;
const SUBSEC_DIGITIZED: u16 = 0x9292;

const ASCII: u16 = 2;
const LONG: u16 = 4;
/// TIFF type 13: an IFD offset (some writers use it for ExifIFD).
const IFD: u16 = 13;

/// Bounds a corrupt entry count (real IFDs hold a few dozen to ~200).
const MAX_ENTRIES: usize = 1024;
/// Longest date or sub-second string accepted; real ones are ≤ 20 bytes.
const MAX_ASCII: u32 = 64;

/// Reads `[DateTimeOriginal, CreateDate, ModifyDate]` (with sub-seconds),
/// like the other readers in `metadata`.
///
/// # Errors
/// When `r` isn't TIFF (`II*\0` / `MM\0*`) or its IFD0 can't be read. A
/// missing or unreadable ExifIFD or tag just leaves that date `None`.
pub fn read(r: &mut (impl Read + Seek)) -> Result<[Option<ExifStamp>; 3], String> {
    let mut head = [0u8; 8];
    r.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    r.read_exact(&mut head).map_err(|e| e.to_string())?;
    let le = match &head[..4] {
        b"II*\0" => true,
        b"MM\0*" => false,
        _ => return Err("not a TIFF header".to_owned()),
    };
    let t = Tiff { le };
    let ifd0 = t.ifd(r, t.u32(&head[4..8]))?;
    let exif = ifd0
        .iter()
        .find(|e| e.tag == EXIF_IFD && matches!(e.typ, LONG | IFD))
        .and_then(|e| t.ifd(r, e.value).ok())
        .unwrap_or_default();
    let text = |entries: &[Entry], tag: u16, r: &mut _| {
        entries
            .iter()
            .find(|e| e.tag == tag)
            .and_then(|e| t.ascii(r, e))
    };
    let mut stamp = |entries: &[Entry], date: u16, sub: u16| {
        let (wall, frac) = parse_datestring(&text(entries, date, r)?)?;
        let subsec = text(&exif, sub, r)
            .as_deref()
            .and_then(normalize_subsec)
            .or(frac);
        Some(ExifStamp { wall, subsec })
    };
    Ok([
        stamp(&exif, DATE_TIME_ORIGINAL, SUBSEC_ORIGINAL),
        stamp(&exif, CREATE_DATE, SUBSEC_DIGITIZED),
        stamp(&ifd0, MODIFY_DATE, SUBSEC_TIME),
    ])
}

/// One raw 12-byte IFD entry.
#[derive(Debug, Clone, Copy)]
struct Entry {
    tag: u16,
    typ: u16,
    count: u32,
    /// The inline value bytes, or the data offset when they don't fit.
    raw: [u8; 4],
    value: u32,
}

struct Tiff {
    le: bool,
}

impl Tiff {
    fn u16(&self, b: &[u8]) -> u16 {
        let b = [b[0], b[1]];
        if self.le {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        }
    }

    fn u32(&self, b: &[u8]) -> u32 {
        let b = [b[0], b[1], b[2], b[3]];
        if self.le {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        }
    }

    fn ifd(&self, r: &mut (impl Read + Seek), at: u32) -> Result<Vec<Entry>, String> {
        let mut n = [0u8; 2];
        r.seek(SeekFrom::Start(at.into()))
            .and_then(|_| r.read_exact(&mut n))
            .map_err(|e| format!("IFD at {at}: {e}"))?;
        let n = usize::from(self.u16(&n));
        if n > MAX_ENTRIES {
            return Err(format!("IFD at {at}: {n} entries"));
        }
        let mut buf = vec![0u8; n * 12];
        r.read_exact(&mut buf)
            .map_err(|e| format!("IFD at {at}: {e}"))?;
        Ok(buf
            .chunks_exact(12)
            .map(|c| Entry {
                tag: self.u16(&c[0..2]),
                typ: self.u16(&c[2..4]),
                count: self.u32(&c[4..8]),
                raw: [c[8], c[9], c[10], c[11]],
                value: self.u32(&c[8..12]),
            })
            .collect())
    }

    /// An ASCII entry's text up to the first NUL; `None` if it isn't ASCII,
    /// is implausibly long, or can't be read.
    fn ascii(&self, r: &mut (impl Read + Seek), e: &Entry) -> Option<String> {
        if e.typ != ASCII || e.count == 0 || e.count > MAX_ASCII {
            return None;
        }
        let bytes = if e.count <= 4 {
            e.raw[..e.count as usize].to_vec()
        } else {
            let mut b = vec![0u8; e.count as usize];
            r.seek(SeekFrom::Start(e.value.into())).ok()?;
            r.read_exact(&mut b).ok()?;
            b
        };
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        Some(String::from_utf8_lossy(&bytes[..end]).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    /// A big-endian TIFF: IFD0 { ExifIFD } → ExifIFD { DateTimeOriginal,
    /// SubSecTimeOriginal "42" inline }.
    fn big_endian() -> Vec<u8> {
        let mut b = b"MM\0*".to_vec();
        b.extend(8u32.to_be_bytes());
        // IFD0 @8: 1 entry, next = 0.
        b.extend(1u16.to_be_bytes());
        b.extend(EXIF_IFD.to_be_bytes());
        b.extend(LONG.to_be_bytes());
        b.extend(1u32.to_be_bytes());
        b.extend(26u32.to_be_bytes());
        b.extend(0u32.to_be_bytes());
        // ExifIFD @26: 2 entries, next = 0, then the date at 56.
        b.extend(2u16.to_be_bytes());
        for (tag, count, value) in [
            (DATE_TIME_ORIGINAL, 20u32, 56u32.to_be_bytes()),
            (SUBSEC_ORIGINAL, 3, *b"42\0\0"),
        ] {
            b.extend(tag.to_be_bytes());
            b.extend(ASCII.to_be_bytes());
            b.extend(count.to_be_bytes());
            b.extend(value);
        }
        b.extend(0u32.to_be_bytes());
        assert_eq!(b.len(), 56);
        b.extend(b"2001:02:03 04:05:06\0");
        b
    }

    #[test]
    fn reads_big_endian_dates_and_inline_subsec() {
        let got = read(&mut Cursor::new(big_endian())).unwrap();
        let dto = got[0].as_ref().unwrap();
        assert_eq!(dto.wall.to_string(), "2001-02-03 04:05:06");
        assert_eq!(dto.subsec.as_deref(), Some("42"));
        assert!(got[1].is_none() && got[2].is_none());
    }

    #[test]
    fn rejects_non_tiff_and_truncated_files() {
        assert!(read(&mut Cursor::new(b"\xff\xd8\xff\xe0 jpeg".to_vec())).is_err());
        let mut cut = big_endian();
        cut.truncate(20);
        assert!(read(&mut Cursor::new(cut)).is_err(), "IFD0 cut short");
        // ExifIFD pointer past EOF: no dates, but not an error.
        let mut far = big_endian();
        far[18..22].copy_from_slice(&9_999u32.to_be_bytes());
        assert_eq!(read(&mut Cursor::new(far)).unwrap(), [None, None, None]);
    }
}
