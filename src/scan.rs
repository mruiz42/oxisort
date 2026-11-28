//! Phase 1: walk the input tree (single thread, sorted, deterministic).
//!
//! Produces the media/other files to process with their XMP sidecars
//! paired, plus per-folder bookkeeping that `--rmdirs` needs. Symlinks are
//! never followed, and system folders, photo-library packages, junk files,
//! the output subtree and the log file are left alone.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

/// Folders that are never entered: OS metadata and NAS thumbnail stores.
const SKIP_DIRS: &[&str] = &[
    ".Trashes",
    ".Spotlight-V100",
    ".fseventsd",
    ".TemporaryItems",
    ".DocumentRevisions-V100",
    "@eaDir",
    "$RECYCLE.BIN",
    "System Volume Information",
];

/// App library packages: moving files out of them would corrupt the library.
const PACKAGE_EXTS: &[&str] = &["photoslibrary", "aplibrary", "lrdata", "lrlibrary"];

/// How a sidecar relates to its media file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidecarForm {
    /// `name.ext.xmp` (darktable style): always follows its own file.
    WithExt,
    /// `name.xmp` (Lightroom style): follows the only file with that stem.
    StemOnly,
}

/// An XMP sidecar attached to a media file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sidecar {
    pub path: PathBuf,
    pub form: SidecarForm,
}

/// One file to process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanEntry {
    pub path: PathBuf,
    pub sidecars: Vec<Sidecar>,
}

/// A folder the scan descended into (used by the `--rmdirs` simulation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirInfo {
    pub path: PathBuf,
    pub depth: usize,
    /// Every direct child seen, of any type (junk included).
    pub children: usize,
    /// Junk files directly inside (`.DS_Store`, `._*` …).
    pub junk: Vec<PathBuf>,
    /// Reading the folder failed, so its contents are unknown.
    pub unreadable: bool,
}

/// Counters for the summary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanCounts {
    pub ignored_junk: usize,
    pub skipped_dirs: usize,
    pub symlinks: usize,
    pub orphan_sidecars: usize,
    pub special_files: usize,
}

/// Everything the scan found.
#[derive(Debug, Default)]
pub struct ScanResult {
    pub entries: Vec<ScanEntry>,
    /// Folders descended into; the input root is first.
    pub dirs: Vec<DirInfo>,
    /// `(path, message)` for unreadable folders/entries.
    pub errors: Vec<(PathBuf, String)>,
    /// `name.xmp` files shared by several media files: left in place.
    pub shared_sidecars: Vec<PathBuf>,
    /// `.oxisort-tmp-*` files from an interrupted run: never sorted, never
    /// deleted (the user removes them), and they keep their folder alive
    /// for `--rmdirs`.
    pub leftovers: Vec<PathBuf>,
    pub counts: ScanCounts,
}

/// Scan settings.
#[derive(Debug, Clone)]
pub struct ScanOptions<'a> {
    pub root: &'a Path,
    /// `Some(0)` = only files directly in `root`.
    pub max_depth: Option<usize>,
    /// Paths (folders or files) to leave out entirely: the output subtree
    /// when it lies inside the input, and the log file.
    pub exclude: &'a [PathBuf],
    /// Folders to leave out by `(dev, ino)`: the duplicates folder when it
    /// lies inside the input (organizing in place), so a differently
    /// spelled path can't sneak its files back in.
    pub exclude_ids: &'a [(u64, u64)],
}

/// `.DS_Store`, `Thumbs.db`, `desktop.ini` and AppleDouble `._*` files.
pub fn is_junk(name: &OsStr) -> bool {
    let name = name.to_string_lossy();
    name == ".DS_Store"
        || name.starts_with("._")
        || name.eq_ignore_ascii_case("Thumbs.db")
        || name.eq_ignore_ascii_case("desktop.ini")
}

pub(crate) fn is_skipped_dir(name: &OsStr) -> bool {
    let name = name.to_string_lossy();
    SKIP_DIRS.contains(&name.as_ref())
        || Path::new(name.as_ref())
            .extension()
            .is_some_and(|e| PACKAGE_EXTS.iter().any(|p| e.eq_ignore_ascii_case(p)))
}

fn is_xmp(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("xmp"))
}

