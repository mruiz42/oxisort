//! Integration tests: --jobs/--io-jobs determinism, console/log output
//! modes, the exit-code matrix and graceful stops.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;

use common::{Env, fixture, stderr, stdout};
use oxisort::exec::{self, Stop};
use oxisort::logging::Progress;
use oxisort::model::{NotRun, Outcome};

/// ~300 files: a few capture times shared by many different files (long
/// `-N` chains), exact duplicates, and nested folders. Trailing bytes after
/// the image data keep the tags intact while making the content unique.
fn big_tree(env: &Env) {
    let bases = [
        "dto.jpg",
        "cd_only.jpg",
        "modify_only.jpg",
        "a.png",
        "a.tiff",
        "fake.arw",
        "base.jpg",
    ];
    for i in 0..300 {
        let base = bases[i % bases.len()];
        let mut bytes = fs::read(fixture(base)).unwrap();
        // Every 10th file is an exact duplicate of an earlier one.
        let variant = if i % 10 == 9 { i - 9 } else { i };
        bytes.extend_from_slice(format!("variant {variant}").as_bytes());
        let ext = base.rsplit('.').next().unwrap();
        env.write(&format!("d{}/e{}/f{i:03}.{ext}", i % 7, i % 3), &bytes);
    }
}

/// Relative path → content hash of every output file.
fn snapshot(env: &Env) -> BTreeMap<String, blake3::Hash> {
    env.out_tree()
        .into_iter()
        .map(|rel| {
            let h = blake3::hash(&fs::read(env.output.join(&rel)).unwrap());
            (rel, h)
        })
        .collect()
}

#[test]
fn output_is_identical_for_any_thread_counts() {
    let mut reference = None;
    for flags in [
        &["--io-jobs", "1", "--jobs", "1"][..],
        &["--io-jobs", "4"],
        &["--io-jobs", "16"],
        &["--jobs", "1", "--io-jobs", "16"],
    ] {
        let env = Env::new();
        big_tree(&env);
        let out = env.run(flags);
        assert_eq!(out.status.code(), Some(0), "{flags:?}: {}", stderr(&out));
        let snap = snapshot(&env);
        assert!(snap.len() > 200, "{}", snap.len());
        match &reference {
            None => reference = Some(snap),
            Some(r) => assert_eq!(&snap, r, "{flags:?} differs from --io-jobs 1"),
        }
    }
}

