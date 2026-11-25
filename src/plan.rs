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

use crate::extract::{ExtractError, Extracted};
use crate::hash;
use crate::model::{Action, Config, Kind, Mode, PlannedSidecar};
use crate::scan::{Sidecar, SidecarForm};

/// Guard against pathological inputs: `stem-9999.ext` is the last name tried.
pub const MAX_COLLISIONS: u32 = 9_999;

/// The complete plan for a run.
#[derive(Debug, Default)]
pub struct Plan {
    pub actions: Vec<Action>,
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
    cfg: &Config,
    pool: &rayon::ThreadPool,
) -> Plan {
    let staged: Vec<Result<Candidate, Action>> = items
        .into_iter()
        .map(|item| match item {
            Ok(x) => Ok(candidate(x, cfg)),
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
    let mut index = DestIndex::load(&dirs, pool);
    index.stat_chains(&cands, pool);
    let hashes = hash::hash_all(&hash_set(&cands, &index), pool);

    let delete = cfg.delete_duplicates && cfg.mode == Mode::Move;
    Plan {
        actions: resolve(staged, &index, &hashes, delete),
    }
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

/// Who holds a planned name.
#[derive(Debug, Clone)]
enum Occupant {
    Main {
        task: usize,
        src: PathBuf,
        dst: PathBuf,
        size: u64,
        dev_ino: (u64, u64),
    },
    Sidecar,
}

/// Plan step 4d (pure): assigns final names in input order.
pub(crate) fn resolve(
    staged: Vec<Result<Candidate, Action>>,
    index: &DestIndex,
    hashes: &HashMap<PathBuf, blake3::Hash>,
    delete_duplicates: bool,
) -> Vec<Action> {
    let mut planned: HashMap<(PathBuf, String), Occupant> = HashMap::new();
    staged
        .into_iter()
        .enumerate()
        .map(|(task, s)| match s {
            Ok(c) => place(task, c, index, hashes, delete_duplicates, &mut planned),
            Err(action) => action,
        })
        .collect()
}

fn same_content(hashes: &HashMap<PathBuf, blake3::Hash>, a: &Path, b: &Path) -> bool {
    matches!((hashes.get(a), hashes.get(b)), (Some(x), Some(y)) if x == y)
}

fn place(
    task: usize,
    c: Candidate,
    index: &DestIndex,
    hashes: &HashMap<PathBuf, blake3::Hash>,
    delete: bool,
    planned: &mut HashMap<(PathBuf, String), Occupant>,
) -> Action {
    let t = &c.target;
    let duplicate =
        |c: Candidate, of: PathBuf, delete: bool, depends_on: Option<usize>| Action::Duplicate {
            of,
            delete,
            depends_on,
            size: c.size,
            dev_ino: c.dev_ino,
            sidecars_left: c.sidecars.into_iter().map(|s| s.path).collect(),
            src: c.src,
        };
    for n in 1..=MAX_COLLISIONS {
        let name = numbered_name(&t.stem, t.ext.as_deref(), n);
        let folded = fold(&name);
        if let Some(occ) = planned.get(&(t.dir.clone(), folded.clone())) {
            match occ {
                Occupant::Main {
                    task: other,
                    src,
                    dst,
                    size,
                    dev_ino,
                } => {
                    // Same inode (hard links in the input): a duplicate that
                    // must never be deleted.
                    if *dev_ino == c.dev_ino {
                        let (dst, other) = (dst.clone(), *other);
                        return duplicate(c, dst, false, Some(other));
                    }
                    if *size == c.size && same_content(hashes, src, &c.src) {
                        let (dst, other) = (dst.clone(), *other);
                        return duplicate(c, dst, delete, Some(other));
                    }
                }
                Occupant::Sidecar => {}
            }
            continue;
        }
        if let Some(existing) = index.get(&t.dir, &folded) {
            if let Some(info) = existing.info {
                if info.dev_ino == c.dev_ino {
                    return Action::AlreadyInPlace { src: c.src };
                }
                if !info.symlink
                    && info.size == c.size
                    && same_content(hashes, &existing.path, &c.src)
                {
                    return duplicate(c, existing.path.clone(), delete, None);
                }
            }
            continue;
        }
        let stem_n = numbered_name(&t.stem, None, n);
        let side_names: Vec<OsString> = c
            .sidecars
            .iter()
            .map(|s| sidecar_name(&name, &stem_n, s, t.generated))
            .collect();
        let taken = |s: &OsString| {
            let f = fold(s);
            index.get(&t.dir, &f).is_some() || planned.contains_key(&(t.dir.clone(), f))
        };
        if side_names.iter().any(taken) {
            continue;
        }
        let dst = t.dir.join(&name);
        planned.insert(
            (t.dir.clone(), folded),
            Occupant::Main {
                task,
                src: c.src.clone(),
                dst: dst.clone(),
                size: c.size,
                dev_ino: c.dev_ino,
            },
        );
        for s in &side_names {
            planned.insert((t.dir.clone(), fold(s)), Occupant::Sidecar);
        }
        let sidecars = c
            .sidecars
            .iter()
            .zip(side_names)
            .map(|(s, n)| PlannedSidecar {
                src: s.path.clone(),
                dst: t.dir.join(n),
            })
            .collect();
        return Action::Transfer {
            src: c.src,
            dst,
            kind: c.kind,
            dated: c.dated,
            size: c.size,
            sidecars,
        };
    }
    Action::Fail {
        error: format!(
            "more than {MAX_COLLISIONS} files want the name {}",
            t.dir.join(&t.stem).display()
        ),
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
    fn collision_cap() {
        let mut index = DestIndex::default();
        for n in 1..=MAX_COLLISIONS {
            index.insert(
                Path::new(OUT),
                &numbered_name(OsStr::new("x"), Some(OsStr::new("jpg")), n),
                None,
            );
        }
        let acts = resolve(
            vec![Ok(cand("/in/a.jpg", "x", 5, 11))],
            &index,
            &HashMap::new(),
            false,
        );
        assert!(matches!(acts[0], Action::Fail { .. }));
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
