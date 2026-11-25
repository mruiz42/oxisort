//! `--date-format` (`-d`): a small tokenizer that turns phockup-style formats
//! such as `YYYY/MM/DD` into folder paths.
//!
//! phockup does chained `str.replace`, so literal words get mangled
//! (`YYYY-MM Month` → `2011-07 Julyonth`). Here each maximal run of ASCII
//! letters is either split *entirely* into tokens (greedy, longest first) or
//! kept as a literal, so `Month` stays `Month` while `YYYYMMDD` still works.

use std::path::PathBuf;

use chrono::NaiveDate;

/// Tokens, longest first so the greedy split prefers `YYYY` over `YY`.
const TOKENS: [(&str, &str); 9] = [
    ("YYYY", "%Y"),
    ("DDD", "%j"),
    ("YY", "%y"),
    ("MM", "%m"),
    ("DD", "%d"),
    ("M", "%B"),
    ("m", "%b"),
    ("U", "%U"),
    ("W", "%W"),
];

/// One folder level of the format: a sequence of strftime pieces.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Component {
    /// A chrono strftime string with every literal `%` escaped.
    strftime: String,
}

/// A compiled `--date-format`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirFormat {
    components: Vec<Component>,
}

/// Why a `--date-format` value was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FormatError {
    #[error("date format is empty")]
    Empty,
    #[error("date format must be relative (it starts with a path separator)")]
    Absolute,
    #[error("date format has an empty folder name (doubled or trailing separator)")]
    EmptyComponent,
    #[error("date format must not contain '..'")]
    ParentDir,
    #[error("date format must not contain NUL")]
    Nul,
}

impl DirFormat {
    /// Compiles a format string.
    ///
    /// # Errors
    /// Returns a [`FormatError`] if the format is empty, absolute, contains a
    /// `..` or empty folder name, or a NUL byte.
    pub fn parse(fmt: &str) -> Result<Self, FormatError> {
        if fmt.is_empty() {
            return Err(FormatError::Empty);
        }
        if fmt.contains('\0') {
            return Err(FormatError::Nul);
        }
        if fmt.starts_with(['/', '\\']) {
            return Err(FormatError::Absolute);
        }
        let mut components = Vec::new();
        for raw in fmt.split(['/', '\\']) {
            if raw.is_empty() {
                return Err(FormatError::EmptyComponent);
            }
            if raw == ".." {
                return Err(FormatError::ParentDir);
            }
            components.push(Component {
                strftime: compile_component(raw),
            });
        }
        Ok(DirFormat { components })
    }

    /// Renders the folder path (relative) for a capture date.
    pub fn render(&self, date: NaiveDate) -> PathBuf {
        self.components
            .iter()
            .map(|c| date.format(&c.strftime).to_string())
            .collect()
    }
}

impl Default for DirFormat {
    fn default() -> Self {
        DirFormat::parse("YYYY/MM/DD").expect("default format is valid")
    }
}

/// Compiles one folder name into a strftime string.
fn compile_component(raw: &str) -> String {
    let mut out = String::new();
    let mut rest = raw;
    while let Some(c) = rest.chars().next() {
        if c.is_ascii_alphabetic() {
            let run_len = rest
                .find(|ch: char| !ch.is_ascii_alphabetic())
                .unwrap_or(rest.len());
            let (run, tail) = rest.split_at(run_len);
            match split_tokens(run) {
                Some(spec) => out.push_str(&spec),
                None => out.push_str(run), // letters never contain '%'
            }
            rest = tail;
        } else {
            if c == '%' {
                out.push_str("%%");
            } else {
                out.push(c);
            }
            rest = &rest[c.len_utf8()..];
        }
    }
    out
}

/// Splits a run of letters into tokens greedily (longest token first).
/// Returns `None` if any part of the run is not a token, so the whole run
/// is then treated as literal text.
fn split_tokens(run: &str) -> Option<String> {
    let mut spec = String::new();
    let mut rest = run;
    while !rest.is_empty() {
        let (tok, s) = TOKENS.iter().find(|(tok, _)| rest.starts_with(tok))?;
        spec.push_str(s);
        rest = &rest[tok.len()..];
    }
    Some(spec)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(fmt: &str, y: i32, m: u32, d: u32) -> String {
        let date = NaiveDate::from_ymd_opt(y, m, d).unwrap();
        DirFormat::parse(fmt)
            .unwrap()
            .render(date)
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn every_token() {
        let cases = [
            ("YYYY", "2011"),
            ("YY", "11"),
            ("MM", "07"),
            ("M", "July"),
            ("m", "Jul"),
            ("DDD", "198"),
            ("DD", "17"),
            ("U", "29"),
            ("W", "28"),
        ];
        for (fmt, want) in cases {
            assert_eq!(render(fmt, 2011, 7, 17), want, "format {fmt}");
        }
    }

    #[test]
    fn default_and_compound() {
        assert_eq!(render("YYYY/MM/DD", 2017, 1, 2), "2017/01/02");
        assert_eq!(render("YYYYMMDD", 2017, 1, 2), "20170102");
        assert_eq!(render("YYYY/U", 2011, 7, 17), "2011/29");
        assert_eq!(render("YYYY\\m-DD", 2011, 7, 17), "2011/Jul-17");
    }

    #[test]
    fn words_stay_literal() {
        assert_eq!(render("YYYY-MM Month", 2011, 7, 17), "2011-07 Month");
        assert_eq!(render("YYYY/MM Week", 2011, 7, 17), "2011/07 Week");
    }

    #[test]
    fn percent_is_escaped() {
        assert_eq!(render("YYYY %d 100%", 2011, 7, 17), "2011 %d 100%");
    }

    #[test]
    fn rejects_bad_formats() {
        assert_eq!(DirFormat::parse(""), Err(FormatError::Empty));
        assert_eq!(DirFormat::parse("/YYYY"), Err(FormatError::Absolute));
        assert_eq!(
            DirFormat::parse("YYYY//MM"),
            Err(FormatError::EmptyComponent)
        );
        assert_eq!(DirFormat::parse("YYYY/"), Err(FormatError::EmptyComponent));
        assert_eq!(DirFormat::parse("../YYYY"), Err(FormatError::ParentDir));
        assert_eq!(DirFormat::parse("YYYY/../x"), Err(FormatError::ParentDir));
        assert_eq!(DirFormat::parse("a\0b"), Err(FormatError::Nul));
    }
}
