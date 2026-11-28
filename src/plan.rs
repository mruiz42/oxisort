//! Phase 4: decide every target path, single-threaded and in input order,
//! so the result is deterministic and dry-run shows exactly what a real run
//! does. Worker threads later only execute these decisions.
//!
//! Steps (plan 4a–4d, amendment A1):
//! - 4a: target folder and base name per file ([`target_for`]);
//! - 4b: list each target folder once ([`DestIndex`]) for name occupancy,
//!   and stat the existing files that share a name chain with some input
//!   (the in-place test); walk all of OUT ([`OutIndex`]);
//! - 4c: hash, size first, every file whose size collides with an input
//!   anywhere in OUT or the input ([`dedupe`]);
//! - 4d: walk the inputs in order: same inode as an OUT file → in place;
//!   a non-keeper of its content class → duplicate; else the first free
//!   `stem[-n].ext` for its whole bundle. Duplicate detection never looks
//!   at names, so a free slot can't hide a match (validation F1).

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use rayon::prelude::*;

use crate::Pools;
use crate::dedupe::{self, ContentKey, Member, OutIndex, WalkSkip};
use crate::exec::Stop;
use crate::extract::{ExtractError, Extracted};
use crate::hash::DevIno;
use crate::model::{
    Action, Config, DeleteGuard, DupHandling, DupPolicy, Kind, PlannedSidecar, SkipReason,
};
use crate::scan::{DirInfo, Sidecar, SidecarForm, is_junk};

/// The complete plan for a run.
#[derive(Debug, Default)]
pub struct Plan {
    pub actions: Vec<Action>,
    /// Source folders `--rmdirs` will remove, deepest first.
    pub rmdirs: Vec<RmDir>,
    /// Temp files from an interrupted run found by the OUT walk.
    pub out_leftovers: Vec<PathBuf>,
}

/// A source folder expected to be empty (apart from junk) after the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RmDir {
    pub path: PathBuf,
    /// Junk files (`.DS_Store` …) to delete first (decision Q4).
    pub junk: Vec<PathBuf>,
}

/// Where one file should go, before collision handling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    pub dir: PathBuf,
    pub stem: OsString,
    pub ext: Option<OsString>,
    /// True when the name was generated from the date (lowercase extension);
    /// false when the original name is kept.
    pub generated: bool,
}

/// One input ready for name resolution.
#[derive(Debug, Clone)]
pub(crate) struct Candidate {
    pub src: PathBuf,
    pub size: u64,
    pub dev_ino: (u64, u64),
    /// At extraction; `--delete-duplicates` re-checks it (and, for an
    /// in-place keeper, the keeper's).
    pub mtime: Option<SystemTime>,
    pub kind: Kind,
    pub dated: bool,
    pub sidecars: Vec<Sidecar>,
    pub target: Target,
}

/// What `stat` says about an existing destination file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileInfo {
    /// Size of the file (following a symlink).
    pub size: u64,
    /// `(dev, ino)` following a symlink, so a link to the input counts as
    /// already in place.
    pub dev_ino: (u64, u64),
    /// The entry itself is a symlink: never treated as a duplicate copy.
    pub symlink: bool,
}

/// A name that already exists in a target folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Existing {
    pub path: PathBuf,
    /// Filled only for chain members (`None`: unknown, never a duplicate).
    pub info: Option<FileInfo>,
}

/// `(folder, case-folded stem, case-folded extension)`: all names
/// `stem.ext`, `stem-2.ext`, `stem-3.ext`, … belong to one chain.
type ChainKey = (PathBuf, String, Option<String>);

/// Names already present in each target folder, keyed case-folded (APFS is
/// case-insensitive by default; on a case-sensitive volume this can only
/// add an unnecessary `-2`).
#[derive(Debug, Default)]
pub(crate) struct DestIndex {
    dirs: HashMap<PathBuf, HashMap<String, Existing>>,
    chains: HashMap<ChainKey, Vec<String>>,
}

impl DestIndex {
    /// Lists each folder once, in parallel. A missing folder counts as empty.
    pub fn load(dirs: &[PathBuf], pool: &rayon::ThreadPool) -> Self {
        let listed: Vec<(PathBuf, Vec<OsString>)> = pool.install(|| {
            dirs.par_iter()
                .map(|d| {
                    let names = match fs::read_dir(d) {
                        Ok(rd) => rd.filter_map(Result::ok).map(|e| e.file_name()).collect(),
                        Err(_) => Vec::new(),
                    };
                    (d.clone(), names)
                })
                .collect()
        });
        let mut index = DestIndex::default();
        for (dir, names) in listed {
            for name in names {
                index.insert(&dir, &name, None);
            }
        }
        index
    }

    /// Adds one existing name (used by `load` and by tests).
    pub fn insert(&mut self, dir: &Path, name: &OsStr, info: Option<FileInfo>) {
        let folded = fold(name);
        for key in chain_keys(dir, Path::new(name)) {
            self.chains.entry(key).or_default().push(folded.clone());
        }
        self.dirs.entry(dir.to_path_buf()).or_default().insert(
            folded,
            Existing {
                path: dir.join(name),
                info,
            },
        );
    }

    fn get(&self, dir: &Path, folded: &str) -> Option<&Existing> {
        self.dirs.get(dir)?.get(folded)
    }

    /// Stats every existing file in a chain some target belongs to.
    fn stat_chains(&mut self, targets: &[&Target], pool: &rayon::ThreadPool) {
        let mut wanted: HashSet<(PathBuf, String)> = HashSet::new();
        for t in targets {
            for key in target_keys(t) {
                for name in self.chains.get(&key).into_iter().flatten() {
                    wanted.insert((key.0.clone(), name.clone()));
                }
            }
        }
        let wanted: Vec<(PathBuf, String, PathBuf)> = wanted
            .into_iter()
            .filter_map(|(d, n)| {
                let path = self.get(&d, &n)?.path.clone();
                Some((d, n, path))
            })
            .collect();
        let infos: Vec<(PathBuf, String, Option<FileInfo>)> = pool.install(|| {
            wanted
                .into_par_iter()
                .map(|(d, n, path)| {
                    let info = stat_existing(&path);
                    (d, n, info)
                })
                .collect()
        });
        for (d, n, info) in infos {
            if let Some(e) = self.dirs.get_mut(&d).and_then(|m| m.get_mut(&n)) {
                e.info = info;
            }
        }
    }
}

fn stat_existing(path: &Path) -> Option<FileInfo> {
    let link = fs::symlink_metadata(path).ok()?;
    let meta = if link.file_type().is_symlink() {
        fs::metadata(path).ok()?
    } else {
        link.clone()
    };
    meta.is_file().then(|| FileInfo {
        size: meta.len(),
        dev_ino: (meta.dev(), meta.ino()),
        symlink: link.file_type().is_symlink(),
    })
}

/// Case-folds a file name for occupancy checks.
pub(crate) fn fold(name: &OsStr) -> String {
    name.to_string_lossy().to_lowercase()
}

/// The chains a name can belong to: its own stem, and — if the stem ends in
/// `-<n>` — the stem without that suffix.
fn chain_keys(dir: &Path, name: &Path) -> Vec<ChainKey> {
    let stem = fold(name.file_stem().unwrap_or_default());
    let ext = name.extension().map(fold);
    let mut keys = vec![(dir.to_path_buf(), stem.clone(), ext.clone())];
    if let Some((base, n)) = stem.rsplit_once('-') {
        if !base.is_empty() && !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) {
            keys.push((dir.to_path_buf(), base.to_owned(), ext));
        }
    }
    keys
}

fn target_keys(t: &Target) -> Vec<ChainKey> {
    let name = numbered_name(&t.stem, t.ext.as_deref(), 1);
    chain_keys(&t.dir, Path::new(&name))
}

/// Builds the plan from extraction results (in scan order).
// `Result<Candidate, Action>` below is an either (pending vs. decided),
// not an error path, so the size of `Action` is irrelevant.
#[allow(clippy::result_large_err)]
pub fn build(
    items: Vec<Result<Extracted, ExtractError>>,
    dirs_scanned: &[DirInfo],
    cfg: &Config,
    pools: &Pools,
    stop: &Stop,
) -> Plan {
    let staged: Vec<Result<Candidate, Action>> = items
        .into_iter()
        .map(|item| match item {
            Ok(x) => match skip_reason(&x, cfg) {
                Some(reason) => Err(Action::Skip {
                    src: x.entry.path,
                    reason,
                }),
                None => {
                    let c = candidate(x, cfg);
                    if inside_dup_dir(&c.target.dir, cfg) {
                        Err(Action::Fail {
                            error: format!(
                                "target folder {} is inside the duplicates folder",
                                c.target.dir.display()
                            ),
                            src: c.src,
                        })
                    } else {
                        Ok(c)
                    }
                }
            },
            Err(e) => Err(Action::Fail {
                src: e.entry.path,
                error: e.message,
            }),
        })
        .collect();
    let cands: Vec<&Candidate> = staged.iter().filter_map(|s| s.as_ref().ok()).collect();

    let mut dirs: Vec<PathBuf> = cands.iter().map(|c| c.target.dir.clone()).collect();
    dirs.sort();
    dirs.dedup();
    let mut index = DestIndex::load(&dirs, &pools.cpu);
    let targets: Vec<&Target> = cands.iter().map(|c| &c.target).collect();
    index.stat_chains(&targets, &pools.cpu);
    // INPUT inside OUT (or equal to it): its files are candidates, not
    // existing output. The duplicates folder is never a keeper source.
    // Both are compared by identity, so case or normalisation can't
    // defeat the check.
    let skip_dirs: Vec<DevIno> = [&cfg.input, &cfg.duplicates.dir]
        .into_iter()
        .filter_map(|p| crate::fsops::dev_ino(p).ok())
        .collect();
    // Nothing to place (all filtered, failed, or only orphan sidecars):
    // no input can be a duplicate or in place, so skip the OUT walk, a
    // full lstat pass that is slow on a big OUT on an HDD or NAS (hashing
    // then has no members and reads nothing). Empty
    // candidates still need it: one hard-linked from OUT is in place.
    let out = if cands.is_empty() {
        OutIndex::default()
    } else {
        OutIndex::build(
            &cfg.output,
            WalkSkip {
                dirs: &skip_dirs,
                file: cfg.log_file.as_deref(),
            },
            &pools.cpu,
            stop,
        )
    };
    // Hashing reads whole files, so it runs on the io pool: `--io-jobs`
    // bounds concurrent reads and spinning disks or SD cards aren't
    // thrashed (listing and stat above are metadata-only).
    let members: Vec<Member<'_>> = cands
        .iter()
        .map(|c| Member {
            path: &c.src,
            size: c.size,
            dev_ino: c.dev_ino,
            candidate: true,
        })
        .chain(out.files().iter().map(|f| Member {
            path: &f.path,
            size: f.size,
            dev_ino: f.dev_ino,
            candidate: false,
        }))
        .collect();
    let hashes = dedupe::hash_contents(&members, &pools.io, stop);

    let mut actions = resolve(staged, &index, &out, &hashes, cfg.duplicates.policy);
    if cfg.duplicates.policy == DupPolicy::File {
        actions = into_dup_dir(actions, cfg, &hashes, pools, stop);
    }
    let rmdirs = if cfg.rmdirs && cfg.mode == crate::model::Mode::Move {
        plan_rmdirs(dirs_scanned, &actions, &cfg.input)
    } else {
        Vec::new()
    };
    Plan {
        actions,
        rmdirs,
        out_leftovers: out.leftovers().to_vec(),
    }
}

