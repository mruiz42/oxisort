//! oxisort: sort photos and videos into date-based folders.
//!
//! The run is a pipeline of separate phases (see `docs/02-plan.md`):
//! scan → extract (parallel) → plan (single-threaded, deterministic) →
//! execute (parallel transfers) → report. Every target name is fixed during
//! planning, so worker threads never race for a name.

#![deny(unsafe_op_in_unsafe_fn)]

// Windows support is deferred to the packaging run (plan decision Q5): the
// no-clobber primitives there need their own code and CI.
#[cfg(not(unix))]
compile_error!("oxisort currently supports macOS and Linux only");

pub mod classify;
pub mod cli;
pub mod date;
pub mod dateformat;
pub mod dedupe;
pub mod exec;
pub mod extract;
pub mod fsops;
pub mod hash;
pub mod logging;
pub mod metadata;
pub mod model;
pub mod plan;
pub mod report;
pub mod scan;
pub mod signal;

use std::path::PathBuf;
use std::time::Instant;

use crate::date::ResolveOptions;
use crate::exec::Stop;
use crate::extract::extract_all;
use crate::logging::{Progress, SUMMARY};
use crate::model::Config;
use crate::plan::Plan;
use crate::report::RunSummary;
use crate::scan::{ScanOptions, ScanResult};

/// A failure that ends the run before any per-file work (exit code 3).
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct FatalError(pub String);

/// The two thread pools of a run: CPU work (`--jobs`) and transfers
/// (`--io-jobs`). The global rayon pool is never used, so blocking I/O can't
/// starve CPU work and vice versa.
pub struct Pools {
    pub cpu: rayon::ThreadPool,
    pub io: rayon::ThreadPool,
}

impl Pools {
    /// Builds both pools with named threads.
    ///
    /// # Errors
    /// If the OS refuses to spawn threads.
    pub fn new(jobs: usize, io_jobs: usize) -> Result<Self, FatalError> {
        let build = |n: usize, name: &'static str| {
            rayon::ThreadPoolBuilder::new()
                .num_threads(n)
                .thread_name(move |i| format!("{name}-{i}"))
                .build()
                .map_err(|e| FatalError(format!("cannot start {name} threads: {e}")))
        };
        Ok(Pools {
            cpu: build(jobs, "oxisort-cpu")?,
            io: build(io_jobs, "oxisort-io")?,
        })
    }
}

/// Runs phases 1–4 (scan, extract, plan) without changing anything.
///
/// `stop` is checked after the scan and before each file of extraction and
/// of the duplicate-hash pass, so a Ctrl-C during a long extraction or
/// hashing (slow SD card, 50k files, large videos) returns once the files
/// in flight finish. The scan itself is not interruptible. Once stopped,
/// the result is an empty plan (a partial extraction or hash map is never
/// planned from), so nothing is changed and the caller reports the
/// interruption (exit 130).
pub fn plan_run(
    cfg: &Config,
    pools: &Pools,
    stop: &Stop,
    progress: &Progress,
) -> (ScanResult, Plan) {
    let mut exclude: Vec<PathBuf> = Vec::new();
    if cfg.output != cfg.input && cfg.output.starts_with(&cfg.input) {
        exclude.push(cfg.output.clone());
    }
    exclude.extend(cfg.log_file.iter().cloned());

    let spinner = progress.spinner("scanning");
    let mut scanned = scan::scan(&ScanOptions {
        root: &cfg.input,
        max_depth: cfg.max_depth,
        exclude: &exclude,
    });
    spinner.finish_and_clear();
    for (path, msg) in &scanned.errors {
        log::error!("{}: {msg}", path.display());
    }

    if stopped_before(stop, "reading metadata") {
        return (scanned, Plan::default());
    }

    let entries = std::mem::take(&mut scanned.entries);
    let bar = progress.bar(entries.len() as u64, "reading metadata");
    let opts = ResolveOptions {
        date_fields: &cfg.date_fields,
        regex: &cfg.regex,
        timestamp: cfg.timestamp,
        zone: cfg.zone,
    };
    let extracted = extract_all(entries, &opts, &pools.cpu, stop, &bar);
    bar.finish_and_clear();
    if stopped_before(stop, "planning") {
        return (scanned, Plan::default());
    }

    let spinner = progress.spinner("planning");
    let plan = plan::build(extracted, &scanned.dirs, cfg, pools, stop);
    spinner.finish_and_clear();
    // A stop during hashing leaves an incomplete hash map; discard the plan.
    if stopped_before(stop, "executing") {
        return (scanned, Plan::default());
    }
    (scanned, plan)
}

/// True (and logged) if the run should stop before `phase`.
fn stopped_before(stop: &Stop, phase: &str) -> bool {
    let stopped = stop.reason().is_some();
    if stopped {
        log::warn!("stopping before {phase}: no file was changed");
    }
    stopped
}

/// Runs the whole pipeline and logs the summary.
///
/// # Errors
/// [`FatalError`] if the thread pools or the output folder can't be
/// created. Per-file problems never abort the run; they are counted in the
/// returned summary.
pub fn run(cfg: &Config, stop: &Stop, progress: &Progress) -> Result<RunSummary, FatalError> {
    let started = Instant::now();
    let pools = Pools::new(cfg.jobs, cfg.io_jobs)?;
    if !cfg.dry_run {
        std::fs::create_dir_all(&cfg.output).map_err(|e| {
            FatalError(format!(
                "cannot create output {}: {e}",
                cfg.output.display()
            ))
        })?;
    }
    let (scanned, plan) = plan_run(cfg, &pools, stop, progress);
    let outcomes = if cfg.dry_run {
        exec::simulate(&plan, cfg.mode)
    } else if let Some(why) = stop.reason() {
        exec::not_run(&plan, why)
    } else {
        exec::execute(&plan, cfg.mode, &pools.io, stop, progress)
    };

    let mut summary = summarize(cfg, &scanned, &plan, &outcomes);
    let rm = exec::remove_dirs(&plan.rmdirs, cfg.dry_run, stop);
    summary.dirs_removed = rm.dirs;
    summary.junk_removed = rm.junk;
    summary.errors.extend(rm.errors);
    // A stop after the last task started leaves no NotRun outcome behind.
    report::apply_stop(&mut summary, stop);
    log::info!(
        target: SUMMARY,
        "{}",
        summary.render(cfg.mode, started.elapsed(), plan.actions.len())
    );
    Ok(summary)
}

/// Folds scan counts and per-action outcomes into the run summary.
pub fn summarize(
    cfg: &Config,
    scanned: &ScanResult,
    plan: &Plan,
    outcomes: &[model::Outcome],
) -> RunSummary {
    let mut summary = RunSummary {
        dry_run: cfg.dry_run,
        scan: scanned.counts.clone(),
        shared_sidecars: scanned.shared_sidecars.len(),
        ..RunSummary::default()
    };
    summary.errors.extend(
        scanned
            .errors
            .iter()
            .map(|(p, m)| format!("{}: {m}", p.display())),
    );
    for (a, o) in plan.actions.iter().zip(outcomes) {
        summary.add(a, o);
    }
    summary
}
