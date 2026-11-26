//! Global duplicate detection (plan amendment A1).
//!
//! A file is a duplicate when another file has the same inode, or the same
//! size and full blake3 hash. "Another file" is any regular file anywhere
//! under OUT ([`OutIndex`]) or another input. Names, dates and chain
//! positions play no part, which is what makes detection independent of
//! how the name chain happens to be filled (validation finding F1).
//!
//! This module holds the I/O-light pieces (the OUT walk, size grouping) and
//! the pure ones (content keys, keeper ordering); `plan::resolve` applies
//! the keeper rules in input order.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use rayon::prelude::*;
use walkdir::WalkDir;

use crate::exec::Stop;
use crate::hash::{self, DevIno, Rep};
use crate::scan::{is_junk, is_skipped_dir};

/// How often the OUT walk checks for a stop (entries).
const STOP_CHECK_EVERY: usize = 1_000;

/// A regular file found under OUT.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutFile {
    pub path: PathBuf,
    pub size: u64,
    pub dev_ino: DevIno,
    /// Re-checked before `--delete-duplicates` deletes an input copy of it.
    pub mtime: Option<SystemTime>,
}

/// Every existing output file that can hold an input's content, plus the
/// inodes OUT symlinks resolve to (they are never content copies, but an
/// input they point at is already in OUT: decision A1-Q2).
#[derive(Debug, Default, Clone)]
pub struct OutIndex {
    files: Vec<OutFile>,
    links: Vec<(PathBuf, DevIno)>,
}

/// What the OUT walk leaves out.
#[derive(Debug, Clone, Copy)]
pub struct WalkSkip<'a> {
    /// Folders skipped by identity, so case or Unicode normalisation can't
    /// defeat the check: the duplicates folder, and INPUT when it lies
    /// inside OUT (its files are inputs, handled as candidates).
    pub dirs: &'a [DevIno],
    /// The `--log` file.
    pub file: Option<&'a Path>,
}

impl OutIndex {
    /// Walks OUT (sorted, symlinks not followed). The walk is serial; the
    /// lstat of each kept entry runs in parallel on `pool` (metadata only).
    ///
    /// A missing OUT is empty. An unreadable subfolder is logged once and
    /// left out: a missed keeper costs an extra copy, never a false
    /// duplicate. A stop returns an empty index; the caller then discards
    /// the plan.
    pub fn build(root: &Path, skip: WalkSkip<'_>, pool: &rayon::ThreadPool, stop: &Stop) -> Self {
        if fs::symlink_metadata(root).is_err() {
            return OutIndex::default();
        }
        let mut found: Vec<(PathBuf, bool)> = Vec::new();
        let mut it = WalkDir::new(root)
            .follow_links(false)
            .sort_by_file_name()
            .into_iter();
        let mut seen = 0usize;
        while let Some(next) = it.next() {
            seen += 1;
            if seen % STOP_CHECK_EVERY == 0 && stop.reason().is_some() {
                return OutIndex::default();
            }
            let entry = match next {
                Ok(e) => e,
                Err(err) => {
                    let path = err.path().unwrap_or(root).display().to_string();
                    log::warn!("{path}: not checked for duplicates: {err}");
                    continue;
                }
            };
            let ft = entry.file_type();
            if ft.is_dir() {
                let skipped = (entry.depth() > 0 && is_skipped_dir(entry.file_name()))
                    || entry
                        .metadata()
                        .is_ok_and(|m| skip.dirs.contains(&(m.dev(), m.ino())));
                if skipped {
                    it.skip_current_dir();
                }
                continue;
            }
            if is_junk(entry.file_name()) || skip.file == Some(entry.path()) {
                continue;
            }
            if ft.is_file() || ft.is_symlink() {
                found.push((entry.into_path(), ft.is_symlink()));
            }
        }
        let stated: Vec<Stated> = pool.install(|| {
            found
                .into_par_iter()
                .filter_map(|(path, link)| stat_out(path, link))
                .collect()
        });
        let mut index = OutIndex::default();
        for s in stated {
            match s {
                Stated::File(f) => index.files.push(f),
                Stated::Link(p, id) => index.links.push((p, id)),
            }
        }
        index
    }

    /// Adds a regular file with no known mtime (tests).
    pub fn insert_file(&mut self, path: impl Into<PathBuf>, size: u64, dev_ino: DevIno) {
        self.files.push(OutFile {
            path: path.into(),
            size,
            dev_ino,
            mtime: None,
        });
    }

    /// Adds a symlink resolving to `target` (tests and the walk).
    pub fn insert_link(&mut self, path: impl Into<PathBuf>, target: DevIno) {
        self.links.push((path.into(), target));
    }

    pub fn files(&self) -> &[OutFile] {
        &self.files
    }

