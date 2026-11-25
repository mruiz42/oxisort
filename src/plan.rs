//! Phase 4: decide every target path, single-threaded and in input order,
//! so the result is deterministic and dry-run shows exactly what a real run
//! does. Worker threads later only execute these decisions.
//!
//! Steps (plan 4a–4d):
//! - 4a: target folder and base name per file ([`target_for`]);
//! - 4b: list each target folder once ([`DestIndex`]), stat the existing
//!   files that share a name chain with some input;
//! - 4c: hash (blake3) only files whose size collides within a chain;
//! - 4d: walk the inputs in order and give each the first free
//!   `stem[-n].ext`, detecting duplicates and files already in place.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use rayon::prelude::*;

use crate::Pools;
use crate::exec::Stop;
use crate::extract::{ExtractError, Extracted};
use crate::hash;
use crate::model::{Action, Config, Kind, Mode, PlannedSidecar, SkipReason};
use crate::scan::{DirInfo, Sidecar, SidecarForm, is_junk};

/// The complete plan for a run.
#[derive(Debug, Default)]
pub struct Plan {
    pub actions: Vec<Action>,
    /// Source folders `--rmdirs` will remove, deepest first.
    pub rmdirs: Vec<RmDir>,
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

    /// Stats every existing file in a chain some candidate belongs to.
    fn stat_chains(&mut self, cands: &[&Candidate], pool: &rayon::ThreadPool) {
        let mut wanted: HashSet<(PathBuf, String)> = HashSet::new();
        for c in cands {
            for key in candidate_keys(c) {
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

fn candidate_keys(c: &Candidate) -> Vec<ChainKey> {
    let name = numbered_name(&c.target.stem, c.target.ext.as_deref(), 1);
    chain_keys(&c.target.dir, Path::new(&name))
}

/// Builds the plan from extraction results (in scan order).
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
                None => Ok(candidate(x, cfg)),
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
    index.stat_chains(&cands, &pools.cpu);
    // Hashing reads whole files, so it runs on the io pool: `--io-jobs`
    // bounds concurrent full reads and spinning disks or SD cards aren't
    // thrashed (listing and stat above are metadata-only).
    let hashes = hash::hash_all(&hash_set(&cands, &index), &pools.io, stop);

    let delete = cfg.delete_duplicates && cfg.mode == Mode::Move;
    let actions = resolve(staged, &index, &hashes, delete);
    let rmdirs = if cfg.rmdirs && cfg.mode == Mode::Move {
        plan_rmdirs(dirs_scanned, &actions, &cfg.input)
    } else {
        Vec::new()
    };
    Plan { actions, rmdirs }
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
/// junk once every planned move and duplicate deletion has happened.
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
                src, delete: true, ..
            } => count_removed(src),
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
        kind: x.kind,
        dated: x.date.is_some(),
        target,
        src: x.entry.path,
        sidecars: x.entry.sidecars,
    }
}

/// Plan step 4c: the files worth hashing. Within each chain, group members
/// (existing regular files and inputs) by size; a group that holds an input
/// and at least two distinct files is hashed in full. Cheap
/// over-approximation, deterministic.
fn hash_set(cands: &[&Candidate], index: &DestIndex) -> Vec<PathBuf> {
    type Group = (Vec<PathBuf>, HashSet<(u64, u64)>, bool);
    let mut groups: HashMap<(ChainKey, u64), Group> = HashMap::new();
    let mut seen_keys: HashSet<ChainKey> = HashSet::new();
    for c in cands {
        for key in candidate_keys(c) {
            let g = groups.entry((key.clone(), c.size)).or_default();
            g.0.push(c.src.clone());
            g.1.insert(c.dev_ino);
            g.2 = true;
            seen_keys.insert(key);
        }
    }
    for key in seen_keys {
        for name in index.chains.get(&key).into_iter().flatten() {
            let Some(e) = index.get(&key.0, name) else {
                continue;
            };
            let Some(info) = e.info.filter(|i| !i.symlink) else {
                continue;
            };
            if let Some(g) = groups.get_mut(&(key.clone(), info.size)) {
                g.0.push(e.path.clone());
                g.1.insert(info.dev_ino);
            }
        }
    }
    let mut paths: Vec<PathBuf> = groups
        .into_values()
        .filter(|(_, ids, has_input)| *has_input && ids.len() >= 2)
        .flat_map(|(paths, _, _)| paths)
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

/// Something an input can turn out to be a copy of: the holder of name
/// `stem[-n].ext` in some chain.
#[derive(Debug, Clone)]
enum Holder {
    /// A transfer planned earlier in this run.
    Planned { task: usize, dst: PathBuf },
    /// A file already in the target folder.
    Existing { path: PathBuf },
}

/// Holders at their chain positions `n`.
type Positions = Vec<(u32, Holder)>;

/// Holders keyed by what makes them match an input, so a candidate finds
/// its duplicate in O(1) instead of walking its whole name chain. Each
/// entry records the holder's position `n` in the chain.
#[derive(Debug, Default)]
struct MatchIndex {
    by_ino: HashMap<(ChainKey, (u64, u64)), Positions>,
    /// Equal hashes imply equal content and size, which is finer than the
    /// size lookup the review asked for: only true duplicates are listed.
    by_hash: HashMap<(ChainKey, blake3::Hash), Positions>,
}

impl MatchIndex {
    fn add(
        &mut self,
        dir: &Path,
        name: &OsStr,
        dev_ino: (u64, u64),
        hash: Option<blake3::Hash>,
        holder: &Holder,
    ) {
        for (key, n) in chain_positions(dir, Path::new(name)) {
            if let Some(h) = hash {
                self.by_hash
                    .entry((key.clone(), h))
                    .or_default()
                    .push((n, holder.clone()));
            }
            self.by_ino
                .entry((key, dev_ino))
                .or_default()
                .push((n, holder.clone()));
        }
    }

    /// The match the original chain walk would stop at: the lowest position
    /// below `limit` (the free slot), an inode match winning a tie.
    fn first(
        &self,
        key: &ChainKey,
        c: &Candidate,
        hash: Option<blake3::Hash>,
        limit: u32,
    ) -> Option<(bool, &Holder)> {
        let ino = self
            .by_ino
            .get(&(key.clone(), c.dev_ino))
            .into_iter()
            .flatten()
            .map(|(n, h)| ((*n, 0u8), (true, h)));
        let content = hash
            .and_then(|h| self.by_hash.get(&(key.clone(), h)))
            .into_iter()
            .flatten()
            .map(|(n, h)| ((*n, 1u8), (false, h)));
        ino.chain(content)
            .filter(|((n, _), _)| *n < limit)
            .min_by_key(|(rank, _)| *rank)
            .map(|(_, m)| m)
    }
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

/// Plan step 4d state. Correctness (no two tasks share a name, no existing
/// name is reused) rests on `planned` + the `DestIndex` alone; `cursors`
/// and `matches` only make the search fast. If a lookup key ever disagreed
/// with the name map (exotic case folding), the effect is a missed
/// duplicate, i.e. an extra `-n` copy, never an overwrite.
struct Resolver<'a> {
    index: &'a DestIndex,
    hashes: &'a HashMap<PathBuf, blake3::Hash>,
    delete: bool,
    /// Every name planned so far (main files and sidecars), case-folded.
    planned: HashSet<(PathBuf, String)>,
    /// Per chain: every `n` below the cursor is known to be occupied.
    /// Occupancy only grows, so the cursor never has to move back.
    cursors: HashMap<ChainKey, u32>,
    matches: MatchIndex,
}

/// Plan step 4d (pure): assigns final names in input order.
pub(crate) fn resolve(
    staged: Vec<Result<Candidate, Action>>,
    index: &DestIndex,
    hashes: &HashMap<PathBuf, blake3::Hash>,
    delete_duplicates: bool,
) -> Vec<Action> {
    let mut matches = MatchIndex::default();
    for existing in index.dirs.values().flat_map(HashMap::values) {
        let (Some(info), Some(dir), Some(name)) = (
            existing.info,
            existing.path.parent(),
            existing.path.file_name(),
        ) else {
            continue;
        };
        // A symlink is never a duplicate copy, but a link to the input
        // still counts as already in place (inode match).
        let hash = (!info.symlink)
            .then(|| hashes.get(&existing.path).copied())
            .flatten();
        let holder = Holder::Existing {
            path: existing.path.clone(),
        };
        matches.add(dir, name, info.dev_ino, hash, &holder);
    }
    let mut r = Resolver {
        index,
        hashes,
        delete: delete_duplicates,
        planned: HashSet::new(),
        cursors: HashMap::new(),
        matches,
    };
    staged
        .into_iter()
        .enumerate()
        .map(|(task, s)| match s {
            Ok(c) => r.place(task, c),
            Err(action) => action,
        })
        .collect()
}

impl Resolver<'_> {
    fn taken(&self, dir: &Path, folded: &str) -> bool {
        self.index.get(dir, folded).is_some()
            || self
                .planned
                .contains(&(dir.to_path_buf(), folded.to_owned()))
    }

    /// Equivalent to walking `n = 1, 2, …` and stopping at the first name
    /// that is either a copy of `c` or free for its whole bundle (main
    /// name plus sidecars): find the free slot from the chain's cursor,
    /// then take any match below it.
    fn place(&mut self, task: usize, c: Candidate) -> Action {
        let t = &c.target;
        let first = numbered_name(&t.stem, t.ext.as_deref(), 1);
        let (key, _) = chain_positions(&t.dir, Path::new(&first)).swap_remove(0);
        let Some((n, name, side_names)) = self.free_slot(&key, &c) else {
            return Action::Fail {
                error: format!("no free name left for {}", t.dir.join(&t.stem).display()),
                src: c.src,
            };
        };
        let hash = self.hashes.get(&c.src).copied();
        if let Some((by_inode, holder)) = self.matches.first(&key, &c, hash, n) {
            return match holder.clone() {
                Holder::Existing { .. } if by_inode => Action::AlreadyInPlace { src: c.src },
                Holder::Existing { path } => duplicate(c, path, self.delete, None),
                // Same inode (hard links in the input): never deleted.
                Holder::Planned { task, dst } if by_inode => duplicate(c, dst, false, Some(task)),
                Holder::Planned { task, dst } => duplicate(c, dst, self.delete, Some(task)),
            };
        }
        let dst = t.dir.join(&name);
        self.planned.insert((t.dir.clone(), fold(&name)));
        for s in &side_names {
            self.planned.insert((t.dir.clone(), fold(s)));
        }
        let holder = Holder::Planned {
            task,
            dst: dst.clone(),
        };
        self.matches.add(&t.dir, &name, c.dev_ino, hash, &holder);
        let sidecars = c
            .sidecars
            .iter()
            .zip(side_names)
            .map(|(s, n)| PlannedSidecar {
                src: s.path.clone(),
                dst: t.dir.join(n),
            })
            .collect();
        Action::Transfer {
            src: c.src,
            dst,
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
    fn free_slot(
        &mut self,
        key: &ChainKey,
        c: &Candidate,
    ) -> Option<(u32, OsString, Vec<OsString>)> {
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
                    return Some((n, name, side_names));
                }
            }
            n = n.checked_add(1)?;
        }
    }
}