#[test]
fn quiet_prints_nothing_on_success() {
    let env = Env::new();
    env.add("dto.jpg", "dto.jpg");
    let out = env.run(&["--quiet"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(stdout(&out), "");
    assert_eq!(stderr(&out), "");
}

#[test]
fn quiet_still_reports_errors() {
    let env = Env::new();
    let locked = env.add("dto.jpg", "locked.jpg");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let out = env.run(&["--quiet"]);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(out.status.code(), Some(1), "unreadable file → exit 1");
    assert_eq!(stdout(&out), "");
    assert!(stderr(&out).contains("locked.jpg"), "{}", stderr(&out));
}

#[test]
fn log_file_gets_header_lines_and_summary() {
    let env = Env::new();
    env.add("dto.jpg", "dto.jpg");
    let log = env.dir.path().join("run.log");
    let out = env.run(&["--quiet", "--log", log.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0));
    let text = fs::read_to_string(&log).unwrap();
    let header = format!("INFO  oxisort {} run: ", env!("CARGO_PKG_VERSION"));
    assert!(text.contains(&header), "{text}");
    assert!(text.contains("INFO  copy "), "{text}");
    assert!(text.contains("transferred:      1"), "{text}");
    // Appends on the next run.
    env.run(&["--quiet", "--log", log.to_str().unwrap()]);
    let again = fs::read_to_string(&log).unwrap();
    assert_eq!(again.matches(" run: ").count(), 2);
}

#[test]
fn log_file_inside_input_is_not_organized() {
    let env = Env::new();
    env.add("dto.jpg", "dto.jpg");
    let log = env.input.join("oxisort.log");
    let out = env.run(&["--log", log.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(env.out_tree(), ["2017/01/02/20170102-030405123.jpg"]);
}

#[test]
fn unwritable_log_is_usage_error() {
    let env = Env::new();
    let out = env.run(&[
        "--log",
        env.dir.path().join("no/such/dir.log").to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("cannot open log file"));
}

#[test]
fn progress_without_tty_is_clean() {
    let env = Env::new();
    env.add("dto.jpg", "dto.jpg");
    let out = env.run(&["--progress"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(stdout(&out), "", "per-file lines are replaced by the bar");
    let err = stderr(&out);
    assert!(err.starts_with("oxisort: copy finished"), "{err:?}");
    assert!(!err.contains('\u{1b}'), "no escape codes when not a TTY");
}

#[test]
fn verbose_shows_debug_details() {
    let env = Env::new();
    env.add("notes.txt", "notes.txt");
    env.write("orphan.xmp", b"x");
    let out = env.run(&["-v"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(stderr(&out).contains("orphan sidecar"), "{}", stderr(&out));
}

#[test]
fn uncreatable_output_is_fatal() {
    let env = Env::new();
    env.add("dto.jpg", "dto.jpg");
    let ro = env.dir.path().join("ro");
    fs::create_dir(&ro).unwrap();
    fs::set_permissions(&ro, fs::Permissions::from_mode(0o500)).unwrap();
    let out = assert_cmd::Command::cargo_bin("oxisort")
        .unwrap()
        .arg(&env.input)
        .arg(ro.join("out"))
        .output()
        .unwrap();
    fs::set_permissions(&ro, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
}

fn planned(
    env: &Env,
) -> (
    oxisort::model::Config,
    oxisort::Pools,
    oxisort::scan::ScanResult,
    oxisort::plan::Plan,
) {
    let cfg = oxisort::cli::parse_from([
        "oxisort".as_ref(),
        "--tz".as_ref(),
        "UTC".as_ref(),
        env.input.as_os_str(),
        env.output.as_os_str(),
    ])
    .unwrap()
    .into_config()
    .unwrap();
    let pools = oxisort::Pools::new(2, 2).unwrap();
    let (scanned, plan) = oxisort::plan_run(&cfg, &pools, &Stop::new(), &Progress::disabled());
    (cfg, pools, scanned, plan)
}

#[test]
fn interrupt_before_transfers_runs_nothing() {
    let env = Env::new();
    for f in ["dto.jpg", "a.heic", "apple.mov"] {
        env.add(f, f);
    }
    let (cfg, pools, scanned, plan) = planned(&env);
    let stop = Stop::new();
    stop.request(NotRun::Interrupted);
    let outcomes = exec::execute(
        &plan,
        cfg.mode,
        Default::default(),
        &pools.io,
        &stop,
        &Progress::disabled(),
    )
    .outcomes;
    assert!(
        outcomes
            .iter()
            .all(|o| *o == Outcome::NotRun(NotRun::Interrupted)),
        "{outcomes:?}"
    );
    assert_eq!(
        oxisort::summarize(&cfg, &scanned, &plan, &outcomes).exit_code(),
        130
    );
    assert!(env.out_tree().is_empty());
}

#[test]
fn disk_full_stop_reports_not_run_and_exit_1() {
    let env = Env::new();
    env.add("dto.jpg", "dto.jpg");
    let (cfg, pools, scanned, plan) = planned(&env);
    let stop = Stop::new();
    stop.request(NotRun::DiskFull);
    let outcomes = exec::execute(
        &plan,
        cfg.mode,
        Default::default(),
        &pools.io,
        &stop,
        &Progress::disabled(),
    )
    .outcomes;
    assert_eq!(outcomes, [Outcome::NotRun(NotRun::DiskFull)]);
    let summary = oxisort::summarize(&cfg, &scanned, &plan, &outcomes);
    assert_eq!(summary.exit_code(), 1);
    let text = summary.render(cfg.mode, Default::default(), 1);
    assert!(
        text.starts_with("oxisort: copy stopped: destination full after"),
        "{text}"
    );
    assert!(
        text.contains("not run:          1 (destination full)"),
        "{text}"
    );
}

/// F2: a duplicate whose transfer never ran because of Ctrl-C is "not run
/// (interrupted)", not a dependency failure, and the summary says so.
#[test]
fn interrupt_reports_pending_duplicates_as_interrupted() {
    let env = Env::new();
    env.add("dto.jpg", "dto.jpg");
    env.add("dto.jpg", "sub/dto_copy.jpg");
    let (cfg, pools, scanned, plan) = planned(&env);
    assert!(
        plan.actions
            .iter()
            .any(|a| matches!(a, oxisort::model::Action::Duplicate { .. })),
        "fixture must plan a duplicate"
    );
    let stop = Stop::new();
    stop.request(NotRun::Interrupted);
    let outcomes = exec::execute(
        &plan,
        cfg.mode,
        Default::default(),
        &pools.io,
        &stop,
        &Progress::disabled(),
    )
    .outcomes;
    assert_eq!(
        outcomes,
        [
            Outcome::NotRun(NotRun::Interrupted),
            Outcome::NotRun(NotRun::Interrupted)
        ]
    );
    let summary = oxisort::summarize(&cfg, &scanned, &plan, &outcomes);
    assert_eq!(summary.exit_code(), 130);
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);
    let text = summary.render(cfg.mode, Default::default(), 2);
    assert!(
        text.starts_with("oxisort: copy interrupted after"),
        "{text}"
    );
    assert!(text.contains("not run:          2 (interrupted)"), "{text}");
}

/// I16: a disk-full stop on the last task leaves no NotRun outcome, yet
/// the header still says the run stopped (via `apply_stop`).
#[test]
fn disk_full_header_end_to_end() {
    let env = Env::new();
    env.add("dto.jpg", "dto.jpg");
    let (cfg, pools, scanned, plan) = planned(&env);
    let outcomes = exec::execute(
        &plan,
        cfg.mode,
        Default::default(),
        &pools.io,
        &Stop::new(),
        &Progress::disabled(),
    )
    .outcomes;
    assert!(
        matches!(outcomes[0], Outcome::Transferred { .. }),
        "{outcomes:?}"
    );
    let mut summary = oxisort::summarize(&cfg, &scanned, &plan, &outcomes);
    assert_eq!(summary.not_run, 0);
    // ENOSPC on the final task: the stop flag is set, every task is done.
    let stop = Stop::new();
    stop.request(NotRun::DiskFull);
    oxisort::report::apply_stop(&mut summary, &stop);
    let text = summary.render(cfg.mode, Default::default(), 1);
    assert!(
        text.starts_with("oxisort: copy stopped: destination full after"),
        "{text}"
    );
    assert_eq!(summary.exit_code(), 1);
}