    /// Inode → the first OUT path (file or symlink) holding it. An input
    /// with one of these inodes is already in OUT (decision A1-Q2).
    pub fn holders(&self) -> HashMap<DevIno, &Path> {
        let mut m: HashMap<DevIno, &Path> = HashMap::new();
        let all = self
            .files
            .iter()
            .map(|f| (f.dev_ino, f.path.as_path()))
            .chain(self.links.iter().map(|(p, id)| (*id, p.as_path())));
        for (id, p) in all {
            m.entry(id)
                .and_modify(|cur| {
                    if p < *cur {
                        *cur = p;
                    }
                })
                .or_insert(p);
        }
        m
    }
}

enum Stated {
    File(OutFile),
    Link(PathBuf, DevIno),
}

fn stat_out(path: PathBuf, link: bool) -> Option<Stated> {
    let meta = if link {
        fs::metadata(&path)
    } else {
        fs::symlink_metadata(&path)
    };
    match meta {
        Ok(m) if m.is_file() && link => Some(Stated::Link(path, (m.dev(), m.ino()))),
        Ok(m) if m.is_file() => Some(Stated::File(OutFile {
            size: m.len(),
            dev_ino: (m.dev(), m.ino()),
            mtime: m.modified().ok(),
            path,
        })),
        Ok(_) => None,
        Err(e) => {
            log::debug!("{}: not checked for duplicates: {e}", path.display());
            None
        }
    }
}

/// A file taking part in size grouping.
#[derive(Debug, Clone, Copy)]
pub struct Member<'a> {
    pub path: &'a Path,
    pub size: u64,
    pub dev_ino: DevIno,
    pub candidate: bool,
}