/// True if `dir` would put a main-tree file inside the duplicates folder
/// (a `--date-format` whose first folder is its name). The CLI refuses the
/// cases it can see (`--prefix`, `--no-date-dir`).
fn inside_dup_dir(dir: &Path, cfg: &Config) -> bool {
    let name = cfg.duplicates.dir.file_name().map(fold);
    let first = dir
        .strip_prefix(&cfg.output)
        .ok()
        .and_then(|rel| rel.components().next())
        .map(|c| fold(c.as_os_str()));
    first.is_some() && first == name
}

/// Plan step 4e (I/O part): lists the duplicates-folder targets and, for
/// copy/link/symlink runs, stats and hashes the chain members there that
/// could already hold a duplicate ("already filed"). Then places them.
fn into_dup_dir(
    actions: Vec<Action>,
    cfg: &Config,
    hashes: &HashMap<DevIno, blake3::Hash>,
    pools: &Pools,
    stop: &Stop,
) -> Vec<Action> {
    let targets: Vec<(Target, u64, DevIno, PathBuf)> = actions
        .iter()
        .filter_map(|a| match a {
            Action::Duplicate {
                src, size, dev_ino, ..
            } => dup_target(src, &cfg.input, &cfg.duplicates.dir)
                .ok()
                .map(|t| (t, *size, *dev_ino, src.clone())),
            _ => None,
        })
        .collect();
    if targets.is_empty() {
        return actions;
    }
    let mut dirs: Vec<PathBuf> = targets.iter().map(|t| t.0.dir.clone()).collect();
    dirs.sort();
    dirs.dedup();
    let mut index = DestIndex::load(&dirs, &pools.cpu);
    // A move empties the input, so it always files; copy-like runs would
    // otherwise add another `-n` copy on every rerun of the same card.
    let check_filed = cfg.mode != crate::model::Mode::Move;
    let mut hashes = hashes.clone();
    if check_filed {
        let refs: Vec<&Target> = targets.iter().map(|t| &t.0).collect();
        index.stat_chains(&refs, &pools.cpu);
        let reps = filed_hash_reps(&targets, &index, &hashes);
        hashes.extend(crate::hash::full_hashes(reps, &pools.io, stop));
    }
    file_duplicates(
        actions,
        &DupDir {
            input: &cfg.input,
            dir: &cfg.duplicates.dir,
            index: &index,
            hashes: &hashes,
            check_filed,
        },
    )
}

/// The inodes worth hashing for the "already filed" test: chain members in
/// the duplicates folder with a duplicate's size and another inode, and
/// the duplicate itself when it has no hash yet (a hard-link duplicate).
fn filed_hash_reps(
    targets: &[(Target, u64, DevIno, PathBuf)],
    index: &DestIndex,
    hashes: &HashMap<DevIno, blake3::Hash>,
) -> Vec<crate::hash::Rep> {
    let mut reps: HashMap<DevIno, crate::hash::Rep> = HashMap::new();
    let mut add = |path: &Path, size: u64, dev_ino: DevIno| {
        if !hashes.contains_key(&dev_ino) {
            reps.entry(dev_ino).or_insert_with(|| crate::hash::Rep {
                path: path.to_path_buf(),
                size,
                dev_ino,
                candidate: false,
            });
        }
    };
    for (t, size, dev_ino, src) in targets {
        let key = own_chain(t);
        let members = index.chains.get(&key).into_iter().flatten();
        let mut any = false;
        for name in members {
            let Some(e) = index.get(&key.0, name) else {
                continue;
            };
            let Some(info) = e.info.filter(|i| !i.symlink) else {
                continue;
            };
            if info.size == *size && info.dev_ino != *dev_ino {
                add(&e.path, info.size, info.dev_ino);
                any = true;
            }
        }
        if any {
            add(src, *size, *dev_ino);
        }
    }
    let mut v: Vec<crate::hash::Rep> = reps.into_values().collect();
    v.sort_by(|a, b| a.path.cmp(&b.path));
    v
}

/// Where a duplicate goes: `<dupdir>/<src relative to INPUT>`, keeping its
/// original name and extension case.
///
/// Errors when `src` isn't under `input`. Every scanned path is, so this
/// can't happen today; but joining a non-relative remainder would put the
/// "duplicates folder" anywhere (an absolute `src` replaces `dup_dir`), so
/// the file fails instead of being filed next to itself.
fn dup_target(src: &Path, input: &Path, dup_dir: &Path) -> Result<Target, String> {
    let rel = src.strip_prefix(input).map_err(|_| {
        format!(
            "not filed: {} is outside the input folder {}",
            src.display(),
            input.display()
        )
    })?;
    let parent = rel.parent().unwrap_or(Path::new(""));
    Ok(Target {
        dir: dup_dir.join(parent),
        stem: src.file_stem().unwrap_or_default().to_owned(),
        ext: src.extension().map(OsStr::to_owned),
        generated: false,
    })
}

/// Recovers how a sidecar was paired with its media file (as the scan
/// did): `name.ext.xmp` follows `name.ext`; anything else is `name.xmp`.
fn sidecar_form(media: &Path, sidecar: &Path) -> SidecarForm {
    let side = sidecar.file_name().unwrap_or_default().to_string_lossy();
    let media = media.file_name().unwrap_or_default().to_string_lossy();
    let base = side.get(..side.len().saturating_sub(4)).unwrap_or("");
    if base.eq_ignore_ascii_case(&media) {
        SidecarForm::WithExt
    } else {
        SidecarForm::StemOnly
    }
}

/// Inputs of plan step 4e.
pub(crate) struct DupDir<'a> {
    pub input: &'a Path,
    /// `OUT/<--duplicates-dir>`.
    pub dir: &'a Path,
    /// Names in the duplicates-folder targets (chain members stat'ed when
    /// `check_filed`).
    pub index: &'a DestIndex,
    pub hashes: &'a HashMap<DevIno, blake3::Hash>,
    /// Copy/link/symlink: skip a duplicate already in its chain there.
    pub check_filed: bool,
}

/// Plan step 4e (pure): turns every report-only duplicate into "file it at
/// `dst`" (unique names, `-2`, `-3` … over the whole bundle, exactly like
/// the main tree) or "already filed at `at`". Runs in action order, so the
/// result is deterministic.
pub(crate) fn file_duplicates(actions: Vec<Action>, dd: &DupDir<'_>) -> Vec<Action> {
    let filed = FiledIndex::new(dd.index, dd.hashes);
    let mut r = Resolver {
        index: dd.index,
        planned: HashSet::new(),
        cursors: HashMap::new(),
    };
    actions
        .into_iter()
        .map(|a| {
            let Action::Duplicate {
                src,
                of,
                handling: DupHandling::Report,
                depends_on,
                size,
                dev_ino,
                sidecars_left,
            } = a
            else {
                return a;
            };
            let target = match dup_target(&src, dd.input, dd.dir) {
                Ok(t) => t,
                Err(error) => return Action::Fail { src, error },
            };
            let hash = dd.hashes.get(&dev_ino).copied();
            let at = dd
                .check_filed
                .then(|| filed.find(&target, size, dev_ino, hash))
                .flatten();
            let handling = match at {
                Some(at) => {
                    return Action::Duplicate {
                        src,
                        of,
                        handling: DupHandling::AlreadyFiled { at },
                        depends_on,
                        size,
                        dev_ino,
                        sidecars_left,
                    };
                }
                None => {
                    let sidecars = sidecars_left
                        .iter()
                        .map(|p| Sidecar {
                            form: sidecar_form(&src, p),
                            path: p.clone(),
                        })
                        .collect();
                    let c = Candidate {
                        src: src.clone(),
                        size,
                        dev_ino,
                        // Only placed (never deleted), so it needs no mtime.
                        mtime: None,
                        kind: Kind::Other,
                        dated: false,
                        sidecars,
                        target,
                    };
                    match r.place(c) {
                        Action::Transfer { dst, sidecars, .. } => {
                            DupHandling::File { dst, sidecars }
                        }
                        other => return other,
                    }
                }
            };
            Action::Duplicate {
                src,
                of,
                handling,
                depends_on,
                size,
                dev_ino,
                sidecars_left: Vec::new(),
            }
        })
        .collect()
}

/// Chain members in the duplicates folder, by inode (any entry, a symlink
/// resolving to it included) and by content (regular files only).
struct FiledIndex {
    by_ino: ChainInodes,
    by_hash: HashMap<(ChainKey, u64, blake3::Hash), (u32, PathBuf)>,
}

impl FiledIndex {
    fn new(index: &DestIndex, hashes: &HashMap<DevIno, blake3::Hash>) -> Self {
        let mut by_hash: HashMap<(ChainKey, u64, blake3::Hash), (u32, PathBuf)> = HashMap::new();
        for e in index.dirs.values().flat_map(HashMap::values) {
            let (Some(info), Some(dir), Some(name)) = (e.info, e.path.parent(), e.path.file_name())
            else {
                continue;
            };
            let Some(h) = hashes.get(&info.dev_ino).filter(|_| !info.symlink) else {
                continue;
            };
            for (key, n) in chain_positions(dir, Path::new(name)) {
                let cand = (n, e.path.clone());
                by_hash
                    .entry((key, info.size, *h))
                    .and_modify(|cur| {
                        if cand < *cur {
                            *cur = cand.clone();
                        }
                    })
                    .or_insert(cand);
            }
        }
        FiledIndex {
            by_ino: ChainInodes::new(index),
            by_hash,
        }
    }

    /// A chain member, at any position, that already holds this content.
    fn find(
        &self,
        t: &Target,
        size: u64,
        dev_ino: DevIno,
        hash: Option<blake3::Hash>,
    ) -> Option<PathBuf> {
        if let Some(p) = self.by_ino.holder(t, dev_ino) {
            return Some(p.to_path_buf());
        }
        let key = (own_chain(t), size, hash?);
        self.by_hash.get(&key).map(|(_, p)| p.clone())
    }
}

/// Plan step 3, the filters: `--file-type` (non-media is always out when it
/// is set), `--skip-unknown`, then `--from`/`--to` on the capture day
/// (inclusive; undated files are never range-filtered).
fn skip_reason(x: &Extracted, cfg: &Config) -> Option<SkipReason> {
    if cfg.file_type.is_some_and(|k| k != x.kind) {
        return Some(SkipReason::FileType);
    }
    let Some(date) = &x.date else {
        return cfg.skip_unknown.then_some(SkipReason::NoDate);
    };
    let day = date.wall.date();
    if cfg.from.is_some_and(|f| day < f) || cfg.to.is_some_and(|t| day > t) {
        return Some(SkipReason::DateRange);
    }
    None
}

