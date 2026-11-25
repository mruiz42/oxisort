//! Phase 4: decide every target path, single-threaded and in input order,
//! so the result is deterministic and dry-run shows exactly what a real run
//! does. Worker threads later only execute these decisions.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};

use rayon::prelude::*;

use crate::extract::{ExtractError, Extracted};
use crate::model::{Action, Config, PlannedSidecar};
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

/// Names already present in each target folder, keyed case-folded (APFS is
/// case-insensitive by default; on a case-sensitive volume this can only
/// add an unnecessary `-2`).
#[derive(Debug, Default)]
pub(crate) struct DestIndex {
    dirs: HashMap<PathBuf, HashSet<String>>,
}

impl DestIndex {
    /// Lists each folder once, in parallel. A missing folder counts as empty.
    pub fn load(dirs: &[PathBuf], pool: &rayon::ThreadPool) -> Self {
        let listed: Vec<(PathBuf, HashSet<String>)> = pool.install(|| {
            dirs.par_iter()
                .map(|d| {
                    let names = match fs::read_dir(d) {
                        Ok(rd) => rd
                            .filter_map(Result::ok)
                            .map(|e| fold(&e.file_name()))
                            .collect(),
                        Err(_) => HashSet::new(),
                    };
                    (d.clone(), names)
                })
                .collect()
        });
        DestIndex {
            dirs: listed.into_iter().collect(),
        }
    }

    fn contains(&self, dir: &Path, folded: &str) -> bool {
        self.dirs.get(dir).is_some_and(|s| s.contains(folded))
    }
}

/// Case-folds a file name for occupancy checks.
pub(crate) fn fold(name: &OsStr) -> String {
    name.to_string_lossy().to_lowercase()
}

/// Builds the plan from extraction results (in scan order).
pub fn build(
    items: Vec<Result<Extracted, ExtractError>>,
    cfg: &Config,
    pool: &rayon::ThreadPool,
) -> Plan {
    let targets: Vec<Option<Target>> = items
        .iter()
        .map(|r| r.as_ref().ok().map(|x| target_for(x, cfg)))
        .collect();
    let mut dirs: Vec<PathBuf> = targets.iter().flatten().map(|t| t.dir.clone()).collect();
    dirs.sort();
    dirs.dedup();
    let index = DestIndex::load(&dirs, pool);

    let mut occupied: HashSet<(PathBuf, String)> = HashSet::new();
    let actions = items
        .into_iter()
        .zip(targets)
        .map(|(item, target)| match (item, target) {
            (Ok(x), Some(t)) => place(&x, &t, &index, &mut occupied),
            (Err(e), _) => Action::Fail {
                src: e.entry.path,
                error: e.message,
            },
            (Ok(x), None) => unreachable!(
                "target computed for every ok item: {}",
                x.entry.path.display()
            ),
        })
        .collect();
    Plan { actions }
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
/// `name.xmp` → `<main stem>.xmp`.
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

/// Finds the first free `stem[-n].ext` for one file (plan step 4d). The
/// main name and every sidecar name must all be free.
fn place(
    x: &Extracted,
    t: &Target,
    index: &DestIndex,
    occupied: &mut HashSet<(PathBuf, String)>,
) -> Action {
    let is_taken = |occupied: &HashSet<(PathBuf, String)>, name: &OsStr| {
        let f = fold(name);
        index.contains(&t.dir, &f) || occupied.contains(&(t.dir.clone(), f))
    };
    for n in 1..=MAX_COLLISIONS {
        let name = numbered_name(&t.stem, t.ext.as_deref(), n);
        if is_taken(occupied, &name) {
            continue;
        }
        let stem_n = numbered_name(&t.stem, None, n);
        let side_names: Vec<OsString> = x
            .entry
            .sidecars
            .iter()
            .map(|s| sidecar_name(&name, &stem_n, s, t.generated))
            .collect();
        if side_names.iter().any(|s| is_taken(occupied, s)) {
            continue;
        }
        occupied.insert((t.dir.clone(), fold(&name)));
        for s in &side_names {
            occupied.insert((t.dir.clone(), fold(s)));
        }
        return Action::Transfer {
            src: x.entry.path.clone(),
            dst: t.dir.join(&name),
            kind: x.kind,
            dated: x.date.is_some(),
            size: x.size,
            sidecars: x
                .entry
                .sidecars
                .iter()
                .zip(side_names)
                .map(|(s, n)| PlannedSidecar {
                    src: s.path.clone(),
                    dst: t.dir.join(n),
                })
                .collect(),
        };
    }
    Action::Fail {
        src: x.entry.path.clone(),
        error: format!(
            "more than {MAX_COLLISIONS} files want the name {}",
            t.dir.join(&t.stem).display()
        ),
    }
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