/// Walks the input tree.
pub fn scan(opts: &ScanOptions<'_>) -> ScanResult {
    let mut res = ScanResult::default();
    // Walk depth of a file directly in root is 1.
    let walk_max = opts.max_depth.map(|d| d + 1);
    let mut walker = WalkDir::new(opts.root)
        .follow_links(false)
        .sort_by_file_name();
    if let Some(m) = walk_max {
        walker = walker.max_depth(m);
    }
    let mut dir_index: HashMap<PathBuf, usize> = HashMap::new();
    // Per folder: indices of entries, and its xmp files, for pairing.
    let mut entries_by_dir: HashMap<PathBuf, Vec<usize>> = HashMap::new();
    let mut xmps_by_dir: Vec<(PathBuf, PathBuf)> = Vec::new();

    let mut it = walker.into_iter();
    while let Some(next) = it.next() {
        let entry = match next {
            Ok(e) => e,
            Err(err) => {
                let path = err.path().map(Path::to_path_buf).unwrap_or_default();
                // A failing read_dir is reported against the folder itself.
                if let Some(&i) = dir_index.get(&path) {
                    res.dirs[i].unreadable = true;
                }
                res.errors.push((path, err.to_string()));
                continue;
            }
        };
        let path = entry.path();
        if entry.depth() == 0 {
            dir_index.insert(path.to_path_buf(), res.dirs.len());
            res.dirs.push(new_dir(path, 0));
            continue;
        }
        let parent = path.parent().map(Path::to_path_buf).unwrap_or_default();
        let parent_idx = dir_index.get(&parent).copied();
        if let Some(i) = parent_idx {
            res.dirs[i].children += 1;
        }
        let ft = entry.file_type();
        if ft.is_symlink() {
            log::debug!("skip {} (symlink)", path.display());
            res.counts.symlinks += 1;
            continue;
        }
        if opts.exclude.iter().any(|x| x == path) {
            if ft.is_dir() {
                it.skip_current_dir();
            }
            continue;
        }
        if ft.is_dir() {
            let excluded_id = !opts.exclude_ids.is_empty()
                && entry.metadata().is_ok_and(|m| {
                    use std::os::unix::fs::MetadataExt;
                    opts.exclude_ids.contains(&(m.dev(), m.ino()))
                });
            if excluded_id {
                log::debug!("skip {} (duplicates folder)", path.display());
                it.skip_current_dir();
            } else if is_skipped_dir(entry.file_name()) {
                log::debug!("skip {} (system or library folder)", path.display());
                res.counts.skipped_dirs += 1;
                it.skip_current_dir();
            } else if walk_max != Some(entry.depth()) {
                // Folders at the depth limit aren't descended; they stay
                // opaque (never rmdir candidates).
                dir_index.insert(path.to_path_buf(), res.dirs.len());
                res.dirs.push(new_dir(path, entry.depth()));
            }
            continue;
        }
        if !ft.is_file() {
            res.counts.special_files += 1;
            continue;
        }
        if crate::fsops::is_temp_leftover(entry.file_name()) {
            res.leftovers.push(path.to_path_buf());
            continue;
        }
        if is_junk(entry.file_name()) {
            res.counts.ignored_junk += 1;
            if let Some(i) = parent_idx {
                res.dirs[i].junk.push(path.to_path_buf());
            }
            continue;
        }
        if is_xmp(path) {
            xmps_by_dir.push((parent, path.to_path_buf()));
            continue;
        }
        entries_by_dir
            .entry(parent)
            .or_default()
            .push(res.entries.len());
        res.entries.push(ScanEntry {
            path: path.to_path_buf(),
            sidecars: Vec::new(),
        });
    }
    pair_sidecars(&mut res, &entries_by_dir, xmps_by_dir);
    res
}

fn new_dir(path: &Path, depth: usize) -> DirInfo {
    DirInfo {
        path: path.to_path_buf(),
        depth,
        children: 0,
        junk: Vec::new(),
        unreadable: false,
    }
}