/// Simulates `--rmdirs`: which scanned folders end up holding nothing but
/// junk once every planned move, filed duplicate and duplicate deletion has
/// happened.
///
/// Never candidates: the input root, unreadable folders, and any folder
/// that is (or contains) a target folder of this run — which also covers
/// organizing in place.
pub(crate) fn plan_rmdirs(dirs: &[DirInfo], actions: &[Action], input: &Path) -> Vec<RmDir> {
    let mut removed_in: HashMap<PathBuf, usize> = HashMap::new();
    let mut protected: HashSet<PathBuf> = HashSet::new();
    let mut count_removed = |p: &Path| {
        if let Some(parent) = p.parent() {
            *removed_in.entry(parent.to_path_buf()).or_default() += 1;
        }
    };
    for a in actions {
        match a {
            Action::Transfer {
                src, dst, sidecars, ..
            } => {
                count_removed(src);
                sidecars.iter().for_each(|s| count_removed(&s.src));
                protected.extend(dst.ancestors().skip(1).map(Path::to_path_buf));
            }
            Action::Duplicate {
                src,
                handling: DupHandling::Delete(_),
                ..
            } => count_removed(src),
            // Only called for --move, so a filed duplicate leaves the input.
            Action::Duplicate {
                src,
                handling: DupHandling::File { dst, sidecars },
                ..
            } => {
                count_removed(src);
                sidecars.iter().for_each(|s| count_removed(&s.src));
                protected.extend(dst.ancestors().skip(1).map(Path::to_path_buf));
            }
            _ => {}
        }
    }
    let mut order: Vec<&DirInfo> = dirs.iter().collect();
    order.sort_by(|a, b| b.depth.cmp(&a.depth).then_with(|| a.path.cmp(&b.path)));
    let mut emptied_subdirs: HashMap<PathBuf, usize> = HashMap::new();
    let mut out = Vec::new();
    for d in order {
        let gone = d.junk.len()
            + removed_in.get(&d.path).copied().unwrap_or(0)
            + emptied_subdirs.get(&d.path).copied().unwrap_or(0);
        let empty = d.children.saturating_sub(gone) == 0;
        if empty && !d.unreadable && d.path != input && !protected.contains(&d.path) {
            if let Some(parent) = d.path.parent() {
                *emptied_subdirs.entry(parent.to_path_buf()).or_default() += 1;
            }
            out.push(RmDir {
                path: d.path.clone(),
                junk: d.junk.clone(),
            });
        }
    }
    out
}

/// True if a folder entry is junk that `--rmdirs` may delete.
pub(crate) fn is_removable_junk(entry: &fs::DirEntry) -> bool {
    entry.file_type().is_ok_and(|t| t.is_file()) && is_junk(&entry.file_name())
}

fn candidate(x: Extracted, cfg: &Config) -> Candidate {
    let target = target_for(&x, cfg);
    Candidate {
        size: x.size,
        dev_ino: (x.dev, x.ino),
        mtime: x.mtime.map(SystemTime::from),
        kind: x.kind,
        dated: x.date.is_some(),
        target,
        src: x.entry.path,
        sidecars: x.entry.sidecars,
    }
}

/// Inputs whose own name chain already holds their inode, i.e. files that
/// already sit at one of their own target names (organizing in place, or a
/// link/symlink rerun). Any chain position counts, so a file at `x-3.jpg`
/// stays put even when `x-2.jpg` is free (rev 1's V4).
#[derive(Debug, Default)]
struct ChainInodes {
    /// `(chain, inode)` → the lowest `(n, path)` holding it.
    by_ino: HashMap<(ChainKey, DevIno), (u32, PathBuf)>,
}

impl ChainInodes {
    fn new(index: &DestIndex) -> Self {
        let mut by_ino: HashMap<(ChainKey, DevIno), (u32, PathBuf)> = HashMap::new();
        for e in index.dirs.values().flat_map(HashMap::values) {
            let (Some(info), Some(dir), Some(name)) = (e.info, e.path.parent(), e.path.file_name())
            else {
                continue;
            };
            for (key, n) in chain_positions(dir, Path::new(name)) {
                let cand = (n, e.path.clone());
                by_ino
                    .entry((key, info.dev_ino))
                    .and_modify(|cur| {
                        if cand < *cur {
                            *cur = cand.clone();
                        }
                    })
                    .or_insert(cand);
            }
        }
        ChainInodes { by_ino }
    }

    /// The existing path in `t`'s own chain that has inode `dev_ino`.
    fn holder(&self, t: &Target, dev_ino: DevIno) -> Option<&Path> {
        self.by_ino
            .get(&(own_chain(t), dev_ino))
            .map(|(_, p)| p.as_path())
    }
}

/// The chain a target's names are drawn from (its stem at `n = 1`).
fn own_chain(t: &Target) -> ChainKey {
    let first = numbered_name(&t.stem, t.ext.as_deref(), 1);
    chain_positions(&t.dir, Path::new(&first)).swap_remove(0).0
}

/// The chains a name sits in, with its position `n` in each: its own stem
/// at `n = 1`, and — when the stem is `base-<n>` as [`numbered_name`] would
/// write it (`n >= 2`, no leading zero) — `base` at `n`.
fn chain_positions(dir: &Path, name: &Path) -> Vec<(ChainKey, u32)> {
    let stem = fold(name.file_stem().unwrap_or_default());
    let ext = name.extension().map(fold);
    let mut out = vec![((dir.to_path_buf(), stem.clone(), ext.clone()), 1)];
    if let Some((base, digits)) = stem.rsplit_once('-') {
        if let Ok(n) = digits.parse::<u32>() {
            if !base.is_empty() && n >= 2 && n.to_string() == digits {
                out.push(((dir.to_path_buf(), base.to_owned(), ext), n));
            }
        }
    }
    out
}

/// The file a content class keeps; every other input of the class is a
/// duplicate of it.
#[derive(Debug, Clone)]
enum Keeper {
    /// Rule 1: an existing output file.
    Existing {
        path: PathBuf,
        dev_ino: DevIno,
        mtime: Option<SystemTime>,
    },
    /// Rule 2: an input already in place; it stays where it is.
    InPlace {
        task: usize,
        src: PathBuf,
        dev_ino: DevIno,
        mtime: Option<SystemTime>,
    },
    /// Rule 3: the first input of the class, transferred by this run.
    Planned {
        task: usize,
        dst: PathBuf,
        dev_ino: DevIno,
    },
}

/// Plan step 4d state for naming. Correctness (no two tasks share a name,
/// no existing name is reused) rests on `planned` + the `DestIndex` alone;
/// `cursors` only make the search fast.
struct Resolver<'a> {
    index: &'a DestIndex,
    /// Every name planned so far (main files and sidecars), case-folded.
    planned: HashSet<(PathBuf, String)>,
    /// Per chain: every `n` below the cursor is known to be occupied.
    /// Occupancy only grows, so the cursor never has to move back.
    cursors: HashMap<ChainKey, u32>,
}

/// Plan step 4d (pure): classifies every input and assigns final names, in
/// input order.
///
/// - `index`: names in the target folders (occupancy) and the stat of
///   chain members (in-place test);
/// - `out`: every regular file and symlink under OUT;
/// - `hashes`: full hashes per inode, as selected by `dedupe::size_groups`
///   (an inode without one is unique, or unreadable: never a duplicate).
///
/// Keeper rules per content class (amendment A1): (1) the first existing
/// output file by `(folder, n, name)`; else (2) the first input already in
/// place, by the same order of its holder; else (3) the first input in
/// input order, which is transferred. An input with the same inode as any
/// OUT file is in place (A1-Q2); empty files are never duplicates.
pub(crate) fn resolve(
    staged: Vec<Result<Candidate, Action>>,
    index: &DestIndex,
    out: &OutIndex,
    hashes: &HashMap<DevIno, blake3::Hash>,
    policy: DupPolicy,
) -> Vec<Action> {
    let chain_inodes = ChainInodes::new(index);
    let out_holders = out.holders();
    let key_of = |c: &Candidate| dedupe::content_key(c.size, c.dev_ino, hashes);

    let mut keepers: HashMap<ContentKey, Keeper> = dedupe::existing_keepers(out, hashes)
        .into_iter()
        .map(|(k, f)| {
            let keeper = Keeper::Existing {
                path: f.path.clone(),
                dev_ino: f.dev_ino,
                mtime: f.mtime,
            };
            (k, keeper)
        })
        .collect();
    for (key, keeper) in in_place_keepers(&staged, &chain_inodes, &out_holders, &key_of) {
        keepers.entry(key).or_insert(keeper);
    }

    let mut r = Resolver {
        index,
        planned: HashSet::new(),
        cursors: HashMap::new(),
    };
    staged
        .into_iter()
        .enumerate()
        .map(|(task, s)| {
            let c = match s {
                Ok(c) => c,
                Err(action) => return action,
            };
            if out_holders.contains_key(&c.dev_ino) {
                return Action::AlreadyInPlace { src: c.src };
            }
            if c.size == 0 {
                if chain_inodes.holder(&c.target, c.dev_ino).is_some() {
                    return Action::AlreadyInPlace { src: c.src };
                }
                return r.place(c);
            }
            let key = key_of(&c);
            match keepers.get(&key).cloned() {
                Some(Keeper::Existing {
                    path,
                    dev_ino,
                    mtime,
                }) => duplicate(c, path, (dev_ino, mtime), None, policy),
                Some(Keeper::InPlace { task: k, .. }) if k == task => {
                    Action::AlreadyInPlace { src: c.src }
                }
                Some(Keeper::InPlace {
                    src,
                    dev_ino,
                    mtime,
                    ..
                }) => duplicate(c, src, (dev_ino, mtime), None, policy),
                Some(Keeper::Planned {
                    task: k,
                    dst,
                    dev_ino,
                }) => duplicate(c, dst, (dev_ino, None), Some(k), policy),
                None => {
                    let dev_ino = c.dev_ino;
                    let action = r.place(c);
                    // A failed placement leaves the class open, so the next
                    // input of the class becomes its keeper.
                    if let Action::Transfer { dst, .. } = &action {
                        let dst = dst.clone();
                        keepers.insert(key, Keeper::Planned { task, dst, dev_ino });
                    }
                    action
                }
            }
        })
        .collect()
}

/// Rule 2 keepers: per class, the in-place input whose holder comes first
/// by `(folder, n, name)`; among inputs sharing one holder (hard links),
/// the one that *is* the holder, then input order. That keeper never
/// moves, so the others can safely be its duplicates.
fn in_place_keepers(
    staged: &[Result<Candidate, Action>],
    chain_inodes: &ChainInodes,
    out_holders: &HashMap<DevIno, &Path>,
    key_of: &impl Fn(&Candidate) -> ContentKey,
) -> HashMap<ContentKey, Keeper> {
    type Rank = ((PathBuf, u32, OsString), bool, usize);
    let mut best: HashMap<ContentKey, (Rank, Keeper)> = HashMap::new();
    for (task, c) in staged.iter().enumerate() {
        let Ok(c) = c else { continue };
        if c.size == 0 {
            continue;
        }
        let holder = out_holders
            .get(&c.dev_ino)
            .copied()
            .or_else(|| chain_inodes.holder(&c.target, c.dev_ino));
        let Some(holder) = holder else { continue };
        let rank: Rank = (dedupe::order_key(holder), c.src != holder, task);
        let keeper = Keeper::InPlace {
            task,
            src: c.src.clone(),
            dev_ino: c.dev_ino,
            mtime: c.mtime,
        };
        best.entry(key_of(c))
            .and_modify(|cur| {
                if rank < cur.0 {
                    *cur = (rank.clone(), keeper.clone());
                }
            })
            .or_insert((rank, keeper));
    }
    best.into_iter()
        .map(|(k, (_, keeper))| (k, keeper))
        .collect()
}

