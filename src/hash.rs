//! Content hashing (blake3) for duplicate detection, size first.
//!
//! [`two_stage`] only reads files whose size collides with another file
//! (grouping is done by `dedupe::size_groups`). Large files first get a
//! cheap partial hash (size, first and last 64 KiB); only those whose
//! partial hash still collides are read in full. Equality is decided by the
//! full hash alone, so the partial stage only saves reads, it never decides
//! that two files are the same.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rayon::prelude::*;

use crate::exec::Stop;

/// Read size per call: large enough for sequential throughput on RAW files.
const BUF_LEN: usize = 1 << 20;

/// Bytes read from each end of a file for the partial hash.
pub const PARTIAL_EDGE: u64 = 64 * 1024;

/// Files up to this size skip the partial stage: the partial hash would
/// read the whole file anyway.
pub const PARTIAL_MIN: u64 = 2 * PARTIAL_EDGE;

/// `(st_dev, st_ino)`: hashes are per inode, so hard links share one read.
pub type DevIno = (u64, u64);

/// One inode to hash, through one representative path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rep {
    pub path: PathBuf,
    pub size: u64,
    pub dev_ino: DevIno,
    /// Some input file has this inode (only groups with an input matter).
    pub candidate: bool,
}

/// What the hashing cost (logged at `-v`).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct HashStats {
    pub size_groups: usize,
    pub partial: usize,
    pub full: usize,
    pub bytes: u64,
}

/// True if a group of distinct inodes can hold a duplicate of an input.
pub fn qualifies(group: &[Rep]) -> bool {
    group.len() >= 2 && group.iter().any(|r| r.candidate)
}