fn duplicate(c: Candidate, of: PathBuf, delete: bool, depends_on: Option<usize>) -> Action {
    Action::Duplicate {
        of,
        delete,
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

/// Target name of a sidecar, derived from the final main name (fixes
/// phockup's `….jpg-2.xmp`): `name.ext.xmp` → `<main name>.xmp`,
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

/// Every distinct folder that transfers write into, sorted.
pub fn target_dirs(plan: &Plan) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = plan
        .actions
        .iter()
        .filter_map(|a| match a {
            Action::Transfer { dst, .. } => dst.parent().map(Path::to_path_buf),
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

    const OUT: &str = "/out/2017/01/02";

    fn cand(src: &str, stem: &str, size: u64, ino: u64) -> Candidate {
        Candidate {
            src: PathBuf::from(src),
            size,
            dev_ino: (1, ino),
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

    fn hashes(pairs: &[(&str, u8)]) -> HashMap<PathBuf, blake3::Hash> {
        pairs
            .iter()
            .map(|(p, h)| (PathBuf::from(p), blake3::hash(&[*h])))
            .collect()
    }

    fn info(size: u64, ino: u64) -> Option<FileInfo> {
        Some(FileInfo {
            size,
            dev_ino: (1, ino),
            symlink: false,
        })
    }

    fn dst(a: &Action) -> String {
        match a {
            Action::Transfer { dst, .. } => dst.display().to_string(),
            other => panic!("expected transfer, got {other:?}"),
        }
    }

    #[test]
    fn collision_chain() {
        let staged = vec![
            Ok(cand("/in/a.jpg", "x", 1, 11)),
            Ok(cand("/in/b.jpg", "x", 2, 12)),
            Ok(cand("/in/c.jpg", "X", 3, 13)), // case-folded collision
        ];
        let acts = resolve(staged, &DestIndex::default(), &HashMap::new(), false);
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
        let mut index = DestIndex::default();
        index.insert(Path::new(OUT), OsStr::new("x.jpg"), info(5, 99));
        let h = hashes(&[(&format!("{OUT}/x.jpg"), 7), ("/in/a.jpg", 7)]);
        let acts = resolve(vec![Ok(cand("/in/a.jpg", "x", 5, 11))], &index, &h, true);
        match &acts[0] {
            Action::Duplicate {
                of,
                delete,
                depends_on,
                ..
            } => {
                assert_eq!(of, &PathBuf::from(format!("{OUT}/x.jpg")));
                assert!(*delete);
                assert_eq!(*depends_on, None);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn same_size_different_content_takes_next_name() {
        let mut index = DestIndex::default();
        index.insert(Path::new(OUT), OsStr::new("x.jpg"), info(5, 99));
        let h = hashes(&[(&format!("{OUT}/x.jpg"), 7), ("/in/a.jpg", 8)]);
        let acts = resolve(vec![Ok(cand("/in/a.jpg", "x", 5, 11))], &index, &h, false);
        assert_eq!(dst(&acts[0]), format!("{OUT}/x-2.jpg"));
    }

    #[test]
    fn duplicate_within_input_depends_on_first() {
        let h = hashes(&[("/in/a.jpg", 1), ("/in/b.jpg", 1)]);
        let staged = vec![
            Ok(cand("/in/a.jpg", "x", 5, 11)),
            Ok(cand("/in/b.jpg", "x", 5, 12)),
        ];
        let acts = resolve(staged, &DestIndex::default(), &h, false);
        assert_eq!(dst(&acts[0]), format!("{OUT}/x.jpg"));
        match &acts[1] {
            Action::Duplicate {
                of,
                depends_on,
                delete,
                ..
            } => {
                assert_eq!(of, &PathBuf::from(format!("{OUT}/x.jpg")));
                assert_eq!(*depends_on, Some(0));
                assert!(!*delete);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn same_inode_is_in_place_or_undeletable_dup() {
        let mut index = DestIndex::default();
        index.insert(Path::new(OUT), OsStr::new("x.jpg"), info(5, 11));
        let acts = resolve(
            vec![Ok(cand("/in/a.jpg", "x", 5, 11))],
            &index,
            &HashMap::new(),
            true,
        );
        assert_eq!(
            acts[0],
            Action::AlreadyInPlace {
                src: "/in/a.jpg".into()
            }
        );

        // Two hard links to one inode in the input: never delete.
        let staged = vec![
            Ok(cand("/in/a.jpg", "x", 5, 11)),
            Ok(cand("/in/b.jpg", "x", 5, 11)),
        ];
        let acts = resolve(staged, &DestIndex::default(), &HashMap::new(), true);
        assert!(
            matches!(acts[1], Action::Duplicate { delete: false, .. }),
            "{:?}",
            acts[1]
        );
    }

    #[test]
    fn symlinked_existing_is_not_a_duplicate_copy() {
        let mut index = DestIndex::default();
        let link = Some(FileInfo {
            size: 5,
            dev_ino: (1, 99),
            symlink: true,
        });
        index.insert(Path::new(OUT), OsStr::new("x.jpg"), link);
        let h = hashes(&[(&format!("{OUT}/x.jpg"), 7), ("/in/a.jpg", 7)]);
        let acts = resolve(vec![Ok(cand("/in/a.jpg", "x", 5, 11))], &index, &h, true);
        assert_eq!(dst(&acts[0]), format!("{OUT}/x-2.jpg"));
    }

    #[test]
    fn sidecar_name_collision_bumps_the_bundle() {
        let mut index = DestIndex::default();
        index.insert(Path::new(OUT), OsStr::new("x.xmp"), None);
        let mut c = cand("/in/a.jpg", "x", 5, 11);
        c.sidecars.push(Sidecar {
            path: "/in/a.xmp".into(),
            form: SidecarForm::StemOnly,
        });
        let acts = resolve(vec![Ok(c)], &index, &HashMap::new(), false);
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
        // 12,000 > the old 9,999 cap; with a quadratic walk this took
        // seconds, with per-chain cursors it is linear.
        const N: u64 = 12_000;
        let staged = (0..N)
            .map(|i| Ok(cand(&format!("/in/{i}.jpg"), "x", i, i)))
            .collect();
        let started = std::time::Instant::now();
        let acts = resolve(staged, &DestIndex::default(), &HashMap::new(), false);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert_eq!(dst(&acts[0]), format!("{OUT}/x.jpg"));
        assert_eq!(dst(&acts[N as usize - 1]), format!("{OUT}/x-{N}.jpg"));
        let unique: HashSet<String> = acts.iter().map(dst).collect();
        assert_eq!(unique.len(), N as usize);
    }

    #[test]
    fn duplicate_found_deep_in_a_long_chain() {
        // Existing x.jpg … x-500.jpg; the input equals x-300.jpg.
        let mut index = DestIndex::default();
        for n in 1..=500 {
            let name = numbered_name(OsStr::new("x"), Some(OsStr::new("jpg")), n);
            index.insert(
                Path::new(OUT),
                &name,
                info(u64::from(n), 1000 + u64::from(n)),
            );
        }
        let h = hashes(&[(&format!("{OUT}/x-300.jpg"), 3), ("/in/a.jpg", 3)]);
        let acts = resolve(vec![Ok(cand("/in/a.jpg", "x", 300, 11))], &index, &h, true);
        assert!(
            matches!(&acts[0], Action::Duplicate { of, .. } if of == &PathBuf::from(format!("{OUT}/x-300.jpg"))),
            "{:?}",
            acts[0]
        );
        // A different file still gets the first free number.
        let acts = resolve(
            vec![Ok(cand("/in/b.jpg", "x", 7, 12))],
            &index,
            &HashMap::new(),
            false,
        );
        assert_eq!(dst(&acts[0]), format!("{OUT}/x-501.jpg"));
    }

    #[test]
    fn other_stem_holding_a_numbered_name_is_found() {
        // "x-2" (a kept original name) lands on x's position 2.
        let h = hashes(&[("/in/a.jpg", 1), ("/in/b.jpg", 1)]);
        let staged = vec![
            Ok(cand("/in/o.jpg", "x", 9, 10)),
            Ok(cand("/in/a.jpg", "x-2", 5, 11)),
            Ok(cand("/in/b.jpg", "x", 5, 12)),
        ];
        let acts = resolve(staged, &DestIndex::default(), &h, false);
        assert_eq!(dst(&acts[1]), format!("{OUT}/x-2.jpg"));
        assert!(
            matches!(
                acts[2],
                Action::Duplicate {
                    depends_on: Some(1),
                    ..
                }
            ),
            "{:?}",
            acts[2]
        );
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
            dir("/in", 0, 3, &[]),
            dir("/in/a", 1, 2, &["/in/a/.DS_Store"]), // a.jpg + junk
            dir("/in/b", 1, 2, &[]),                  // b.jpg + keep.txt
            dir("/in/c", 1, 1, &[]),                  // c/d
            dir("/in/c/d", 2, 1, &[]),                // d.jpg
        ];
        let actions = [
            moved("/in/a/a.jpg", "/out/x/1.jpg"),
            moved("/in/b/b.jpg", "/out/x/2.jpg"),
            moved("/in/c/d/d.jpg", "/out/x/3.jpg"),
        ];
        let got: Vec<String> = plan_rmdirs(&dirs, &actions, Path::new("/in"))
            .iter()
            .map(|r| r.path.display().to_string())
            .collect();
        assert_eq!(got, ["/in/c/d", "/in/a", "/in/c"]);
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

    #[test]
    fn hash_set_only_size_collisions() {
        let mut index = DestIndex::default();
        index.insert(Path::new(OUT), OsStr::new("x.jpg"), info(5, 99));
        index.insert(Path::new(OUT), OsStr::new("x-2.jpg"), info(6, 98));
        let a = cand("/in/a.jpg", "x", 5, 11);
        let b = cand("/in/b.jpg", "x", 7, 12);
        let set = hash_set(&[&a, &b], &index);
        assert_eq!(
            set,
            [
                PathBuf::from("/in/a.jpg"),
                PathBuf::from(format!("{OUT}/x.jpg"))
            ]
        );
    }
}