impl Resolver<'_> {
    fn taken(&self, dir: &Path, folded: &str) -> bool {
        self.index.get(dir, folded).is_some()
            || self
                .planned
                .contains(&(dir.to_path_buf(), folded.to_owned()))
    }

    /// Gives `c` the lowest `n` whose whole bundle (main name plus sidecar
    /// names) is free, and reserves those names.
    fn place(&mut self, c: Candidate) -> Action {
        let key = own_chain(&c.target);
        let Some((name, side_names)) = self.free_slot(&key, &c) else {
            let t = &c.target;
            return Action::Fail {
                error: format!("no free name left for {}", t.dir.join(&t.stem).display()),
                src: c.src,
            };
        };
        let dir = &c.target.dir;
        self.planned.insert((dir.clone(), fold(&name)));
        for s in &side_names {
            self.planned.insert((dir.clone(), fold(s)));
        }
        let sidecars = c
            .sidecars
            .iter()
            .zip(side_names)
            .map(|(s, n)| PlannedSidecar {
                src: s.path.clone(),
                dst: dir.join(n),
            })
            .collect();
        Action::Transfer {
            dst: dir.join(&name),
            src: c.src,
            kind: c.kind,
            dated: c.dated,
            size: c.size,
            sidecars,
        }
    }

    /// The lowest `n` whose main name and sidecar names are all free,
    /// starting at the chain's cursor (amortised O(1) per input even when
    /// thousands of files share one timestamp). `None` only if `u32` runs
    /// out, which needs more files than any filesystem folder can hold.
    fn free_slot(&mut self, key: &ChainKey, c: &Candidate) -> Option<(OsString, Vec<OsString>)> {
        let t = &c.target;
        let mut n = self.cursors.get(key).copied().unwrap_or(1);
        let mut advancing = true;
        loop {
            let name = numbered_name(&t.stem, t.ext.as_deref(), n);
            if self.taken(&t.dir, &fold(&name)) {
                if advancing {
                    self.cursors.insert(key.clone(), n.checked_add(1)?);
                }
            } else {
                advancing = false;
                let stem_n = numbered_name(&t.stem, None, n);
                let side_names: Vec<OsString> = c
                    .sidecars
                    .iter()
                    .map(|s| sidecar_name(&name, &stem_n, s, t.generated))
                    .collect();
                if !side_names.iter().any(|s| self.taken(&t.dir, &fold(s))) {
                    return Some((name, side_names));
                }
            }
            n = n.checked_add(1)?;
        }
    }
}

/// A duplicate of `of` under the run's policy. A hard link to `of` is
/// never deleted: removing it frees nothing and drops a name the user made.
///
/// `of_stat` is the keeper's `(dev, ino)` and its mtime at planning (`None`
/// for a keeper this run creates).
fn duplicate(
    c: Candidate,
    of: PathBuf,
    (of_dev_ino, of_mtime): (DevIno, Option<SystemTime>),
    depends_on: Option<usize>,
    policy: DupPolicy,
) -> Action {
    let handling = match policy {
        // Filing needs the duplicates-folder index; step 4e turns these
        // into `File` / `AlreadyFiled` (`file_duplicates`).
        DupPolicy::File | DupPolicy::Skip => DupHandling::Report,
        DupPolicy::Delete if c.dev_ino == of_dev_ino => DupHandling::KeepSameFile,
        DupPolicy::Delete => DupHandling::Delete(DeleteGuard {
            src_mtime: c.mtime,
            of_mtime,
        }),
    };
    Action::Duplicate {
        of,
        handling,
        depends_on,
        size: c.size,
        dev_ino: c.dev_ino,
        sidecars_left: c.sidecars.into_iter().map(|s| s.path).collect(),
        src: c.src,
    }
}

/// Computes the folder and base name for one file (plan step 4a).
pub(crate) fn target_for(x: &Extracted, cfg: &Config) -> Target {
    let mut dir = cfg.output.clone();
    if let Some(p) = &cfg.prefix {
        dir.push(p);
    }
    match &x.date {
        Some(d) => dir.push(cfg.dir_format.render(d.wall.date())),
        None => dir.push(&cfg.no_date_dir),
    }
    if let Some(s) = &cfg.suffix {
        dir.push(s);
    }
    let path = &x.entry.path;
    match &x.date {
        Some(d) if !cfg.original_names => {
            let mut stem = OsString::from(d.wall.format("%Y%m%d-%H%M%S").to_string());
            if let Some(sub) = &d.subsec {
                stem.push(sub);
            }
            Target {
                dir,
                stem,
                ext: path
                    .extension()
                    .map(|e| OsString::from(e.to_string_lossy().to_lowercase())),
                generated: true,
            }
        }
        // No date, or --original-names: keep the name verbatim.
        _ => Target {
            dir,
            stem: path.file_stem().unwrap_or_default().to_owned(),
            ext: path.extension().map(OsStr::to_owned),
            generated: false,
        },
    }
}

/// `stem[-n][.ext]`.
pub(crate) fn numbered_name(stem: &OsStr, ext: Option<&OsStr>, n: u32) -> OsString {
    let mut name = stem.to_owned();
    if n > 1 {
        name.push(format!("-{n}"));
    }
    if let Some(e) = ext {
        name.push(".");
        name.push(e);
    }
    name
}

/// Target name of a sidecar, derived from the final main name (so a
/// collision gives `x-2.jpg.xmp`, not `x.jpg-2.xmp`): `name.ext.xmp` → `<main name>.xmp`,
/// `name.xmp` → `<main stem>.xmp`. Generated names use a lowercase `.xmp`;
/// kept names keep the sidecar's own spelling.
pub(crate) fn sidecar_name(
    main_name: &OsStr,
    main_stem_n: &OsStr,
    sidecar: &Sidecar,
    generated: bool,
) -> OsString {
    let xmp_ext: OsString = if generated {
        "xmp".into()
    } else {
        sidecar
            .path
            .extension()
            .unwrap_or(OsStr::new("xmp"))
            .to_owned()
    };
    let mut name = match sidecar.form {
        SidecarForm::WithExt => main_name.to_owned(),
        SidecarForm::StemOnly => main_stem_n.to_owned(),
    };
    name.push(".");
    name.push(xmp_ext);
    name
}