/// Hashes one file, reusing `buf` (grown to [`BUF_LEN`] on first use).
///
/// # Errors
/// Any I/O error opening or reading the file.
pub fn hash_file(path: &Path, buf: &mut Vec<u8>) -> io::Result<blake3::Hash> {
    buf.resize(BUF_LEN, 0);
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    loop {
        let n = match file.read(buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize())
}

/// blake3 of `size ‖ first 64 KiB ‖ last 64 KiB`, for files larger than
/// [`PARTIAL_MIN`]. The size is mixed in so the hash stands on its own.
///
/// # Errors
/// Any I/O error, including a file that shrank below `size` since it was
/// stat'ed (`read_exact` fails): it is then never treated as a duplicate.
pub fn partial_hash(path: &Path, size: u64, buf: &mut Vec<u8>) -> io::Result<blake3::Hash> {
    let edge = PARTIAL_EDGE as usize;
    buf.resize(edge, 0);
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(&size.to_le_bytes());
    file.read_exact(&mut buf[..edge])?;
    hasher.update(&buf[..edge]);
    file.seek(SeekFrom::Start(size.saturating_sub(PARTIAL_EDGE)))?;
    file.read_exact(&mut buf[..edge])?;
    hasher.update(&buf[..edge]);
    Ok(hasher.finalize())
}

/// Runs both stages over qualifying size groups on `pool` and returns the
/// full hash of every inode that might duplicate an input.
///
/// `partial` and `full` do the reads (injected so the staging logic is
/// testable without I/O); `None` means "unreadable", which leaves the inode
/// out of the map, so it compares as different: never a false duplicate.
/// `stop` is checked before each read; once set, the remaining files are
/// left out, the map is incomplete, and the caller must not plan from it.
pub fn two_stage<P, F>(
    groups: Vec<Vec<Rep>>,
    partial: P,
    full: F,
    pool: &rayon::ThreadPool,
    stop: &Stop,
) -> (HashMap<DevIno, blake3::Hash>, HashStats)
where
    P: Fn(&Rep, &mut Vec<u8>) -> Option<blake3::Hash> + Sync,
    F: Fn(&Rep, &mut Vec<u8>) -> Option<blake3::Hash> + Sync,
{
    let bytes = AtomicU64::new(0);
    let mut stats = HashStats {
        size_groups: groups.len(),
        ..HashStats::default()
    };
    let (large, mut to_full): (Vec<Vec<Rep>>, Vec<Vec<Rep>>) = groups
        .into_iter()
        .partition(|g| g.first().is_some_and(|r| r.size > PARTIAL_MIN));

    let firsts: Vec<Rep> = large.into_iter().flatten().collect();
    stats.partial = firsts.len();
    let partials: Vec<(Rep, Option<blake3::Hash>)> = pool.install(|| {
        firsts
            .into_par_iter()
            .map_init(Vec::new, |buf, r| {
                if stop.reason().is_some() {
                    return (r, None);
                }
                bytes.fetch_add(PARTIAL_MIN, Ordering::Relaxed);
                let h = partial(&r, buf);
                (r, h)
            })
            .collect()
    });
    let mut regrouped: HashMap<(u64, blake3::Hash), Vec<Rep>> = HashMap::new();
    for (r, h) in partials {
        if let Some(h) = h {
            regrouped.entry((r.size, h)).or_default().push(r);
        }
    }
    to_full.extend(regrouped.into_values().filter(|g| qualifies(g)));

    let all: Vec<Rep> = to_full.into_iter().flatten().collect();
    stats.full = all.len();
    let fulls: HashMap<DevIno, blake3::Hash> = pool.install(|| {
        all.into_par_iter()
            .map_init(Vec::new, |buf, r| {
                if stop.reason().is_some() {
                    return None;
                }
                bytes.fetch_add(r.size, Ordering::Relaxed);
                full(&r, buf).map(|h| (r.dev_ino, h))
            })
            .flatten()
            .collect()
    });
    stats.bytes = bytes.into_inner();
    (fulls, stats)
}

/// [`two_stage`] with real reads. Unreadable files are logged and left out.
pub fn hash_groups(
    groups: Vec<Vec<Rep>>,
    pool: &rayon::ThreadPool,
    stop: &Stop,
) -> (HashMap<DevIno, blake3::Hash>, HashStats) {
    let warn = |r: &Rep, e: io::Error| {
        log::warn!("{}: cannot hash for duplicate check: {e}", r.path.display());
    };
    two_stage(
        groups,
        |r, buf| {
            partial_hash(&r.path, r.size, buf)
                .map_err(|e| warn(r, e))
                .ok()
        },
        |r, buf| hash_file(&r.path, buf).map_err(|e| warn(r, e)).ok(),
        pool,
        stop,
    )
}

/// Full hashes of `reps` on `pool`, without the size grouping (for the
/// handful of duplicates-folder chain members). Unreadable files are logged
/// and left out; a stop leaves the rest out.
pub fn full_hashes(
    reps: Vec<Rep>,
    pool: &rayon::ThreadPool,
    stop: &Stop,
) -> HashMap<DevIno, blake3::Hash> {
    pool.install(|| {
        reps.into_par_iter()
            .map_init(Vec::new, |buf, r| {
                if stop.reason().is_some() {
                    return None;
                }
                match hash_file(&r.path, buf) {
                    Ok(h) => Some((r.dev_ino, h)),
                    Err(e) => {
                        log::warn!("{}: cannot hash for duplicate check: {e}", r.path.display());
                        None
                    }
                }
            })
            .flatten()
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn pool(n: usize) -> rayon::ThreadPool {
        rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .build()
            .unwrap()
    }

    fn rep(path: &Path, ino: u64, candidate: bool) -> Rep {
        Rep {
            path: path.to_path_buf(),
            size: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
            dev_ino: (1, ino),
            candidate,
        }
    }

    #[test]
    fn equal_content_equal_hash_and_unreadable_left_out() {
        let d = tempfile::tempdir().unwrap();
        let (a, b, c) = (d.path().join("a"), d.path().join("b"), d.path().join("c"));
        std::fs::write(&a, b"same").unwrap();
        std::fs::write(&b, b"same").unwrap();
        std::fs::write(&c, b"diff").unwrap();
        let mut missing = rep(&d.path().join("missing"), 4, false);
        missing.size = 4;
        let group = vec![
            rep(&a, 1, true),
            rep(&b, 2, false),
            rep(&c, 3, false),
            missing,
        ];
        let (h, stats) = hash_groups(vec![group], &pool(2), &Stop::new());
        assert_eq!(h[&(1, 1)], h[&(1, 2)]);
        assert_ne!(h[&(1, 1)], h[&(1, 3)]);
        assert!(!h.contains_key(&(1, 4)));
        assert_eq!(
            (stats.partial, stats.full),
            (0, 4),
            "small files skip stage 1"
        );
    }

    #[test]
    fn partial_hash_sees_head_and_tail_only() {
        let d = tempfile::tempdir().unwrap();
        let size = 3 * PARTIAL_EDGE as usize;
        let base = vec![7u8; size];
        let mut middle = base.clone();
        middle[size / 2] = 0;
        let mut tail = base.clone();
        tail[size - 1] = 0;
        let w = |name: &str, bytes: &[u8]| {
            let p = d.path().join(name);
            std::fs::write(&p, bytes).unwrap();
            p
        };
        let (pb, pm, pt) = (w("b", &base), w("m", &middle), w("t", &tail));
        let mut buf = Vec::new();
        let ph = |p: &Path, buf: &mut Vec<u8>| partial_hash(p, size as u64, buf).unwrap();
        assert_eq!(ph(&pb, &mut buf), ph(&pm, &mut buf), "middle not read");
        assert_ne!(ph(&pb, &mut buf), ph(&pt, &mut buf), "tail is read");
        // A file shorter than its recorded size is unreadable, not equal.
        let short = w("s", &base[..PARTIAL_EDGE as usize + 10]);
        assert!(partial_hash(&short, size as u64, &mut buf).is_err());
    }

    /// U15 (hashing half): the stages, with injected readers.
    #[test]
    fn global_hash_set_is_size_first() {
        let big = 1_000_000;
        let r = |name: &str, size: u64, ino: u64, candidate: bool| Rep {
            path: PathBuf::from(name),
            size,
            dev_ino: (1, ino),
            candidate,
        };
        // Partial "content": same letter → same head and tail.
        let groups = vec![
            vec![
                r("a1", big, 1, true),
                r("a2", big, 2, false),
                r("a3", big, 3, false),
            ],
            vec![r("s1", 10, 4, true), r("s2", 10, 5, false)],
        ];
        let partial_calls = Mutex::new(Vec::new());
        let full_calls = Mutex::new(Vec::new());
        let (h, stats) = two_stage(
            groups,
            |r, _| {
                partial_calls.lock().unwrap().push(r.path.clone());
                // a3 differs at the head or tail.
                let tag = if r.path == Path::new("a3") { 9 } else { 1 };
                Some(blake3::hash(&[tag]))
            },
            |r, _| {
                full_calls.lock().unwrap().push(r.path.clone());
                Some(blake3::hash(r.path.as_os_str().as_encoded_bytes()))
            },
            &pool(2),
            &Stop::new(),
        );
        let mut p = partial_calls.into_inner().unwrap();
        p.sort();
        assert_eq!(p, [PathBuf::from("a1"), "a2".into(), "a3".into()]);
        let mut f = full_calls.into_inner().unwrap();
        f.sort();
        assert_eq!(
            f,
            [PathBuf::from("a1"), "a2".into(), "s1".into(), "s2".into()],
            "a3 stops at stage 1; small files go straight to stage 2"
        );
        assert!(!h.contains_key(&(1, 3)));
        assert_eq!(stats.size_groups, 2);
        assert_eq!((stats.partial, stats.full), (3, 4));
    }

    #[test]
    fn stopped_hashes_nothing() {
        let d = tempfile::tempdir().unwrap();
        let (a, b) = (d.path().join("a"), d.path().join("b"));
        std::fs::write(&a, b"same").unwrap();
        std::fs::write(&b, b"same").unwrap();
        let stop = Stop::new();
        stop.request(crate::model::NotRun::Interrupted);
        let group = vec![rep(&a, 1, true), rep(&b, 2, false)];
        assert!(hash_groups(vec![group], &pool(1), &stop).0.is_empty());
    }
}
