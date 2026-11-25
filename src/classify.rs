//! Classifies a file as image, video or other: content sniffing first
//! (`infer`), then a case-insensitive extension map.
//!
//! `infer` has no matchers for most RAW formats or 3GP (investigation
//! Finding 2), so the extension map is what classifies ARW/NEF/ORF/RW2/3GP;
//! content wins when it is recognized, so a mislabelled file is still right.

use crate::model::Kind;

/// How many header bytes [`classify`] wants (enough for every `infer`
/// matcher we rely on).
pub const HEADER_LEN: usize = 64;

const IMAGE_EXTS: &[&str] = &[
    "jpg", "jpeg", "jpe", "heic", "heif", "hif", "avif", "png", "webp", "tif", "tiff", "gif",
    "bmp", "psd", "dng", "cr2", "cr3", "crw", "nef", "nrw", "arw", "srf", "sr2", "raf", "orf",
    "rw2", "pef", "rwl", "3fr", "iiq", "x3f",
];

const VIDEO_EXTS: &[&str] = &[
    "mov", "mp4", "m4v", "3gp", "3g2", "mkv", "webm", "avi", "mpg", "mpeg", "mts", "m2ts", "wmv",
];

/// The result of classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Class {
    pub kind: Kind,
    /// MIME type from content sniffing, when `infer` recognized the header.
    pub mime: Option<&'static str>,
}

/// Classifies a file from its first bytes and its extension.
pub fn classify(header: &[u8], ext: Option<&str>) -> Class {
    let mime = infer::get(header).map(|t| t.mime_type());
    let by_content = mime.and_then(|m| {
        if m.starts_with("image/") {
            Some(Kind::Image)
        } else if m.starts_with("video/") {
            Some(Kind::Video)
        } else {
            None
        }
    });
    let kind = by_content.unwrap_or_else(|| kind_from_ext(ext));
    Class { kind, mime }
}

/// Extension-only classification (case-insensitive).
pub fn kind_from_ext(ext: Option<&str>) -> Kind {
    let Some(ext) = ext else { return Kind::Other };
    let lower = ext.to_ascii_lowercase();
    if IMAGE_EXTS.contains(&lower.as_str()) {
        Kind::Image
    } else if VIDEO_EXTS.contains(&lower.as_str()) {
        Kind::Video
    } else {
        Kind::Other
    }
}

/// ORF and RW2 use a non-standard TIFF magic that neither metadata crate
/// reads (decision Q7), so we don't try.
pub fn is_unreadable_raw(ext: Option<&str>) -> bool {
    ext.is_some_and(|e| e.eq_ignore_ascii_case("orf") || e.eq_ignore_ascii_case("rw2"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10, b'J', b'F', b'I', b'F', 0];
    const TIFF: &[u8] = &[b'I', b'I', 0x2A, 0, 8, 0, 0, 0];

    #[test]
    fn content_wins_over_extension() {
        assert_eq!(classify(JPEG, Some("txt")).kind, Kind::Image);
        assert_eq!(classify(JPEG, None).mime, Some("image/jpeg"));
    }

    #[test]
    fn raw_and_video_by_extension() {
        // TIFF-based RAW: infer says image/tiff, which is already an image.
        assert_eq!(classify(TIFF, Some("ARW")).kind, Kind::Image);
        let unknown = b"IIRO\x08\x00\x00\x00";
        assert_eq!(classify(unknown, Some("orf")).kind, Kind::Image);
        assert_eq!(classify(unknown, Some("3GP")).kind, Kind::Video);
        assert_eq!(classify(unknown, Some("txt")).kind, Kind::Other);
        assert_eq!(classify(unknown, None).kind, Kind::Other);
    }

    #[test]
    fn unreadable_raw() {
        assert!(is_unreadable_raw(Some("ORF")));
        assert!(is_unreadable_raw(Some("rw2")));
        assert!(!is_unreadable_raw(Some("arw")));
        assert!(!is_unreadable_raw(None));
    }
}