/// Every distinct folder that transfers and filed duplicates write into,
/// sorted.
pub fn target_dirs(plan: &Plan) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = plan
        .actions
        .iter()
        .filter_map(|a| match a {
            Action::Transfer { dst, .. }
            | Action::Duplicate {
                handling: DupHandling::File { dst, .. },
                ..
            } => dst.parent().map(Path::to_path_buf),
            _ => None,
        })
        .collect();
    dirs.sort();
    dirs.dedup();
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::Stop;
    use crate::hash::{self, Rep};

    const OUT: &str = "/out/2017/01/02";

    fn cand(src: &str, stem: &str, size: u64, ino: u64) -> Candidate {
        Candidate {
            src: PathBuf::from(src),
            size,
            dev_ino: (1, ino),
            mtime: None,
            kind: Kind::Image,
            dated: true,
            sidecars: vec![],
            target: Target {
                dir: PathBuf::from(OUT),
                stem: stem.into(),
                ext: Some("jpg".into()),
                generated: true,
            },
        }
    }

    fn with_sidecars(mut c: Candidate, forms: &[SidecarForm]) -> Candidate {
        for form in forms {
            let suffix = match form {
                SidecarForm::WithExt => ".jpg.xmp",
                SidecarForm::StemOnly => ".xmp",
            };
            let src = c.src.with_extension("").display().to_string() + suffix;
            c.sidecars.push(Sidecar {
                path: src.into(),
                form: *form,
            });
        }
        c
    }

    fn info(size: u64, ino: u64) -> Option<FileInfo> {
        Some(FileInfo {
            size,
            dev_ino: (1, ino),
            symlink: false,
        })
    }

    /// The plan's view of the world: target-folder names, OUT, hashes.
    #[derive(Default)]
    struct World {
        dest: DestIndex,
        out: OutIndex,
        hashes: HashMap<DevIno, blake3::Hash>,
    }

    impl World {
        /// An existing regular output file; `content` = Some(byte) if it
        /// was hashed (its size collides with an input).
        fn file(&mut self, dir: &str, name: &str, size: u64, ino: u64, content: Option<u8>) {
            self.dest
                .insert(Path::new(dir), OsStr::new(name), info(size, ino));
            self.out
                .insert_file(Path::new(dir).join(name), size, (1, ino));
            if let Some(b) = content {
                self.content(ino, b);
            }
        }

        /// A name that only blocks a slot (a stale sidecar, say).
        fn occupied(&mut self, dir: &str, name: &str) {
            self.dest.insert(Path::new(dir), OsStr::new(name), None);
        }

        fn content(&mut self, ino: u64, b: u8) {
            self.hashes.insert((1, ino), blake3::hash(&[b]));
        }

        fn resolve(&self, cands: Vec<Candidate>, policy: DupPolicy) -> Vec<Action> {
            let staged = cands.into_iter().map(Ok).collect();
            resolve(staged, &self.dest, &self.out, &self.hashes, policy)
        }
    }

    fn dst(a: &Action) -> String {
        match a {
            Action::Transfer { dst, .. } => dst.display().to_string(),
            other => panic!("expected transfer, got {other:?}"),
        }
    }

    /// `(of, depends_on, handling)` of a duplicate.
    fn dup(a: &Action) -> (String, Option<usize>, DupHandling) {
        match a {
            Action::Duplicate {
                of,
                depends_on,
                handling,
                ..
            } => (of.display().to_string(), *depends_on, handling.clone()),
            other => panic!("expected duplicate, got {other:?}"),
        }
    }

    fn in_place(a: &Action) -> bool {
        matches!(a, Action::AlreadyInPlace { .. })
    }

    #[test]
    fn collision_chain() {
        let acts = World::default().resolve(
            vec![
                cand("/in/a.jpg", "x", 1, 11),
                cand("/in/b.jpg", "x", 2, 12),
                cand("/in/c.jpg", "X", 3, 13), // case-folded collision
            ],
            DupPolicy::Skip,
        );
        let got: Vec<String> = acts.iter().map(dst).collect();
        assert_eq!(
            got,
            [
                format!("{OUT}/x.jpg"),
                format!("{OUT}/x-2.jpg"),
                format!("{OUT}/X-3.jpg")
            ]
        );
    }

    #[test]
    fn duplicate_of_existing() {
        let mut w = World::default();
        w.file(OUT, "x.jpg", 5, 99, Some(7));
        w.content(11, 7);
        let acts = w.resolve(vec![cand("/in/a.jpg", "x", 5, 11)], DupPolicy::Delete);
        assert_eq!(
            dup(&acts[0]),
            (
                format!("{OUT}/x.jpg"),
                None,
                DupHandling::Delete(DeleteGuard::default())
            )
        );
    }

    #[test]
    fn same_size_different_content_takes_next_name() {
        let mut w = World::default();
        w.file(OUT, "x.jpg", 5, 99, Some(7));
        w.content(11, 8);
        let acts = w.resolve(vec![cand("/in/a.jpg", "x", 5, 11)], DupPolicy::Skip);
        assert_eq!(dst(&acts[0]), format!("{OUT}/x-2.jpg"));
    }

    #[test]
    fn duplicate_within_input_depends_on_first() {
        let mut w = World::default();
        w.content(11, 1);
        w.content(12, 1);
        let acts = w.resolve(
            vec![cand("/in/a.jpg", "x", 5, 11), cand("/in/b.jpg", "x", 5, 12)],
            DupPolicy::Skip,
        );
        assert_eq!(dst(&acts[0]), format!("{OUT}/x.jpg"));
        assert_eq!(
            dup(&acts[1]),
            (format!("{OUT}/x.jpg"), Some(0), DupHandling::Report)
        );
    }

    #[test]
    fn same_inode_is_in_place() {
        let mut w = World::default();
        w.file(OUT, "x.jpg", 5, 11, None);
        let acts = w.resolve(vec![cand("/in/a.jpg", "x", 5, 11)], DupPolicy::Delete);
        assert!(in_place(&acts[0]), "{acts:?}");
    }

    /// U10.
    #[test]
    fn hardlinked_inputs_are_duplicates_never_deleted() {
        let acts = World::default().resolve(
            vec![cand("/in/a.jpg", "x", 5, 11), cand("/in/b.jpg", "x", 5, 11)],
            DupPolicy::Delete,
        );
        assert_eq!(dst(&acts[0]), format!("{OUT}/x.jpg"));
        assert_eq!(
            dup(&acts[1]),
            (format!("{OUT}/x.jpg"), Some(0), DupHandling::KeepSameFile)
        );
    }

    /// U12.
    #[test]
    fn out_symlink_is_never_a_content_keeper() {
        let mut w = World::default();
        let link = Some(FileInfo {
            size: 5,
            dev_ino: (1, 99),
            symlink: true,
        });
        w.dest.insert(Path::new(OUT), OsStr::new("x.jpg"), link);
        w.out.insert_link(format!("{OUT}/x.jpg"), (1, 99));
        w.out.insert_link("/out/other/y.jpg", (1, 99));
        w.content(99, 7);
        w.content(11, 7);
        let acts = w.resolve(vec![cand("/in/a.jpg", "x", 5, 11)], DupPolicy::Delete);
        assert_eq!(dst(&acts[0]), format!("{OUT}/x-2.jpg"));
    }

    #[test]
    fn sidecar_name_collision_bumps_the_bundle() {
        let mut w = World::default();
        w.occupied(OUT, "x.xmp");
        let c = with_sidecars(cand("/in/a.jpg", "x", 5, 11), &[SidecarForm::StemOnly]);
        let acts = w.resolve(vec![c], DupPolicy::Skip);
        match &acts[0] {
            Action::Transfer { dst, sidecars, .. } => {
                assert_eq!(dst, &PathBuf::from(format!("{OUT}/x-2.jpg")));
                assert_eq!(sidecars[0].dst, PathBuf::from(format!("{OUT}/x-2.xmp")));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn sidecar_names() {
        let s = |p: &str, form| Sidecar {
            path: p.into(),
            form,
        };
        let with = s("/in/A.JPG.XMP", SidecarForm::WithExt);
        let stem = s("/in/A.XMP", SidecarForm::StemOnly);
        let n =
            |a: &str, b: &str, sc: &Sidecar, g| sidecar_name(OsStr::new(a), OsStr::new(b), sc, g);
        assert_eq!(n("x-2.jpg", "x-2", &with, true), "x-2.jpg.xmp");
        assert_eq!(n("x-2.jpg", "x-2", &stem, true), "x-2.xmp");
        assert_eq!(n("A.JPG", "A", &with, false), "A.JPG.XMP");
    }

    #[test]
    fn many_files_sharing_one_timestamp_all_get_names_fast() {
        // 12,000 > the old 9,999 cap. Correctness only, no wall-clock
        // check: a timing assert would be flaky on a loaded CI box.
        const N: u64 = 12_000;
        let cands = (0..N)
            .map(|i| cand(&format!("/in/{i}.jpg"), "x", i + 1, i))
            .collect();
        let acts = World::default().resolve(cands, DupPolicy::Skip);
        assert_eq!(dst(&acts[0]), format!("{OUT}/x.jpg"));
        assert_eq!(dst(&acts[N as usize - 1]), format!("{OUT}/x-{N}.jpg"));
        let unique: HashSet<String> = acts.iter().map(dst).collect();
        assert_eq!(unique.len(), N as usize);
    }

    #[test]
    fn duplicate_found_deep_in_a_long_chain() {
        // Existing x.jpg … x-500.jpg; the input equals x-300.jpg.
        let mut w = World::default();
        for n in 1..=500u32 {
            let name = numbered_name(OsStr::new("x"), Some(OsStr::new("jpg")), n);
            let content = (n == 300).then_some(3);
            w.file(
                OUT,
                &name.to_string_lossy(),
                u64::from(n),
                1000 + u64::from(n),
                content,
            );
        }
        w.content(11, 3);
        let acts = w.resolve(vec![cand("/in/a.jpg", "x", 300, 11)], DupPolicy::Delete);
        assert_eq!(dup(&acts[0]).0, format!("{OUT}/x-300.jpg"));
        // A different file still gets the first free number.
        let acts = w.resolve(vec![cand("/in/b.jpg", "x", 7, 12)], DupPolicy::Skip);
        assert_eq!(dst(&acts[0]), format!("{OUT}/x-501.jpg"));
    }

    #[test]
    fn match_still_found_when_a_sidecar_name_is_taken() {
        // x.xmp blocks x as a free slot for the bundle, but x.jpg is
        // identical to the input, so the input is its duplicate.
        let mut w = World::default();
        w.occupied(OUT, "x.xmp");
        w.file(OUT, "x.jpg", 5, 90, Some(3));
        w.content(11, 3);
        let c = with_sidecars(cand("/in/a.jpg", "x", 5, 11), &[SidecarForm::StemOnly]);
        let acts = w.resolve(vec![c], DupPolicy::Delete);
        assert_eq!(dup(&acts[0]).0, format!("{OUT}/x.jpg"));
    }

    #[test]
    fn other_stem_holding_a_numbered_name_is_found() {
        // "x-2" (a kept original name) lands on x's position 2.
        let mut w = World::default();
        w.content(11, 1);
        w.content(12, 1);
        let acts = w.resolve(
            vec![
                cand("/in/o.jpg", "x", 9, 10),
                cand("/in/a.jpg", "x-2", 5, 11),
                cand("/in/b.jpg", "x", 5, 12),
            ],
            DupPolicy::Skip,
        );
        assert_eq!(dst(&acts[1]), format!("{OUT}/x-2.jpg"));
        assert_eq!(dup(&acts[2]).1, Some(1));
    }

    /// U1: the exact F1 shape. Existing different `T.jpg` and a stale
    /// `T-2.jpg.xmp`: a (with both sidecars) can't use `-2`, so it takes
    /// `-3`; its sidecar-less twin b must not land in the `-2` gap.
    #[test]
    fn f1_planned_twin_above_a_sidecar_gap_is_a_duplicate() {
        let mut w = World::default();
        w.file(OUT, "T.jpg", 5, 90, Some(9));
        w.occupied(OUT, "T-2.jpg.xmp");
        w.content(11, 1);
        w.content(12, 1);
        let a = with_sidecars(
            cand("/in/dto.jpg", "T", 5, 11),
            &[SidecarForm::WithExt, SidecarForm::StemOnly],
        );
        let b = cand("/in/sub/dto_copy.jpg", "T", 5, 12);
        let acts = w.resolve(vec![a, b], DupPolicy::Skip);
        match &acts[0] {
            Action::Transfer { dst, sidecars, .. } => {
                assert_eq!(dst, &PathBuf::from(format!("{OUT}/T-3.jpg")));
                let side: Vec<String> = sidecars
                    .iter()
                    .map(|s| s.dst.display().to_string())
                    .collect();
                assert_eq!(
                    side,
                    [format!("{OUT}/T-3.jpg.xmp"), format!("{OUT}/T-3.xmp")]
                );
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            dup(&acts[1]),
            (format!("{OUT}/T-3.jpg"), Some(0), DupHandling::Report)
        );
    }

    /// U2: a single stale sidecar name is enough to open the gap.
    #[test]
    fn f1_minimal_stale_sidecar_gap() {
        let mut w = World::default();
        w.occupied(OUT, "T.jpg.xmp");
        w.content(11, 1);
        w.content(12, 1);
        let a = with_sidecars(cand("/in/a.jpg", "T", 5, 11), &[SidecarForm::WithExt]);
        let b = cand("/in/b.jpg", "T", 5, 12);
        let acts = w.resolve(vec![a, b], DupPolicy::Skip);
        assert_eq!(dst(&acts[0]), format!("{OUT}/T-2.jpg"));
        assert_eq!(dup(&acts[1]).0, format!("{OUT}/T-2.jpg"));
    }

    /// U3: reverse order. The sidecar-less twin comes first and takes the
    /// lowest free slot; the other one is its duplicate and, report-only,
    /// leaves its sidecars in place.
    #[test]
    fn twin_without_sidecars_first_takes_the_slot() {
        let mut w = World::default();
        w.file(OUT, "T.jpg", 5, 90, Some(9));
        w.occupied(OUT, "T-2.jpg.xmp");
        w.content(11, 1);
        w.content(12, 1);
        let b = cand("/in/a/copy.jpg", "T", 5, 12);
        let a = with_sidecars(cand("/in/b/dto.jpg", "T", 5, 11), &[SidecarForm::WithExt]);
        let acts = w.resolve(vec![b, a], DupPolicy::Skip);
        assert_eq!(dst(&acts[0]), format!("{OUT}/T-2.jpg"));
        assert_eq!(
            dup(&acts[1]),
            (format!("{OUT}/T-2.jpg"), Some(0), DupHandling::Report)
        );
        match &acts[1] {
            Action::Duplicate { sidecars_left, .. } => {
                assert_eq!(sidecars_left, &[PathBuf::from("/in/b/dto.jpg.xmp")]);
            }
            other => panic!("{other:?}"),
        }
    }

    /// U4 (rev 1's V3; replaces `match_above_the_first_free_slot_is_ignored`).
    #[test]
    fn existing_twin_above_a_gap_is_a_duplicate() {
        let mut w = World::default();
        w.file(OUT, "x.jpg", 9, 90, Some(1));
        w.file(OUT, "x-3.jpg", 5, 93, Some(3));
        w.content(11, 3);
        let acts = w.resolve(vec![cand("/in/a.jpg", "x", 5, 11)], DupPolicy::Delete);
        assert_eq!(
            dup(&acts[0]),
            (
                format!("{OUT}/x-3.jpg"),
                None,
                DupHandling::Delete(DeleteGuard::default())
            )
        );
    }

    /// U5 (rev 1's V4): organizing in place, the OUT walk is empty, and a
    /// file at `x-3.jpg` stays there although `x-2.jpg` is free.
    #[test]
    fn inode_match_above_a_gap_is_in_place() {
        let mut w = World::default();
        w.dest
            .insert(Path::new(OUT), OsStr::new("x.jpg"), info(9, 90));
        w.dest
            .insert(Path::new(OUT), OsStr::new("x-3.jpg"), info(5, 11));
        let acts = w.resolve(
            vec![cand(&format!("{OUT}/x-3.jpg"), "x", 5, 11)],
            DupPolicy::Skip,
        );
        assert!(in_place(&acts[0]), "{acts:?}");
    }

    /// U6.
    #[test]
    fn different_content_still_takes_lowest_free_slot() {
        let mut w = World::default();
        w.file(OUT, "x.jpg", 5, 90, Some(1));
        w.file(OUT, "x-3.jpg", 5, 93, Some(3));
        w.content(11, 4);
        let acts = w.resolve(vec![cand("/in/a.jpg", "x", 5, 11)], DupPolicy::Skip);
        assert_eq!(dst(&acts[0]), format!("{OUT}/x-2.jpg"));
    }

    /// U7: global, so neither the date nor the name matters.
    #[test]
    fn identical_content_with_another_date_or_name_is_a_duplicate() {
        let mut w = World::default();
        w.content(11, 1);
        w.content(12, 1);
        let a = cand("/in/IMG_20160915_123456.jpg", "20160915-123456", 5, 11);
        let mut b = cand("/in/plain.jpg", "plain", 5, 12);
        b.target.dir = PathBuf::from("/out/unknown");
        b.target.generated = false;
        let acts = w.resolve(vec![a, b], DupPolicy::Skip);
        assert_eq!(dst(&acts[0]), format!("{OUT}/20160915-123456.jpg"));
        assert_eq!(
            dup(&acts[1]),
            (
                format!("{OUT}/20160915-123456.jpg"),
                Some(0),
                DupHandling::Report
            )
        );
    }

    /// U8.
    #[test]
    fn identical_existing_file_anywhere_in_out_is_the_keeper() {
        let mut w = World::default();
        w.file("/out/elsewhere", "old name.png", 5, 50, Some(3));
        w.content(11, 3);
        let acts = w.resolve(vec![cand("/in/a.jpg", "x", 5, 11)], DupPolicy::Skip);
        assert_eq!(
            dup(&acts[0]),
            (
                "/out/elsewhere/old name.png".into(),
                None,
                DupHandling::Report
            )
        );
    }

    /// U9.
    #[test]
    fn keeper_precedence() {
        // Existing beats in place: the input whose chain holds it is still
        // a duplicate of the existing file elsewhere in OUT.
        let mut w = World::default();
        w.dest
            .insert(Path::new(OUT), OsStr::new("x-2.jpg"), info(5, 11));
        w.file("/out/z", "k.jpg", 5, 50, Some(3));
        w.content(11, 3);
        w.content(12, 3);
        let acts = w.resolve(
            vec![cand("/in/a.jpg", "x", 5, 11), cand("/in/b.jpg", "x", 5, 12)],
            DupPolicy::Skip,
        );
        assert_eq!(dup(&acts[0]).0, "/out/z/k.jpg");
        assert_eq!(dup(&acts[1]).0, "/out/z/k.jpg");

        // In place beats the first input: b is in place, a (earlier in
        // input order) is its duplicate.
        let mut w = World::default();
        w.dest
            .insert(Path::new(OUT), OsStr::new("x-2.jpg"), info(5, 12));
        w.content(11, 3);
        w.content(12, 3);
        let acts = w.resolve(
            vec![
                cand("/in/a.jpg", "x", 5, 11),
                cand(&format!("{OUT}/x-2.jpg"), "x", 5, 12),
            ],
            DupPolicy::Skip,
        );
        assert_eq!(
            dup(&acts[0]),
            (format!("{OUT}/x-2.jpg"), None, DupHandling::Report)
        );
        assert!(in_place(&acts[1]), "{acts:?}");

        // Among in-place twins, the holder order decides, not input order:
        // the file at x.jpg stays, the one at x-2.jpg is its duplicate.
        let mut w = World::default();
        w.dest
            .insert(Path::new(OUT), OsStr::new("x-2.jpg"), info(5, 11));
        w.dest
            .insert(Path::new(OUT), OsStr::new("x.jpg"), info(5, 12));
        w.content(11, 3);
        w.content(12, 3);
        let acts = w.resolve(
            vec![
                cand(&format!("{OUT}/x-2.jpg"), "x", 5, 11),
                cand(&format!("{OUT}/x.jpg"), "x", 5, 12),
            ],
            DupPolicy::Skip,
        );
        assert_eq!(dup(&acts[0]).0, format!("{OUT}/x.jpg"));
        assert!(in_place(&acts[1]), "{acts:?}");

        // The base name beats -2 (path order alone would pick x-2.jpg).
        let mut w = World::default();
        w.file(OUT, "x-2.jpg", 5, 60, Some(3));
        w.file(OUT, "x.jpg", 5, 61, Some(3));
        w.content(11, 3);
        let acts = w.resolve(vec![cand("/in/a.jpg", "q", 5, 11)], DupPolicy::Skip);
        assert_eq!(dup(&acts[0]).0, format!("{OUT}/x.jpg"));

        // Ties are broken by folder.
        let mut w = World::default();
        w.file("/out/b", "x.jpg", 5, 70, Some(3));
        w.file("/out/a", "x.jpg", 5, 71, Some(3));
        w.content(11, 3);
        let acts = w.resolve(vec![cand("/in/a.jpg", "q", 5, 11)], DupPolicy::Skip);
        assert_eq!(dup(&acts[0]).0, "/out/a/x.jpg");
    }

    /// U11 (decision A1-Q2 = A): same inode as any OUT file, regular or
    /// symlink, is in place, even with `--delete-duplicates`.
    #[test]
    fn same_inode_as_output_is_in_place() {
        let mut w = World::default();
        w.file("/out/other", "foo.jpg", 5, 11, Some(3));
        w.out.insert_link("/out/links/bar.jpg", (1, 12));
        w.content(11, 3);
        let acts = w.resolve(
            vec![cand("/in/a.jpg", "x", 5, 11), cand("/in/b.jpg", "x", 6, 12)],
            DupPolicy::Delete,
        );
        assert!(in_place(&acts[0]) && in_place(&acts[1]), "{acts:?}");
    }

    /// U13.
    #[test]
    fn empty_files_are_never_duplicates() {
        let mut w = World::default();
        w.file(OUT, "x.jpg", 0, 90, None);
        let acts = w.resolve(
            vec![cand("/in/a.jpg", "x", 0, 11), cand("/in/b.jpg", "x", 0, 12)],
            DupPolicy::Delete,
        );
        assert_eq!(dst(&acts[0]), format!("{OUT}/x-2.jpg"));
        assert_eq!(dst(&acts[1]), format!("{OUT}/x-3.jpg"));
    }

    /// U14: no hash (the read failed) means no duplicate.
    #[test]
    fn unreadable_file_is_never_a_duplicate() {
        let mut w = World::default();
        w.file(OUT, "x.jpg", 5, 90, Some(3));
        let acts = w.resolve(vec![cand("/in/a.jpg", "x", 5, 11)], DupPolicy::Delete);
        assert_eq!(dst(&acts[0]), format!("{OUT}/x-2.jpg"));
    }

    fn dir(path: &str, depth: usize, children: usize, junk: &[&str]) -> DirInfo {
        DirInfo {
            path: path.into(),
            depth,
            children,
            junk: junk.iter().map(PathBuf::from).collect(),
            unreadable: false,
        }
    }

    fn moved(src: &str, dst: &str) -> Action {
        Action::Transfer {
            src: src.into(),
            dst: dst.into(),
            kind: Kind::Image,
            dated: true,
            size: 1,
            sidecars: vec![],
        }
    }

    #[test]
    fn rmdirs_simulation() {
        let dirs = [
            dir("/in", 0, 4, &[]),
            dir("/in/a", 1, 2, &["/in/a/.DS_Store"]), // a.jpg + junk
            dir("/in/b", 1, 2, &[]),                  // b.jpg + keep.txt
            dir("/in/c", 1, 1, &[]),                  // c/d
            dir("/in/c/d", 2, 1, &[]),                // d.jpg
            dir("/in/e", 1, 1, &[]),                  // deleted duplicate
        ];
        let deleted = Action::Duplicate {
            src: "/in/e/dup.jpg".into(),
            of: "/out/x/1.jpg".into(),
            handling: DupHandling::Delete(DeleteGuard::default()),
            depends_on: Some(0),
            size: 1,
            dev_ino: (1, 1),
            sidecars_left: vec![],
        };
        let actions = [
            moved("/in/a/a.jpg", "/out/x/1.jpg"),
            moved("/in/b/b.jpg", "/out/x/2.jpg"),
            moved("/in/c/d/d.jpg", "/out/x/3.jpg"),
            deleted,
        ];
        let got: Vec<String> = plan_rmdirs(&dirs, &actions, Path::new("/in"))
            .iter()
            .map(|r| r.path.display().to_string())
            .collect();
        assert_eq!(got, ["/in/c/d", "/in/a", "/in/c", "/in/e"]);
    }

    #[test]
    fn rmdirs_never_removes_target_folders() {
        // Organizing in place: /in/2017 receives a file and must stay.
        let dirs = [dir("/in", 0, 2, &[]), dir("/in/2017", 1, 1, &[])];
        let actions = [moved("/in/2017/a.jpg", "/in/2017/b.jpg")];
        assert!(plan_rmdirs(&dirs, &actions, Path::new("/in")).is_empty());
    }

    #[test]
    fn chain_keys_strip_numeric_suffix() {
        let keys = chain_keys(Path::new("/d"), Path::new("IMG-12.JPG"));
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[1].1, "img");
        assert_eq!(chain_keys(Path::new("/d"), Path::new("a-b.jpg")).len(), 1);
    }

    const DUPS: &str = "/out/duplicates";

    /// A report-only duplicate as `resolve` leaves it for step 4e.
    fn pending_dup(src: &str, ino: u64, sidecars: &[&str]) -> Action {
        Action::Duplicate {
            src: src.into(),
            of: format!("{OUT}/x.jpg").into(),
            handling: DupHandling::Report,
            depends_on: Some(0),
            size: 5,
            dev_ino: (1, ino),
            sidecars_left: sidecars.iter().map(PathBuf::from).collect(),
        }
    }

    fn file_all(w: &World, actions: Vec<Action>, check_filed: bool) -> Vec<Action> {
        file_duplicates(
            actions,
            &DupDir {
                input: Path::new("/in"),
                dir: Path::new(DUPS),
                index: &w.dest,
                hashes: &w.hashes,
                check_filed,
            },
        )
    }

    fn filed(a: &Action) -> (String, Vec<String>) {
        match a {
            Action::Duplicate {
                handling: DupHandling::File { dst, sidecars },
                sidecars_left,
                ..
            } => {
                assert!(sidecars_left.is_empty(), "sidecars travel with it");
                let side = sidecars
                    .iter()
                    .map(|s| s.dst.display().to_string())
                    .collect();
                (dst.display().to_string(), side)
            }
            other => panic!("expected a filed duplicate, got {other:?}"),
        }
    }

    /// U19.
    #[test]
    fn dupdir_placement() {
        let d = format!("{DUPS}/a/b");
        // The relative path and the original name are kept.
        let w = World::default();
        let acts = file_all(
            &w,
            vec![pending_dup(
                "/in/a/b/IMG_1.JPG",
                11,
                &["/in/a/b/IMG_1.JPG.xmp", "/in/a/b/IMG_1.XMP"],
            )],
            true,
        );
        assert_eq!(
            filed(&acts[0]),
            (
                format!("{d}/IMG_1.JPG"),
                vec![format!("{d}/IMG_1.JPG.xmp"), format!("{d}/IMG_1.XMP")]
            )
        );

        // A different file at the name gives -2 for the whole bundle; a
        // stale sidecar name there (a gap) bumps it to -3.
        let mut w = World::default();
        w.dest
            .insert(Path::new(&d), OsStr::new("IMG_1.JPG"), info(9, 90));
        w.dest
            .insert(Path::new(&d), OsStr::new("IMG_1-2.JPG.xmp"), None);
        w.content(90, 1);
        w.content(11, 2);
        let acts = file_all(
            &w,
            vec![pending_dup(
                "/in/a/b/IMG_1.JPG",
                11,
                &["/in/a/b/IMG_1.JPG.xmp"],
            )],
            true,
        );
        assert_eq!(
            filed(&acts[0]),
            (
                format!("{d}/IMG_1-3.JPG"),
                vec![format!("{d}/IMG_1-3.JPG.xmp")]
            )
        );

        // Already filed, by inode or by content, at any chain position:
        // nothing to write in copy-like modes, and sidecars stay.
        let mut w = World::default();
        w.dest
            .insert(Path::new(&d), OsStr::new("IMG_1.JPG"), info(5, 90));
        w.dest
            .insert(Path::new(&d), OsStr::new("IMG_1-4.JPG"), info(5, 94));
        w.dest
            .insert(Path::new(&d), OsStr::new("IMG_1-7.JPG"), info(5, 12));
        w.content(90, 1);
        w.content(94, 3);
        w.content(11, 3);
        let acts = file_all(
            &w,
            vec![
                pending_dup("/in/a/b/IMG_1.JPG", 11, &["/in/a/b/IMG_1.JPG.xmp"]),
                pending_dup("/in/a/b/img_1.jpg", 12, &[]),
            ],
            true,
        );
        match &acts[0] {
            Action::Duplicate {
                handling: DupHandling::AlreadyFiled { at },
                sidecars_left,
                ..
            } => {
                assert_eq!(at, &PathBuf::from(format!("{d}/IMG_1-4.JPG")));
                assert_eq!(sidecars_left.len(), 1);
            }
            other => panic!("{other:?}"),
        }
        assert!(
            matches!(&acts[1], Action::Duplicate { handling: DupHandling::AlreadyFiled { at }, .. } if at == &PathBuf::from(format!("{d}/IMG_1-7.JPG"))),
            "{:?}",
            acts[1]
        );

        // A move always files (it costs no space and empties the input).
        let acts = file_all(&w, vec![pending_dup("/in/a/b/IMG_1.JPG", 11, &[])], false);
        assert_eq!(filed(&acts[0]).0, format!("{d}/IMG_1-2.JPG"));

        // Other actions pass through untouched.
        let t = moved("/in/x.jpg", "/out/x.jpg");
        assert_eq!(file_all(&w, vec![t.clone()], true), [t]);
    }

    /// U20: a `--date-format` that renders into the duplicates folder.
    #[test]
    fn target_inside_dupdir_fails() {
        let d = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(d.path()).unwrap();
        fs::create_dir(root.join("in")).unwrap();
        let cfg = crate::cli::parse_from([
            "oxisort",
            "-d",
            "Duplicates/YYYY",
            root.join("in").to_str().unwrap(),
            root.join("out").to_str().unwrap(),
        ])
        .unwrap()
        .into_config()
        .unwrap();
        assert!(inside_dup_dir(&root.join("out/Duplicates/2017"), &cfg));
        assert!(!inside_dup_dir(&root.join("out/2017/duplicates"), &cfg));
        assert!(!inside_dup_dir(&root.join("out/unknown"), &cfg));
    }

    /// U21.
    #[test]
    fn rmdirs_counts_filed_duplicates_and_protects_dupdir() {
        let dirs = [
            dir("/in", 0, 3, &[]),
            dir("/in/a", 1, 2, &[]),          // x.jpg + x.jpg.xmp, both filed
            dir("/in/duplicates", 1, 1, &[]), // in place: the dupdir itself
            dir("/in/duplicates/a", 2, 1, &[]), // an earlier filing
            dir("/in/b", 1, 1, &[]),          // a report-only duplicate stays
        ];
        let filed = Action::Duplicate {
            src: "/in/a/x.jpg".into(),
            of: "/in/2017/x.jpg".into(),
            handling: DupHandling::File {
                dst: "/in/duplicates/a/x-2.jpg".into(),
                sidecars: vec![PlannedSidecar {
                    src: "/in/a/x.jpg.xmp".into(),
                    dst: "/in/duplicates/a/x-2.jpg.xmp".into(),
                }],
            },
            depends_on: None,
            size: 1,
            dev_ino: (1, 1),
            sidecars_left: vec![],
        };
        let reported = Action::Duplicate {
            src: "/in/b/y.jpg".into(),
            of: "/in/2017/x.jpg".into(),
            handling: DupHandling::Report,
            depends_on: None,
            size: 1,
            dev_ino: (1, 2),
            sidecars_left: vec![],
        };
        let got: Vec<String> = plan_rmdirs(&dirs, &[filed, reported], Path::new("/in"))
            .iter()
            .map(|r| r.path.display().to_string())
            .collect();
        assert_eq!(got, ["/in/a"]);
    }

    #[test]
    fn sidecar_form_is_recovered_from_names() {
        let f = |m: &str, s: &str| sidecar_form(Path::new(m), Path::new(s));
        assert_eq!(f("/i/A.JPG", "/i/A.JPG.xmp"), SidecarForm::WithExt);
        assert_eq!(f("/i/A.JPG", "/i/a.jpg.XMP"), SidecarForm::WithExt);
        assert_eq!(f("/i/A.JPG", "/i/A.xmp"), SidecarForm::StemOnly);
    }

    // ---- U17 / U18: seeded differential test against a brute-force
    // reference that knows nothing about chains, classes or hashes.

    /// SplitMix64: a tiny deterministic generator (no extra dependency).
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
        fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
            &items[self.below(items.len() as u64) as usize]
        }
        fn chance(&mut self, percent: u64) -> bool {
            self.below(100) < percent
        }
    }

    /// Content id → size. 0 is empty; 3–6 are "large" (two-stage hash),
    /// with 4 and 5 sharing a partial hash so stage 2 has to tell them apart.
    const SIZES: [u64; 7] = [0, 5, 5, 200_000, 200_000, 200_000, 200_000];
    const D: &str = OUT;
    const E: &str = "/out/other";

    #[derive(Debug, Clone, Copy)]
    struct Obj {
        ino: u64,
        cid: usize,
        readable: bool,
    }

    #[derive(Debug, Clone)]
    struct Ex {
        dir: &'static str,
        name: String,
        obj: Obj,
        symlink: bool,
        in_out: bool,
    }

    impl Ex {
        fn path(&self) -> PathBuf {
            Path::new(self.dir).join(&self.name)
        }
    }

    #[derive(Debug, Clone)]
    struct Case {
        existing: Vec<Ex>,
        cands: Vec<(Candidate, Obj)>,
        policy: DupPolicy,
    }

    fn gen_case(seed: u64) -> Case {
        let mut rng = Rng(seed);
        let mut next_ino = 100;
        let mut fresh = |rng: &mut Rng| {
            next_ino += 1;
            Obj {
                ino: next_ino,
                cid: rng.below(7) as usize,
                readable: !rng.chance(10),
            }
        };
        // In-place runs: INPUT == OUT, so the OUT walk is empty and the
        // existing names are only seen through the target folders.
        let in_out = !rng.chance(25);
        let mut cands: Vec<(Candidate, Obj)> = Vec::new();
        for i in 0..1 + rng.below(6) {
            let obj = if !cands.is_empty() && rng.chance(15) {
                rng.pick(&cands).1 // hard link to an earlier input
            } else if !cands.is_empty() && rng.chance(30) {
                // Same content as an earlier input, another inode.
                let twin = rng.pick(&cands).1;
                Obj {
                    ino: fresh(&mut rng).ino,
                    ..twin
                }
            } else {
                fresh(&mut rng)
            };
            let stem = *rng.pick(&["x", "X", "x-2", "y"]);
            let ext = *rng.pick(&[Some("jpg"), Some("JPG"), None]);
            let mut forms = Vec::new();
            if rng.chance(30) {
                forms.push(SidecarForm::WithExt);
            }
            if rng.chance(30) {
                forms.push(SidecarForm::StemOnly);
            }
            let mut c = with_sidecars(
                cand(&format!("/in/{i}.jpg"), stem, SIZES[obj.cid], obj.ino),
                &forms,
            );
            c.dev_ino = (1, obj.ino);
            c.mtime = Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(i));
            c.target.ext = ext.map(OsString::from);
            c.target.generated = rng.chance(50);
            if rng.chance(20) {
                c.target.dir = PathBuf::from(E);
            }
            cands.push((c, obj));
        }
        let names = [
            "x.jpg",
            "X.JPG",
            "x-2.jpg",
            "x-3.jpg",
            "x-2-2.jpg",
            "x",
            "x-2",
            "x.jpg.xmp",
            "x-2.jpg.xmp",
            "x.xmp",
            "x-2.xmp",
            "y.jpg",
            "x-02.jpg",
            "q.png",
        ];
        let mut existing: Vec<Ex> = Vec::new();
        for _ in 0..rng.below(9) {
            let dir = if rng.chance(75) { D } else { E };
            let name = (*rng.pick(&names)).to_owned();
            if existing
                .iter()
                .any(|e| e.dir == dir && e.name.to_lowercase() == name.to_lowercase())
            {
                continue;
            }
            let obj = if rng.chance(40) {
                rng.pick(&cands).1 // hard link (or symlink target) to an input
            } else {
                fresh(&mut rng)
            };
            existing.push(Ex {
                dir,
                name,
                obj,
                symlink: rng.chance(15),
                in_out,
            });
        }
        let policy = *rng.pick(&[DupPolicy::Skip, DupPolicy::Delete, DupPolicy::File]);
        Case {
            existing,
            cands,
            policy,
        }
    }

    /// Builds the plan's inputs, hashing through the real size grouping and
    /// two-stage driver with fake readers. `reverse` inserts everything in
    /// the opposite order (U18).
    fn run_case(case: &Case, pool: &rayon::ThreadPool, reverse: bool) -> Vec<Action> {
        let mut existing = case.existing.clone();
        if reverse {
            existing.reverse();
        }
        let mut w = World::default();
        let mut objs: HashMap<u64, Obj> = HashMap::new();
        for e in &existing {
            objs.insert(e.obj.ino, e.obj);
            let fi = FileInfo {
                size: SIZES[e.obj.cid],
                dev_ino: (1, e.obj.ino),
                symlink: e.symlink,
            };
            w.dest
                .insert(Path::new(e.dir), OsStr::new(&e.name), Some(fi));
            if e.in_out && e.symlink {
                w.out.insert_link(e.path(), (1, e.obj.ino));
            } else if e.in_out {
                w.out
                    .insert_file(e.path(), SIZES[e.obj.cid], (1, e.obj.ino));
            }
        }
        for (_, o) in &case.cands {
            objs.insert(o.ino, *o);
        }
        let mut members: Vec<Member<'_>> = case
            .cands
            .iter()
            .map(|(c, _)| Member {
                path: &c.src,
                size: c.size,
                dev_ino: c.dev_ino,
                candidate: true,
            })
            .collect();
        members.extend(w.out.files().iter().map(|f| Member {
            path: &f.path,
            size: f.size,
            dev_ino: f.dev_ino,
            candidate: false,
        }));
        let read = |r: &Rep, partial: bool| {
            let o = objs[&r.dev_ino.1];
            let tag = if partial { o.cid / 2 } else { o.cid };
            o.readable
                .then(|| blake3::hash(&[tag as u8, partial as u8]))
        };
        let (hashes, _) = hash::two_stage(
            dedupe::size_groups(&members),
            |r, _| read(r, true),
            |r, _| read(r, false),
            pool,
            &Stop::new(),
        );
        w.hashes = hashes;
        let cands = case.cands.iter().map(|(c, _)| c.clone()).collect();
        w.resolve(cands, case.policy)
    }

    /// The rules of amendment A1, stated directly: pairwise comparison, a
    /// linear search for names.
    ///
    /// Naming and the keeper order are restated here too, not borrowed
    /// from production (`numbered_name`, `sidecar_name`, `order_key`), so a
    /// bug there can't hide in both sides.
    fn reference(case: &Case) -> Vec<Action> {
        let same = |a: &Obj, b: &Obj| {
            a.ino == b.ino || (a.readable && b.readable && a.cid == b.cid && SIZES[a.cid] > 0)
        };
        let folded = |s: &OsStr| s.to_string_lossy().to_lowercase();
        // `stem[-n][.ext]`.
        let name_n = |stem: &OsStr, ext: Option<&OsStr>, n: u32| -> OsString {
            let mut s = stem.to_string_lossy().into_owned();
            if n > 1 {
                s += &format!("-{n}");
            }
            if let Some(e) = ext {
                s += &format!(".{}", e.to_string_lossy());
            }
            s.into()
        };
        // `(folder, n, name)`: n from a `-<n>` stem suffix (n >= 2, no
        // leading zero, non-empty base), else 1.
        let order = |p: &Path| -> (PathBuf, u32, OsString) {
            let stem = p.file_stem().unwrap().to_string_lossy().into_owned();
            let n = match stem.rsplit_once('-') {
                Some((base, d)) if !base.is_empty() && !d.starts_with('0') => {
                    d.parse::<u32>().ok().filter(|&n| n >= 2).unwrap_or(1)
                }
                _ => 1,
            };
            let parent = p.parent().unwrap().to_path_buf();
            (parent, n, p.file_name().unwrap().to_owned())
        };
        // The existing path in c's own chain holding c's inode, lowest n.
        let chain_holder = |c: &Candidate, o: &Obj| -> Option<PathBuf> {
            (1..=60u32).find_map(|n| {
                let name = folded(&name_n(&c.target.stem, c.target.ext.as_deref(), n));
                case.existing
                    .iter()
                    .filter(|e| Path::new(e.dir) == c.target.dir && e.obj.ino == o.ino)
                    .filter(|e| e.name.to_lowercase() == name)
                    .map(Ex::path)
                    .min()
            })
        };
        let out_holder = |o: &Obj| -> Option<PathBuf> {
            case.existing
                .iter()
                .filter(|e| e.in_out && e.obj.ino == o.ino)
                .map(Ex::path)
                .min()
        };
        // `keeper`: its inode and plan-time mtime (None for an OUT file
        // from `insert_file`, or one this run creates).
        let handling = |c: &Candidate, o: &Obj, keeper: (u64, Option<SystemTime>)| {
            match case.policy {
                // Step 4e (filing) is checked separately, in U17's test.
                DupPolicy::File | DupPolicy::Skip => DupHandling::Report,
                DupPolicy::Delete if o.ino == keeper.0 => DupHandling::KeepSameFile,
                DupPolicy::Delete => DupHandling::Delete(DeleteGuard {
                    src_mtime: c.mtime,
                    of_mtime: keeper.1,
                }),
            }
        };
        let mut planned: Vec<(PathBuf, String)> = Vec::new();
        let mut out: Vec<Action> = Vec::new();
        for (i, (c, o)) in case.cands.iter().enumerate() {
            let dup = |of: PathBuf, keeper, depends_on| Action::Duplicate {
                src: c.src.clone(),
                of,
                handling: handling(c, o, keeper),
                depends_on,
                size: c.size,
                dev_ino: c.dev_ino,
                sidecars_left: c.sidecars.iter().map(|s| s.path.clone()).collect(),
            };
            let in_place = Action::AlreadyInPlace { src: c.src.clone() };
            if out_holder(o).is_some() {
                out.push(in_place);
                continue;
            }
            let nonempty = SIZES[o.cid] > 0;
            if !nonempty && chain_holder(c, o).is_some() {
                out.push(in_place);
                continue;
            }
            if nonempty {
                // Rule 1.
                let existing = case
                    .existing
                    .iter()
                    .filter(|e| e.in_out && !e.symlink && same(&e.obj, o))
                    .min_by_key(|e| order(&e.path()));
                if let Some(e) = existing {
                    out.push(dup(e.path(), (e.obj.ino, None), None));
                    continue;
                }
                // Rule 2.
                let in_place_keeper = case
                    .cands
                    .iter()
                    .enumerate()
                    .filter(|(_, (_, oj))| same(oj, o))
                    .filter_map(|(j, (cj, oj))| {
                        let h = out_holder(oj).or_else(|| chain_holder(cj, oj))?;
                        Some(((order(&h), cj.src != h, j), j))
                    })
                    .min();
                if let Some((_, j)) = in_place_keeper {
                    let (cj, oj) = &case.cands[j];
                    out.push(if j == i {
                        in_place
                    } else {
                        dup(cj.src.clone(), (oj.ino, cj.mtime), None)
                    });
                    continue;
                }
                // Rule 3.
                let first = (0..i).find(|&j| {
                    same(&case.cands[j].1, o) && matches!(out[j], Action::Transfer { .. })
                });
                if let Some(j) = first {
                    let Action::Transfer { dst, .. } = &out[j] else {
                        unreachable!()
                    };
                    out.push(dup(dst.clone(), (case.cands[j].1.ino, None), Some(j)));
                    continue;
                }
            }
            // Naming: the lowest n whose whole bundle is free.
            let t = &c.target;
            let taken = |name: &OsStr, planned: &[(PathBuf, String)]| {
                let f = folded(name);
                case.existing
                    .iter()
                    .any(|e| Path::new(e.dir) == t.dir && e.name.to_lowercase() == f)
                    || planned.iter().any(|(d, n)| *d == t.dir && *n == f)
            };
            let (name, sides) = (1..)
                .find_map(|n| {
                    let name = name_n(&t.stem, t.ext.as_deref(), n);
                    // The generator's sidecars are all `….xmp`, so generated
                    // and kept spellings agree.
                    let sides: Vec<OsString> = c
                        .sidecars
                        .iter()
                        .map(|s| {
                            let base = match s.form {
                                SidecarForm::WithExt => name.clone(),
                                SidecarForm::StemOnly => name_n(&t.stem, None, n),
                            };
                            format!("{}.xmp", base.to_string_lossy()).into()
                        })
                        .collect();
                    let free = !taken(&name, &planned) && !sides.iter().any(|s| taken(s, &planned));
                    free.then_some((name, sides))
                })
                .unwrap();
            planned.push((t.dir.clone(), folded(&name)));
            planned.extend(sides.iter().map(|s| (t.dir.clone(), folded(s))));
            out.push(Action::Transfer {
                src: c.src.clone(),
                dst: t.dir.join(&name),
                kind: c.kind,
                dated: c.dated,
                size: c.size,
                sidecars: c
                    .sidecars
                    .iter()
                    .zip(sides)
                    .map(|(s, n)| PlannedSidecar {
                        src: s.path.clone(),
                        dst: t.dir.join(n),
                    })
                    .collect(),
            });
        }
        out
    }

    fn one_thread() -> rayon::ThreadPool {
        rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .unwrap()
    }

    #[test]
    fn duplicate_outside_input_fails_instead_of_filing_next_to_itself() {
        let index = DestIndex::default();
        let hashes = HashMap::new();
        let dd = DupDir {
            input: Path::new("/in"),
            dir: Path::new("/out/duplicates"),
            index: &index,
            hashes: &hashes,
            check_filed: false,
        };
        let dup = Action::Duplicate {
            src: "/elsewhere/a.jpg".into(),
            of: "/out/x.jpg".into(),
            handling: DupHandling::Report,
            depends_on: None,
            size: 5,
            dev_ino: (1, 2),
            sidecars_left: vec![],
        };
        match &file_duplicates(vec![dup], &dd)[0] {
            Action::Fail { src, error } => {
                assert_eq!(src, Path::new("/elsewhere/a.jpg"));
                assert!(error.contains("outside the input folder"), "{error}");
            }
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    /// U17, default-policy part: files every report-only duplicate of `acts`
    /// into a duplicates folder that already holds some names, and checks
    /// the placement rules directly. Returns how many were filed.
    fn check_filing(acts: &[Action], seed: u64) -> usize {
        const DUP: &str = "/out/duplicates";
        let mut rng = Rng(seed ^ 0x5eed);
        let pool = ["0.jpg", "0-2.jpg", "1.JPG", "0.xmp", "0.jpg.xmp", "2-2.jpg"];
        let mut index = DestIndex::default();
        let mut existing: HashSet<String> = HashSet::new();
        for name in pool {
            if rng.chance(40) {
                index.insert(Path::new(DUP), OsStr::new(name), None);
                existing.insert(name.to_lowercase());
            }
        }
        let hashes = HashMap::new();
        let dd = DupDir {
            input: Path::new("/in"),
            dir: Path::new(DUP),
            index: &index,
            hashes: &hashes,
            check_filed: false,
        };
        let filed = file_duplicates(acts.to_vec(), &dd);
        let mut names: HashSet<String> = HashSet::new();
        let mut n = 0;
        for (before, after) in acts.iter().zip(&filed) {
            let Action::Duplicate {
                src,
                handling: DupHandling::Report,
                ..
            } = before
            else {
                assert_eq!(before, after, "seed {seed}: only duplicates are filed");
                continue;
            };
            let Action::Duplicate {
                handling: DupHandling::File { dst, sidecars },
                sidecars_left,
                ..
            } = after
            else {
                panic!("seed {seed}: {src:?} not filed: {after:?}");
            };
            assert!(sidecars_left.is_empty(), "seed {seed}: sidecars go along");
            let stem = src.file_stem().unwrap().to_string_lossy().into_owned();
            let main = dst.file_name().unwrap().to_string_lossy().into_owned();
            assert!(
                main.starts_with(&stem),
                "seed {seed}: keeps its name: {main}"
            );
            for p in std::iter::once(dst).chain(sidecars.iter().map(|s| &s.dst)) {
                assert_eq!(p.parent(), Some(Path::new(DUP)), "seed {seed}");
                let name = p.file_name().unwrap().to_string_lossy().to_lowercase();
                assert!(!existing.contains(&name), "seed {seed}: reused {name}");
                assert!(names.insert(name.clone()), "seed {seed}: {name} twice");
            }
            n += 1;
        }
        n
    }

    /// U17.
    #[test]
    fn resolver_matches_reference_rule() {
        let pool = one_thread();
        let (mut dups, mut filed) = (0, 0);
        for seed in 0..3_000 {
            let case = gen_case(seed);
            let got = run_case(&case, &pool, false);
            let want = reference(&case);
            assert_eq!(got, want, "seed {seed}: {case:#?}");
            dups += got
                .iter()
                .filter(|a| matches!(a, Action::Duplicate { .. }))
                .count();
            if case.policy == DupPolicy::File {
                filed += check_filing(&got, seed);
            }
        }
        assert!(dups > 500, "the generator must produce duplicates ({dups})");
        assert!(filed > 100, "the generator must file duplicates ({filed})");
    }

    /// U18.
    #[test]
    fn resolve_is_independent_of_index_insertion_order() {
        let pool = one_thread();
        for seed in 0..1_000 {
            let case = gen_case(seed);
            assert_eq!(
                run_case(&case, &pool, false),
                run_case(&case, &pool, true),
                "seed {seed}"
            );
        }
    }
}