/// The size groups worth hashing, as distinct inodes with one
/// representative path each (the first member with that inode, so inputs,
/// listed first, are read rather than their hard links in OUT).
///
/// Empty files never take part (they are never duplicates). A group
/// qualifies if it holds an input and at least two distinct inodes; any
/// other group can't contain a duplicate of an input. Sorted by size, so
/// the result is deterministic.
pub fn size_groups(members: &[Member<'_>]) -> Vec<Vec<Rep>> {
    let mut by_size: HashMap<u64, Vec<Rep>> = HashMap::new();
    let mut slot: HashMap<DevIno, (u64, usize)> = HashMap::new();
    for m in members.iter().filter(|m| m.size > 0) {
        if let Some(&(size, i)) = slot.get(&m.dev_ino) {
            if let Some(r) = by_size.get_mut(&size).and_then(|g| g.get_mut(i)) {
                r.candidate |= m.candidate;
            }
            continue;
        }
        let group = by_size.entry(m.size).or_default();
        slot.insert(m.dev_ino, (m.size, group.len()));
        group.push(Rep {
            path: m.path.to_path_buf(),
            size: m.size,
            dev_ino: m.dev_ino,
            candidate: m.candidate,
        });
    }
    let mut groups: Vec<Vec<Rep>> = by_size
        .into_values()
        .filter(|g| hash::qualifies(g))
        .collect();
    groups.sort_by_key(|g| g[0].size);
    groups
}

/// Hashes what [`size_groups`] selects, on the io pool, and logs the cost.
pub fn hash_contents(
    members: &[Member<'_>],
    pool: &rayon::ThreadPool,
    stop: &Stop,
) -> HashMap<DevIno, blake3::Hash> {
    let (hashes, s) = hash::hash_groups(size_groups(members), pool, stop);
    log::debug!(
        "dedupe: {} size groups, {} partial, {} full hashes, {:.1} MiB read",
        s.size_groups,
        s.partial,
        s.full,
        s.bytes as f64 / 1_048_576.0
    );
    hashes
}

/// What makes two files "the same" for duplicate detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContentKey {
    /// Hashed in full: equal size and hash.
    Hash(u64, blake3::Hash),
    /// Not hashed (unique size, unreadable …): only its own hard links.
    Inode(DevIno),
}

/// The key of a file: its full hash if it has one, else its inode. Hard
/// links share an inode and therefore a hash, so they always share a key.
pub fn content_key(
    size: u64,
    dev_ino: DevIno,
    hashes: &HashMap<DevIno, blake3::Hash>,
) -> ContentKey {
    match hashes.get(&dev_ino) {
        Some(h) => ContentKey::Hash(size, *h),
        None => ContentKey::Inode(dev_ino),
    }
}

/// Total order for choosing among existing or in-place keepers:
/// `(folder, chain position n, name)`. The position puts `x.jpg` before
/// `x-2.jpg`, which plain path order would not (`-` sorts before `.`).
pub fn order_key(path: &Path) -> (PathBuf, u32, OsString) {
    let folder = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let name = path.file_name().unwrap_or_default().to_owned();
    (folder, chain_position(path), name)
}

/// `n` of a name written as `base-<n>.ext` by the namer (n ≥ 2, no leading
/// zero), else 1.
fn chain_position(path: &Path) -> u32 {
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    stem.rsplit_once('-')
        .and_then(|(base, digits)| {
            let n: u32 = digits.parse().ok()?;
            (!base.is_empty() && n >= 2 && n.to_string() == digits).then_some(n)
        })
        .unwrap_or(1)
}

/// Rule 1 keepers: per hashed class, the first existing output file by
/// [`order_key`]. Unhashed OUT files are left out: they can only match
/// their own hard links, which are in place anyway (A1-Q2).
pub fn existing_keepers<'a>(
    out: &'a OutIndex,
    hashes: &HashMap<DevIno, blake3::Hash>,
) -> HashMap<ContentKey, &'a OutFile> {
    let mut best: HashMap<ContentKey, &OutFile> = HashMap::new();
    for f in out.files.iter().filter(|f| f.size > 0) {
        let key = content_key(f.size, f.dev_ino, hashes);
        if matches!(key, ContentKey::Inode(_)) {
            continue;
        }
        best.entry(key)
            .and_modify(|cur| {
                if order_key(&f.path) < order_key(&cur.path) {
                    *cur = f;
                }
            })
            .or_insert(f);
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool() -> rayon::ThreadPool {
        rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .unwrap()
    }

    fn id(p: &Path) -> DevIno {
        let m = fs::metadata(p).unwrap();
        (m.dev(), m.ino())
    }

    /// U16: what the OUT walk leaves out.
    #[test]
    fn out_walk_exclusions() {
        let d = tempfile::tempdir().unwrap();
        let out = fs::canonicalize(d.path()).unwrap();
        let w = |rel: &str| {
            let p = out.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, rel).unwrap();
        };
        for rel in [
            "2017/a.jpg",
            "Duplicates/dup.jpg",
            "import/new.jpg",
            ".Trashes/t.jpg",
            "Lib.photoslibrary/p.jpg",
            ".DS_Store",
            "2017/._a.jpg",
            "run.log",
            "unknown/b.png",
        ] {
            w(rel);
        }
        std::os::unix::fs::symlink(out.join("2017/a.jpg"), out.join("link.jpg")).unwrap();
        // Identity, not name: "duplicates" spelled differently on disk.
        let skip_dirs = [id(&out.join("duplicates")), id(&out.join("import"))];
        let log = out.join("run.log");
        let idx = OutIndex::build(
            &out,
            WalkSkip {
                dirs: &skip_dirs,
                file: Some(&log),
            },
            &pool(),
            &Stop::new(),
        );
        let mut files: Vec<String> = idx
            .files()
            .iter()
            .map(|f| f.path.strip_prefix(&out).unwrap().display().to_string())
            .collect();
        files.sort();
        assert_eq!(files, ["2017/a.jpg", "unknown/b.png"]);
        assert_eq!(idx.links.len(), 1);
        assert_eq!(idx.links[0].1, id(&out.join("2017/a.jpg")));

        // INPUT == OUT: the whole walk is skipped.
        let all = [id(&out)];
        let none = OutIndex::build(
            &out,
            WalkSkip {
                dirs: &all,
                file: None,
            },
            &pool(),
            &Stop::new(),
        );
        assert!(none.files().is_empty());

        // Missing OUT (dry run of a fresh output): empty.
        let fresh = OutIndex::build(
            &out.join("nope"),
            WalkSkip {
                dirs: &[],
                file: None,
            },
            &pool(),
            &Stop::new(),
        );
        assert!(fresh.files().is_empty());
    }

    /// U15 (grouping half): only groups with an input and ≥ 2 inodes, one
    /// path per inode, empty files never.
    #[test]
    fn size_groups_select_only_possible_duplicates() {
        let m = |p: &'static str, size, ino, candidate| Member {
            path: Path::new(p),
            size,
            dev_ino: (1, ino),
            candidate,
        };
        let members = [
            m("/in/a", 5, 1, true),
            m("/in/a-link", 5, 1, true), // hard link: same inode
            m("/out/x", 5, 2, false),
            m("/in/b", 7, 3, true), // unique size
            m("/out/y", 9, 4, false),
            m("/out/z", 9, 5, false), // no input in this group
            m("/in/e", 0, 6, true),
            m("/out/e", 0, 7, false), // empty files
            m("/in/c", 11, 8, true),
            m("/out/c", 11, 8, false), // same inode only: nothing to hash
        ];
        let groups = size_groups(&members);
        assert_eq!(groups.len(), 1, "{groups:?}");
        let paths: Vec<&Path> = groups[0].iter().map(|r| r.path.as_path()).collect();
        assert_eq!(paths, [Path::new("/in/a"), Path::new("/out/x")]);
    }

    #[test]
    fn order_key_puts_base_before_numbered() {
        assert!(order_key(Path::new("/o/x.jpg")) < order_key(Path::new("/o/x-2.jpg")));
        assert!(order_key(Path::new("/o/x-2.jpg")) < order_key(Path::new("/o/x-10.jpg")));
        assert!(order_key(Path::new("/a/x-3.jpg")) < order_key(Path::new("/b/x.jpg")));
        assert_eq!(chain_position(Path::new("x-02.jpg")), 1);
        assert_eq!(chain_position(Path::new("-2.jpg")), 1);
    }
}