/// Attaches each `.xmp` to its media file (decision Q2):
/// `name.ext.xmp` → the file `name.ext`; `name.xmp` → the only file with
/// stem `name`. A `name.xmp` matching several files is left in place with a
/// warning; one matching nothing is an orphan. Matching is case-insensitive
/// (APFS default).
fn pair_sidecars(
    res: &mut ScanResult,
    entries_by_dir: &HashMap<PathBuf, Vec<usize>>,
    xmps: Vec<(PathBuf, PathBuf)>,
) {
    for (dir, xmp) in xmps {
        let siblings = entries_by_dir.get(&dir).map(Vec::as_slice).unwrap_or(&[]);
        let xmp_name = xmp
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        // Strip ".xmp" (4 ASCII bytes, any case).
        let base = &xmp_name[..xmp_name.len() - 4];
        let name_of = |i: usize| {
            res.entries[i]
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        };
        let stem_of = |i: usize| {
            res.entries[i]
                .path
                .file_stem()
                .map(|n| n.to_string_lossy().into_owned())
        };

        let with_ext = siblings
            .iter()
            .copied()
            .find(|&i| name_of(i).as_deref() == Some(base))
            .or_else(|| {
                siblings
                    .iter()
                    .copied()
                    .find(|&i| name_of(i).is_some_and(|n| n.eq_ignore_ascii_case(base)))
            });
        let (target, form) = match with_ext {
            Some(i) => (Some(i), SidecarForm::WithExt),
            None => {
                let matches: Vec<usize> = siblings
                    .iter()
                    .copied()
                    .filter(|&i| stem_of(i).is_some_and(|s| s.eq_ignore_ascii_case(base)))
                    .collect();
                match matches.as_slice() {
                    [one] => (Some(*one), SidecarForm::StemOnly),
                    [] => (None, SidecarForm::StemOnly),
                    _ => {
                        log::warn!(
                            "sidecar {} matches {} files; left in place",
                            xmp.display(),
                            matches.len()
                        );
                        res.shared_sidecars.push(xmp);
                        continue;
                    }
                }
            }
        };
        match target {
            Some(i) => res.entries[i].sidecars.push(Sidecar { path: xmp, form }),
            None => {
                log::debug!("skip {} (orphan sidecar)", xmp.display());
                res.counts.orphan_sidecars += 1;
            }
        }
    }
    for e in &mut res.entries {
        e.sidecars.sort_by(|a, b| a.path.cmp(&b.path));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn touch(root: &Path, rel: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, rel.as_bytes()).unwrap();
    }

    fn rels(root: &Path, res: &ScanResult) -> Vec<String> {
        res.entries
            .iter()
            .map(|e| e.path.strip_prefix(root).unwrap().display().to_string())
            .collect()
    }

    fn scan_dir(root: &Path, max_depth: Option<usize>, exclude: &[PathBuf]) -> ScanResult {
        scan(&ScanOptions {
            root,
            max_depth,
            exclude,
            exclude_ids: &[],
        })
    }

    #[test]
    fn sorted_with_ignores() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        for f in [
            "b.jpg",
            "a.jpg",
            "sub/c.jpg",
            ".DS_Store",
            "._a.jpg",
            "Thumbs.db",
            "lib.photoslibrary/x.jpg",
            ".Trashes/y.jpg",
            "out/z.jpg",
        ] {
            touch(r, f);
        }
        std::os::unix::fs::symlink(r.join("a.jpg"), r.join("link.jpg")).unwrap();
        let res = scan_dir(r, None, &[r.join("out")]);
        assert_eq!(rels(r, &res), ["a.jpg", "b.jpg", "sub/c.jpg"]);
        assert_eq!(res.counts.ignored_junk, 3);
        assert_eq!(res.counts.skipped_dirs, 2);
        assert_eq!(res.counts.symlinks, 1);
        assert_eq!(res.dirs[0].junk.len(), 3);
    }

    #[test]
    fn max_depth() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        for f in ["a.jpg", "s/b.jpg", "s/t/c.jpg"] {
            touch(r, f);
        }
        assert_eq!(rels(r, &scan_dir(r, Some(0), &[])), ["a.jpg"]);
        assert_eq!(rels(r, &scan_dir(r, Some(1), &[])), ["a.jpg", "s/b.jpg"]);
        // Folder at the depth limit is not a DirInfo (it's opaque).
        assert_eq!(scan_dir(r, Some(0), &[]).dirs.len(), 1);
    }

    #[test]
    fn sidecar_pairing() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        for f in [
            "IMG_1.ARW",
            "IMG_1.ARW.xmp",
            "IMG_2.JPG",
            "IMG_2.XMP",
            "IMG_3.ARW",
            "IMG_3.JPG",
            "IMG_3.xmp",
            "lonely.xmp",
        ] {
            touch(r, f);
        }
        let res = scan_dir(r, None, &[]);
        let side = |name: &str| {
            let e = res.entries.iter().find(|e| e.path.ends_with(name)).unwrap();
            e.sidecars
                .iter()
                .map(|s| {
                    (
                        s.path.file_name().unwrap().to_string_lossy().into_owned(),
                        s.form,
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            side("IMG_1.ARW"),
            [("IMG_1.ARW.xmp".to_owned(), SidecarForm::WithExt)]
        );
        assert_eq!(
            side("IMG_2.JPG"),
            [("IMG_2.XMP".to_owned(), SidecarForm::StemOnly)]
        );
        assert!(side("IMG_3.ARW").is_empty() && side("IMG_3.JPG").is_empty());
        assert_eq!(res.shared_sidecars, [r.join("IMG_3.xmp")]);
        assert_eq!(res.counts.orphan_sidecars, 1);
    }
}
