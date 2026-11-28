//! A minimal TIFF date reader: IFD0 → ExifIFD, the three date tags and
//! their sub-seconds, nothing else.
//!
//! It is the fallback for TIFF-based files (ARW, NEF, DNG, CR2, ORF, RW2,
//! TIFF …) when nom-exif fails (it doesn't know the ORF/RW2 magics at all). nom-exif walks *every* IFD0 entry and gives up on the
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
/// When `r` isn't TIFF-shaped (`II*\0`, `MM\0*`, or the ORF/RW2 variants
/// `IIRO`, `IIRS`, `MMOR`, `IIU\0`) or its IFD0 can't be read. A
/// missing or unreadable ExifIFD or tag just leaves that date `None`.
pub fn read(r: &mut (impl Read + Seek)) -> Result<[Option<ExifStamp>; 3], String> {
    let mut head = [0u8; 8];
    r.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    r.read_exact(&mut head).map_err(|e| e.to_string())?;
    let le = match &head[..4] {
        // Standard TIFF, Olympus ORF (IIRO, IIRS, MMOR) and Panasonic RW2
        // (IIU\0): the same IFD structure behind a vendor magic.
        b"II*\0" | b"IIRO" | b"IIRS" | b"IIU\0" => true,
        b"MM\0*" | b"MMOR" => false,
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

    /// Builds a TIFF-shaped file with the given 4-byte magic and byte order:
    /// IFD0 at `ifd0_at` holding `pad_entries` filler entries and then the
    /// ExifIFD pointer as its *last* entry (the RW2 layout), and an ExifIFD
    /// { DateTimeOriginal, SubSecTimeOriginal "42" inline }.
    fn tiff(magic: &[u8; 4], le: bool, ifd0_at: u32, pad_entries: u16) -> Vec<u8> {
        let u16b = |v: u16| if le { v.to_le_bytes() } else { v.to_be_bytes() };
        let u32b = |v: u32| if le { v.to_le_bytes() } else { v.to_be_bytes() };
        let mut b = magic.to_vec();
        b.extend(u32b(ifd0_at));
        b.resize(ifd0_at as usize, 0);
        let entries = pad_entries + 1;
        let exif_at = ifd0_at + 2 + 12 * u32::from(entries) + 4;
        b.extend(u16b(entries));
        for i in 0..pad_entries {
            // A harmless SHORT tag the reader must skip over.
            b.extend(u16b(0x0001 + i));
            b.extend(u16b(3));
            b.extend(u32b(1));
            b.extend(u32b(0));
        }
        b.extend(u16b(EXIF_IFD));
        b.extend(u16b(LONG));
        b.extend(u32b(1));
        b.extend(u32b(exif_at));
        b.extend(u32b(0));
        assert_eq!(b.len() as u32, exif_at);
        let date_at = exif_at + 2 + 2 * 12 + 4;
        b.extend(u16b(2));
        for (tag, count, value) in [
            (DATE_TIME_ORIGINAL, 20u32, u32b(date_at)),
            (SUBSEC_ORIGINAL, 3, *b"42\0\0"),
        ] {
            b.extend(u16b(tag));
            b.extend(u16b(ASCII));
            b.extend(u32b(count));
            b.extend(value);
        }
        b.extend(u32b(0));
        assert_eq!(b.len() as u32, date_at);
        b.extend(b"2001:02:03 04:05:06\0");
        b
    }

    fn big_endian() -> Vec<u8> {
        tiff(b"MM\0*", false, 8, 0)
    }

    fn assert_dto(bytes: Vec<u8>) {
        let got = read(&mut Cursor::new(bytes)).unwrap();
        let dto = got[0].as_ref().unwrap();
        assert_eq!(dto.wall.to_string(), "2001-02-03 04:05:06");
        assert_eq!(dto.subsec.as_deref(), Some("42"));
    }

    #[test]
    fn reads_raw_variant_magics() {
        // Olympus ORF (IIRO, IIRS), and the big-endian MMOR variant.
        assert_dto(tiff(b"IIRO", true, 8, 0));
        assert_dto(tiff(b"IIRS", true, 8, 0));
        assert_dto(tiff(b"MMOR", false, 8, 0));
        // Panasonic RW2: IFD0 at 24, ExifIFD pointer as the last entry.
        assert_dto(tiff(b"IIU\0", true, 24, 5));
        assert_dto(tiff(b"II*\0", true, 8, 3));
    }

    #[test]
    fn rejects_unknown_magic() {
        assert!(read(&mut Cursor::new(tiff(b"IIXX", true, 8, 0))).is_err());
        assert!(read(&mut Cursor::new(tiff(b"MMXX", false, 8, 0))).is_err());
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
