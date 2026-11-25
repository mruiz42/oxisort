//! Integration tests: collisions, duplicates (existing and within the
//! input), case folding, dry-run fidelity and the plan/execute race seam.

mod common;

use std::fs;

use common::{Env, sorted_lines, stderr};
use oxisort::exec::{self, Stop};
use oxisort::logging::Progress;
use oxisort::model::Outcome;

/// Two different files with the same capture second, plus one exact copy.
fn colliding_input(env: &Env) {
    env.add("dto.jpg", "a/one.jpg");
    let mut other = fs::read(common::fixture("dto.jpg")).unwrap();
    other.extend_from_slice(b"different trailing bytes");
    env.write("b/two.jpg", &other);
    env.add("dto.jpg", "c/copy-of-one.jpg");
}

#[test]
fn collisions_and_duplicates_within_input() {
    let env = Env::new();
    colliding_input(&env);
    let out = env.run(&["--io-jobs", "1"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        env.out_tree(),
        [
            "2017/01/02/20170102-030405123-2.jpg",
            "2017/01/02/20170102-030405123.jpg"
        ]
    );
    let dst = env.output.join("2017/01/02/20170102-030405123.jpg");
    let dup_line = format!(
        "dup {} == {}",
        env.input.join("c/copy-of-one.jpg").display(),
        dst.display()
    );
    assert!(
        sorted_lines(&out).contains(&dup_line),
        "{:?}",
        sorted_lines(&out)
    );
    assert!(stderr(&out).contains("duplicates:       1 (deleted 0)"));
}

#[test]
fn second_run_finds_only_duplicates() {
    let env = Env::new();
    for f in ["dto.jpg", "a.heic", "apple.mov", "notes.txt", "base.jpg"] {
        env.add(f, f);
    }
    assert_eq!(env.run(&[]).status.code(), Some(0));
    let before = env.out_tree();
    let again = env.run(&[]);
    assert_eq!(again.status.code(), Some(0), "{}", stderr(&again));
    assert_eq!(env.out_tree(), before, "nothing new written");
    let lines = sorted_lines(&again);
    assert_eq!(lines.len(), 5);
    assert!(lines.iter().all(|l| l.starts_with("dup ")), "{lines:?}");
    assert!(stderr(&again).contains("transferred:      0"));
}

#[test]
fn dry_run_matches_real_run() {
    let env = Env::new();
    colliding_input(&env);
    env.add("a.heic", "a.heic");
    let dry = env.run(&["--dry-run"]);
    assert_eq!(dry.status.code(), Some(0));
    assert!(!env.output.exists(), "dry-run creates nothing");
    assert!(stderr(&dry).contains("(dry run, nothing changed)"));
    let real = env.run(&[]);
    let stripped: Vec<String> = {
        let mut v: Vec<String> = sorted_lines(&dry)
            .iter()
            .map(|l| {
                l.strip_prefix("[dry-run] would ")
                    .expect("prefixed")
                    .to_owned()
            })
            .collect();
        v.sort();
        v
    };
    assert_eq!(stripped, sorted_lines(&real));
}

#[test]
fn case_folded_original_names() {
    let env = Env::new();
    env.add("dto.jpg", "x/A.jpg");
    let mut other = fs::read(common::fixture("dto.jpg")).unwrap();
    other.push(0);
    env.write("y/a.jpg", &other);
    let out = env.run(&["--original-names"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(env.out_tree(), ["2017/01/02/A.jpg", "2017/01/02/a-2.jpg"]);
}

#[test]
fn destination_appearing_after_planning_is_never_overwritten() {
    let env = Env::new();
    env.add("dto.jpg", "dto.jpg");
    let cfg = oxisort::cli::parse_from([
        "oxisort".as_ref(),
        "--tz".as_ref(),
        "+00:00".as_ref(),
        env.input.as_os_str(),
        env.output.as_os_str(),
    ])
    .unwrap()
    .into_config()
    .unwrap();
    let pools = oxisort::Pools::new(2, 2).unwrap();
    let progress = Progress::disabled();
    let (scanned, plan) = oxisort::plan_run(&cfg, &pools, &Stop::new(), &progress);

    // Another process creates the planned destination in the meantime.
    let dst = env.output.join("2017/01/02/20170102-030405123.jpg");
    fs::create_dir_all(dst.parent().unwrap()).unwrap();
    fs::write(&dst, b"someone else's file").unwrap();

    let outcomes = exec::execute(&plan, cfg.mode, &pools.io, &Stop::new(), &progress);
    match &outcomes[0] {
        Outcome::Failed(msg) => assert!(msg.contains("destination appeared"), "{msg}"),
        other => panic!("expected failure, got {other:?}"),
    }
    assert_eq!(fs::read(&dst).unwrap(), b"someone else's file");
    let summary = oxisort::summarize(&cfg, &scanned, &plan, &outcomes);
    assert_eq!(summary.exit_code(), 1);
}

fn config(args: &[&std::ffi::OsStr]) -> oxisort::model::Config {
    let mut argv: Vec<&std::ffi::OsStr> =
        vec!["oxisort".as_ref(), "--tz".as_ref(), "+00:00".as_ref()];
    argv.extend_from_slice(args);
    oxisort::cli::parse_from(argv)
        .unwrap()
        .into_config()
        .unwrap()
}

#[test]
fn failed_move_keeps_its_in_input_duplicate() {
    let env = Env::new();
    let a = env.add("dto.jpg", "a/one.jpg");
    let b = env.add("dto.jpg", "b/copy.jpg");
    let cfg = config(&[
        "--move".as_ref(),
        "--delete-duplicates".as_ref(),
        env.input.as_os_str(),
        env.output.as_os_str(),
    ]);
    let pools = oxisort::Pools::new(2, 2).unwrap();
    let progress = Progress::disabled();
    let (scanned, plan) = oxisort::plan_run(&cfg, &pools, &Stop::new(), &progress);

    // A's planned destination is taken by someone else, so A's move fails.
    let dst = env.output.join("2017/01/02/20170102-030405123.jpg");
    fs::create_dir_all(dst.parent().unwrap()).unwrap();
    fs::write(&dst, b"someone else's file").unwrap();

    let outcomes = exec::execute(&plan, cfg.mode, &pools.io, &Stop::new(), &progress);
    assert!(matches!(outcomes[0], Outcome::Failed(_)), "{outcomes:?}");
    assert_eq!(
        outcomes[1],
        Outcome::NotRun(oxisort::model::NotRun::DependencyFailed)
    );
    let original = fs::read(common::fixture("dto.jpg")).unwrap();
    assert_eq!(fs::read(&a).unwrap(), original, "A stays in the input");
    assert_eq!(fs::read(&b).unwrap(), original, "B is never deleted");
    let summary = oxisort::summarize(&cfg, &scanned, &plan, &outcomes);
    assert_eq!(summary.exit_code(), 1);
}

#[test]
fn organizing_in_place_twice_is_stable() {
    let env = Env::new();
    env.add("dto.jpg", "x/one.jpg");
    env.add("dto.jpg", "y/copy.jpg");
    env.add("a.heic", "z/deep/two.heic");
    let run = || {
        assert_cmd::Command::cargo_bin("oxisort")
            .unwrap()
            .env("TZ", "UTC")
            .args(["--tz", "+00:00", "--io-jobs", "1"])
            .args(["--move", "--delete-duplicates", "--rmdirs"])
            .arg(&env.input)
            .arg(&env.input)
            .output()
            .unwrap()
    };
    let first = run();
    assert_eq!(first.status.code(), Some(0), "{}", stderr(&first));
    let organized = env.in_tree();
    assert_eq!(organized.len(), 2, "{organized:?}");
    assert!(
        organized.iter().all(|p| p.starts_with("20")),
        "{organized:?}"
    );

    let second = run();
    assert_eq!(second.status.code(), Some(0), "{}", stderr(&second));
    let lines = sorted_lines(&second);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(
        lines.iter().all(|l| l.starts_with("in-place ")),
        "{lines:?}"
    );
    assert_eq!(env.in_tree(), organized, "no target folder removed");
}

#[test]
fn stop_before_execution_changes_nothing_and_reports_interruption() {
    let env = Env::new();
    let src = env.add("dto.jpg", "one.jpg");
    let cfg = config(&[
        "--move".as_ref(),
        env.input.as_os_str(),
        env.output.as_os_str(),
    ]);
    let stop = Stop::new();
    stop.request(oxisort::model::NotRun::Interrupted);
    let summary = oxisort::run(&cfg, &stop, &Progress::disabled()).unwrap();
    assert!(src.exists());
    assert!(env.out_tree().is_empty());
    assert_eq!(summary.exit_code(), 130);
}

const F1_DIR: &str = "2017/01/02";
const F1_BASE: &str = "2017/01/02/20170102-030405123";

/// The validator's F1 repro (docs/05-validation.md): `dto.jpg` with both
/// sidecar forms, its byte-identical twin `sub/dto_copy.jpg`, and an OUT
/// holding a different `…123.jpg` plus a stale `…123-2.jpg.xmp`.
fn f1_setup(env: &Env) -> (Vec<u8>, Vec<u8>) {
    env.add("dto.jpg", "dto.jpg");
    env.write("dto.jpg.xmp", b"<with ext/>");
    env.write("dto.xmp", b"<stem only/>");
    env.add("dto.jpg", "sub/dto_copy.jpg");
    let precious = b"precious, different".to_vec();
    let stale = b"<stale sidecar/>".to_vec();
    fs::create_dir_all(env.output.join(F1_DIR)).unwrap();
    fs::write(env.output.join(format!("{F1_BASE}.jpg")), &precious).unwrap();
    fs::write(env.output.join(format!("{F1_BASE}-2.jpg.xmp")), &stale).unwrap();
    (precious, stale)
}

fn assert_f1_untouched(env: &Env, precious: &[u8], stale: &[u8]) {
    let read = |suffix: &str| fs::read(env.output.join(format!("{F1_BASE}{suffix}"))).unwrap();
    assert_eq!(read(".jpg"), precious);
    assert_eq!(read("-2.jpg.xmp"), stale);
    assert!(
        !env.output.join(format!("{F1_BASE}-2.jpg")).exists(),
        "no copy in the gap"
    );
}

/// I1 (F1 regression).
#[test]
fn f1_regression_in_input_twin_behind_sidecar_gap() {
    let env = Env::new();
    let (precious, stale) = f1_setup(&env);
    let out = env.run(&["--io-jobs", "1"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let main = |suffix: &str| format!("{F1_BASE}{suffix}");
    for want in [main("-3.jpg"), main("-3.jpg.xmp"), main("-3.xmp")] {
        assert!(
            env.out_tree().contains(&want),
            "{want}: {:?}",
            env.out_tree()
        );
    }
    assert_f1_untouched(&env, &precious, &stale);
    let dup_line = format!(
        "dup {} == {}",
        env.input.join("sub/dto_copy.jpg").display(),
        env.output.join(main("-3.jpg")).display()
    );
    assert!(
        sorted_lines(&out).contains(&dup_line),
        "{:?}",
        sorted_lines(&out)
    );
}

/// I2: F1 with `-m --delete-duplicates`: one copy of the content in OUT.
#[test]
fn f1_regression_with_delete_duplicates() {
    let env = Env::new();
    let (precious, stale) = f1_setup(&env);
    let out = env.run(&["--move", "--delete-duplicates", "--io-jobs", "1"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("duplicates:       1 (deleted 1)"),
        "{}",
        stderr(&out)
    );
    assert_f1_untouched(&env, &precious, &stale);
    let original = fs::read(common::fixture("dto.jpg")).unwrap();
    let copies = env
        .out_tree()
        .iter()
        .filter(|p| fs::read(env.output.join(p)).unwrap() == original)
        .count();
    assert_eq!(copies, 1, "{:?}", env.out_tree());
    assert!(env.in_tree().is_empty(), "{:?}", env.in_tree());
}

/// I3: F1 as a dry run predicts the real run line by line.
#[test]
fn f1_dry_run_equals_real_run() {
    let env = Env::new();
    f1_setup(&env);
    let (in_before, out_before) = (env.in_tree(), env.out_tree());
    let dry = env.run(&["--dry-run"]);
    assert_eq!(dry.status.code(), Some(0), "{}", stderr(&dry));
    assert_eq!((env.in_tree(), env.out_tree()), (in_before, out_before));
    let mut predicted: Vec<String> = sorted_lines(&dry)
        .iter()
        .map(|l| {
            l.strip_prefix("[dry-run] would ")
                .expect("prefixed")
                .to_owned()
        })
        .collect();
    predicted.sort();
    let real = env.run(&[]);
    assert_eq!(predicted, sorted_lines(&real));
}

/// I4 (rev 1's V4): organizing an output in place leaves a file at `-3`
/// where it is, although `-2` is free.
#[test]
fn rerun_over_output_with_gap_is_in_place() {
    let env = Env::new();
    let dir = env.input.join(F1_DIR);
    fs::create_dir_all(&dir).unwrap();
    let mut different = fs::read(common::fixture("dto.jpg")).unwrap();
    different.extend_from_slice(b"different");
    fs::write(dir.join("20170102-030405123.jpg"), &different).unwrap();
    fs::copy(
        common::fixture("dto.jpg"),
        dir.join("20170102-030405123-3.jpg"),
    )
    .unwrap();
    let before = env.in_tree();
    let out = assert_cmd::Command::cargo_bin("oxisort")
        .unwrap()
        .env("TZ", "UTC")
        .args(["--tz", "+00:00", "--move"])
        .arg(&env.input)
        .arg(&env.input)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let lines = sorted_lines(&out);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(
        lines.iter().all(|l| l.starts_with("in-place ")),
        "{lines:?}"
    );
    assert_eq!(env.in_tree(), before);
}

/// I5: global rule; a filename-dated file and its undated copy.
#[test]
fn identical_files_with_different_dates_are_duplicates() {
    let env = Env::new();
    env.add("IMG_20160915_123456.jpg", "IMG_20160915_123456.jpg");
    env.add("IMG_20160915_123456.jpg", "plain.jpg");
    let out = env.run(&["--io-jobs", "1"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(env.out_tree(), ["2016/09/15/20160915-123456.jpg"]);
    let dup_line = format!(
        "dup {} == {}",
        env.input.join("plain.jpg").display(),
        env.output.join("2016/09/15/20160915-123456.jpg").display()
    );
    assert!(
        sorted_lines(&out).contains(&dup_line),
        "{:?}",
        sorted_lines(&out)
    );
}

/// I6: an identical file anywhere in OUT, under any name, is the keeper.
#[test]
fn duplicate_of_an_existing_file_elsewhere_in_out() {
    let env = Env::new();
    env.add("dto.jpg", "dto.jpg");
    let old = env.output.join("old stuff/renamed by hand.jpg");
    fs::create_dir_all(old.parent().unwrap()).unwrap();
    fs::copy(common::fixture("dto.jpg"), &old).unwrap();
    let out = env.run(&[]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        env.out_tree(),
        ["old stuff/renamed by hand.jpg"],
        "main tree untouched"
    );
    let dup_line = format!(
        "dup {} == {}",
        env.input.join("dto.jpg").display(),
        old.display()
    );
    assert!(
        sorted_lines(&out).contains(&dup_line),
        "{:?}",
        sorted_lines(&out)
    );
}
