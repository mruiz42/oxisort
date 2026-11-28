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
            "2017/01/02/20170102-030405123.jpg",
            "duplicates/c/copy-of-one.jpg",
        ]
    );
    let dst = env.output.join("2017/01/02/20170102-030405123.jpg");
    let dup_line = format!(
        "dup-copy {} -> {} (duplicate of {})",
        env.input.join("c/copy-of-one.jpg").display(),
        env.output.join("duplicates/c/copy-of-one.jpg").display(),
        dst.display()
    );
    assert!(
        sorted_lines(&out).contains(&dup_line),
        "{:?}",
        sorted_lines(&out)
    );
    assert!(
        stderr(&out).contains("duplicates:       1 (copied to duplicates/: 1)"),
        "{}",
        stderr(&out)
    );
    assert!(
        env.input.join("c/copy-of-one.jpg").exists(),
        "copy keeps the source"
    );
}

const SECOND_RUN_SET: [&str; 5] = ["dto.jpg", "a.heic", "apple.mov", "notes.txt", "base.jpg"];

#[test]
fn second_run_finds_only_duplicates() {
    let env = Env::new();
    for f in SECOND_RUN_SET {
        env.add(f, f);
    }
    assert_eq!(env.run(&["--skip-duplicates"]).status.code(), Some(0));
    let before = env.out_tree();
    let again = env.run(&["--skip-duplicates"]);
    assert_eq!(again.status.code(), Some(0), "{}", stderr(&again));
    assert_eq!(env.out_tree(), before, "nothing new written");
    let lines = sorted_lines(&again);
    assert_eq!(lines.len(), 5);
    assert!(lines.iter().all(|l| l.starts_with("dup ")), "{lines:?}");
    assert!(stderr(&again).contains("transferred:      0"));
    assert!(stderr(&again).contains("duplicates:       5 (left in place)"));
}

/// I10: a copy rerun files every input into the duplicates folder once;
/// the third run writes nothing ("already there").
#[test]
fn copy_rerun_files_duplicates_once() {
    let env = Env::new();
    for f in SECOND_RUN_SET {
        env.add(f, f);
    }
    assert_eq!(env.run(&[]).status.code(), Some(0));
    let sorted = env.out_tree();
    assert!(
        !sorted.iter().any(|p| p.starts_with("duplicates/")),
        "{sorted:?}"
    );

    let second = env.run(&[]);
    assert_eq!(second.status.code(), Some(0), "{}", stderr(&second));
    let filed: Vec<String> = env
        .out_tree()
        .into_iter()
        .filter(|p| p.starts_with("duplicates/"))
        .collect();
    let mut want: Vec<String> = SECOND_RUN_SET
        .iter()
        .map(|f| format!("duplicates/{f}"))
        .collect();
    want.sort();
    assert_eq!(filed, want);
    assert!(stderr(&second).contains("duplicates:       5 (copied to duplicates/: 5)"));

    let after_second = env.out_tree();
    let third = env.run(&[]);
    assert_eq!(third.status.code(), Some(0), "{}", stderr(&third));
    assert_eq!(env.out_tree(), after_second, "nothing new written");
    let lines = sorted_lines(&third);
    assert!(
        lines.iter().all(|l| l.contains("(already in ")),
        "{lines:?}"
    );
    assert!(
        stderr(&third).contains("duplicates:       5 (copied to duplicates/: 0, already there: 5)"),
        "{}",
        stderr(&third)
    );
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

    let outcomes = exec::execute(
        &plan,
        cfg.mode,
        Default::default(),
        &pools.io,
        &Stop::new(),
        &progress,
    )
    .outcomes;
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

    let outcomes = exec::execute(
        &plan,
        cfg.mode,
        Default::default(),
        &pools.io,
        &Stop::new(),
        &progress,
    )
    .outcomes;
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
    let filed = env.output.join("duplicates/sub/dto_copy.jpg");
    let dup_line = format!(
        "dup-copy {} -> {} (duplicate of {})",
        env.input.join("sub/dto_copy.jpg").display(),
        filed.display(),
        env.output.join(main("-3.jpg")).display()
    );
    assert!(
        sorted_lines(&out).contains(&dup_line),
        "{:?}",
        sorted_lines(&out)
    );
    assert_eq!(
        fs::read(&filed).unwrap(),
        fs::read(common::fixture("dto.jpg")).unwrap()
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
    assert_eq!(
        env.out_tree(),
        ["2016/09/15/20160915-123456.jpg", "duplicates/plain.jpg"]
    );
    let dup_line = format!(
        "dup-copy {} -> {} (duplicate of {})",
        env.input.join("plain.jpg").display(),
        env.output.join("duplicates/plain.jpg").display(),
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
        ["duplicates/dto.jpg", "old stuff/renamed by hand.jpg"],
        "nothing written to the main tree"
    );
    let dup_line = format!(
        "dup-copy {} -> {} (duplicate of {})",
        env.input.join("dto.jpg").display(),
        env.output.join("duplicates/dto.jpg").display(),
        old.display()
    );
    assert!(
        sorted_lines(&out).contains(&dup_line),
        "{:?}",
        sorted_lines(&out)
    );
}

// ---- The duplicates folder (plan amendment A1, slice 3).

const DST: &str = "2017/01/02/20170102-030405123.jpg";

fn in_place_cmd(env: &Env, extra: &[&str]) -> std::process::Output {
    assert_cmd::Command::cargo_bin("oxisort")
        .unwrap()
        .env("TZ", "UTC")
        .args(["--tz", "+00:00", "--io-jobs", "1"])
        .args(extra)
        .arg(&env.input)
        .arg(&env.input)
        .output()
        .unwrap()
}

/// I7.
#[test]
fn copy_files_duplicates_into_duplicates_folder() {
    let env = Env::new();
    env.add("dto.jpg", "a/one.jpg");
    env.add("dto.jpg", "b/deep/copy.jpg");
    env.write("b/deep/copy.jpg.xmp", b"<with ext/>");
    env.write("b/deep/copy.xmp", b"<stem/>");
    let before = env.in_tree();
    let out = env.run(&["--io-jobs", "1"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        env.out_tree(),
        [
            DST,
            "duplicates/b/deep/copy.jpg",
            "duplicates/b/deep/copy.jpg.xmp",
            "duplicates/b/deep/copy.xmp",
        ]
    );
    assert_eq!(
        fs::read(env.output.join("duplicates/b/deep/copy.xmp")).unwrap(),
        b"<stem/>"
    );
    assert_eq!(env.in_tree(), before, "copy keeps every source");
    let lines = sorted_lines(&out);
    let side = format!(
        "  + {} -> {}",
        env.input.join("b/deep/copy.xmp").display(),
        env.output.join("duplicates/b/deep/copy.xmp").display()
    );
    assert!(lines.contains(&side), "{lines:?}");
}

/// I8.
#[test]
fn move_files_duplicates_and_rmdirs_cleans_up() {
    let env = Env::new();
    env.add("dto.jpg", "a/one.jpg");
    env.add("dto.jpg", "b/copy.jpg");
    env.write("b/copy.jpg.xmp", b"<x/>");
    env.write("b/.DS_Store", b"junk");
    let out = env.run(&["--move", "--rmdirs", "--io-jobs", "1"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(env.in_tree().is_empty(), "{:?}", env.in_tree());
    assert!(!env.input.join("a").exists() && !env.input.join("b").exists());
    assert_eq!(
        env.out_tree(),
        [DST, "duplicates/b/copy.jpg", "duplicates/b/copy.jpg.xmp"]
    );
    let err = stderr(&out);
    assert!(
        err.contains("duplicates:       1 (moved to duplicates/: 1)"),
        "{err}"
    );
    assert!(
        err.contains("folders removed:  2 (junk files removed 1)"),
        "{err}"
    );
}

/// I9.
#[test]
fn dupdir_collisions_get_suffixes() {
    let env = Env::new();
    env.add("dto.jpg", "a/one.jpg");
    env.add("dto.jpg", "b/copy.jpg");
    env.write("b/copy.jpg.xmp", b"<x/>");
    let dups = env.output.join("duplicates/b");
    fs::create_dir_all(&dups).unwrap();
    fs::write(dups.join("copy.jpg"), b"someone else's copy.jpg").unwrap();
    fs::write(dups.join("copy-2.jpg.xmp"), b"stale").unwrap();
    let out = env.run(&["--io-jobs", "1"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        env.out_tree(),
        [
            DST,
            "duplicates/b/copy-2.jpg.xmp",
            "duplicates/b/copy-3.jpg",
            "duplicates/b/copy-3.jpg.xmp",
            "duplicates/b/copy.jpg",
        ]
    );
    assert_eq!(
        fs::read(dups.join("copy.jpg")).unwrap(),
        b"someone else's copy.jpg"
    );
    assert_eq!(fs::read(dups.join("copy-2.jpg.xmp")).unwrap(), b"stale");
}

/// I11.
#[test]
fn dupdir_files_are_never_keepers_or_reprocessed() {
    // Identical only to a file in the duplicates folder: a normal transfer.
    let env = Env::new();
    env.add("dto.jpg", "dto.jpg");
    let parked = env.output.join("duplicates/old/dto.jpg");
    fs::create_dir_all(parked.parent().unwrap()).unwrap();
    fs::copy(common::fixture("dto.jpg"), &parked).unwrap();
    let out = env.run(&[]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(env.out_tree(), [DST, "duplicates/old/dto.jpg"]);

    // Organizing in place never scans the duplicates folder, even when
    // its name is spelled differently on disk (where the volume is
    // case-insensitive, e.g. macOS; elsewhere the canonical spelling).
    let env = Env::new();
    env.add("dto.jpg", "x/one.jpg");
    env.add("dto.jpg", "Duplicates/earlier/two.jpg");
    let parked = if env.input.join("duplicates").exists() {
        "Duplicates/earlier/two.jpg"
    } else {
        fs::rename(env.input.join("Duplicates"), env.input.join("duplicates")).unwrap();
        "duplicates/earlier/two.jpg"
    };
    let first = in_place_cmd(&env, &["--move"]);
    assert_eq!(first.status.code(), Some(0), "{}", stderr(&first));
    assert_eq!(env.in_tree(), [DST, parked]);
    let second = in_place_cmd(&env, &["--move"]);
    assert_eq!(second.status.code(), Some(0), "{}", stderr(&second));
    assert_eq!(env.in_tree(), [DST, parked]);
}

/// I12.
#[test]
fn custom_duplicates_dir() {
    let env = Env::new();
    env.add("dto.jpg", "a/one.jpg");
    env.add("dto.jpg", "b/copy.jpg");
    let out = env.run(&["--duplicates-dir", "Dups 2024"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(env.out_tree(), [DST, "Dups 2024/b/copy.jpg"]);
    assert!(stderr(&out).contains("duplicates:       1 (copied to Dups 2024/: 1)"));
}

/// I13.
#[test]
fn skip_duplicates_reports_only() {
    let env = Env::new();
    env.add("dto.jpg", "a/one.jpg");
    let copy = env.add("dto.jpg", "b/copy.jpg");
    let side = env.write("b/copy.xmp", b"<x/>");
    let out = env.run(&["--move", "--skip-duplicates"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(env.out_tree(), [DST]);
    assert!(copy.exists() && side.exists(), "duplicate and sidecar stay");
    let lines = sorted_lines(&out);
    let want = [
        format!("  sidecar left in place {}", side.display()),
        format!(
            "dup {} == {}",
            copy.display(),
            env.output.join(DST).display()
        ),
    ];
    for w in want {
        assert!(lines.contains(&w), "{w}: {lines:?}");
    }
    assert!(stderr(&out).contains("duplicates:       1 (left in place)"));
}

/// I14.
#[test]
fn in_place_default_files_duplicates_and_is_stable() {
    let env = Env::new();
    env.add("dto.jpg", "x/one.jpg");
    env.add("dto.jpg", "y/copy.jpg");
    env.add("a.heic", "z/deep/two.heic");
    let first = in_place_cmd(&env, &["--move", "--rmdirs"]);
    assert_eq!(first.status.code(), Some(0), "{}", stderr(&first));
    let organized = env.in_tree();
    assert_eq!(
        organized,
        [
            DST,
            "2019/03/04/20190304-050607.heic",
            "duplicates/y/copy.jpg"
        ]
        .iter()
        .map(|s| s.to_string())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
    );
    let second = in_place_cmd(&env, &["--move", "--rmdirs"]);
    assert_eq!(second.status.code(), Some(0), "{}", stderr(&second));
    let lines = sorted_lines(&second);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(
        lines.iter().all(|l| l.starts_with("in-place ")),
        "{lines:?}"
    );
    assert_eq!(env.in_tree(), organized, "duplicates folder untouched");
}

/// I15.
#[test]
fn dry_run_equals_real_run_with_duplicates_folder() {
    let env = Env::new();
    env.add("dto.jpg", "a/one.jpg");
    env.add("dto.jpg", "b/copy.jpg");
    env.write("b/copy.jpg.xmp", b"<x/>");
    let (in_before, out_before) = (env.in_tree(), env.out_tree());
    let dry = env.run(&["--move", "--rmdirs", "--dry-run", "--io-jobs", "1"]);
    assert_eq!(dry.status.code(), Some(0), "{}", stderr(&dry));
    assert_eq!((env.in_tree(), env.out_tree()), (in_before, out_before));
    assert!(!env.output.exists(), "dry-run creates nothing");
    let mut predicted: Vec<String> = sorted_lines(&dry)
        .iter()
        .map(|l| {
            l.strip_prefix("[dry-run] would ")
                .expect("prefixed")
                .to_owned()
        })
        .collect();
    predicted.sort();
    assert!(
        predicted.iter().any(|l| l.starts_with("dup-move ")),
        "{predicted:?}"
    );
    assert!(
        predicted.iter().any(|l| l.starts_with("rmdir ")),
        "{predicted:?}"
    );
    let real = env.run(&["--move", "--rmdirs", "--io-jobs", "1"]);
    assert_eq!(real.status.code(), Some(0), "{}", stderr(&real));
    assert_eq!(predicted, sorted_lines(&real));
}

/// I17.
#[test]
fn duplicate_flag_errors_exit_2_and_touch_nothing() {
    let env = Env::new();
    env.add("dto.jpg", "dto.jpg");
    for bad in [
        &["--skip-duplicates", "--duplicates-dir", "d"][..],
        &["--move", "--skip-duplicates", "--delete-duplicates"],
        &["--move", "--duplicates-dir", "d", "--delete-duplicates"],
        &["--duplicates-dir", ""],
        &["--duplicates-dir", ".."],
        &["--duplicates-dir", "a/b"],
        &["--prefix", "duplicates/2024"],
        &["--no-date-dir", "Duplicates"],
    ] {
        let out = env.run(bad);
        assert_eq!(out.status.code(), Some(2), "{bad:?}: {}", stderr(&out));
        assert!(!env.output.exists(), "{bad:?} must not create the output");
    }

    // The duplicates folder is a file, or INPUT lies inside it.
    fs::create_dir_all(&env.output).unwrap();
    fs::write(env.output.join("duplicates"), b"file").unwrap();
    let out = env.run(&[]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert_eq!(common::tree(&env.output), ["duplicates"]);

    let inner = Env::new();
    let input = inner.output.join("duplicates/sub");
    fs::create_dir_all(&input).unwrap();
    fs::copy(common::fixture("dto.jpg"), input.join("dto.jpg")).unwrap();
    let out = assert_cmd::Command::cargo_bin("oxisort")
        .unwrap()
        .arg(&input)
        .arg(&inner.output)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("inside the duplicates folder"),
        "{}",
        stderr(&out)
    );
    assert_eq!(common::tree(&inner.output), ["duplicates/sub/dto.jpg"]);
}

/// U20 end to end: a date format rendering into the duplicates folder.
#[test]
fn date_folder_inside_dupdir_fails_the_file() {
    let env = Env::new();
    let src = env.add("dto.jpg", "dto.jpg");
    let out = env.run(&["-d", "duplicates/YYYY"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("inside the duplicates folder"),
        "{}",
        stderr(&out)
    );
    assert!(src.exists());
    assert!(env.out_tree().is_empty());
}

/// The default-policy twin of `failed_move_keeps_its_in_input_duplicate`:
/// B is not filed when A's move failed, so its content isn't only in the
/// duplicates folder.
#[test]
fn failed_move_keeps_its_duplicate_out_of_the_dupdir() {
    let env = Env::new();
    let a = env.add("dto.jpg", "a/one.jpg");
    let b = env.add("dto.jpg", "b/copy.jpg");
    let cfg = config(&[
        "--move".as_ref(),
        env.input.as_os_str(),
        env.output.as_os_str(),
    ]);
    let pools = oxisort::Pools::new(2, 2).unwrap();
    let progress = Progress::disabled();
    let (_, plan) = oxisort::plan_run(&cfg, &pools, &Stop::new(), &progress);
    let dst = env.output.join(DST);
    fs::create_dir_all(dst.parent().unwrap()).unwrap();
    fs::write(&dst, b"someone else's file").unwrap();
    let outcomes = exec::execute(
        &plan,
        cfg.mode,
        Default::default(),
        &pools.io,
        &Stop::new(),
        &progress,
    )
    .outcomes;
    assert!(matches!(outcomes[0], Outcome::Failed(_)), "{outcomes:?}");
    assert_eq!(
        outcomes[1],
        Outcome::NotRun(oxisort::model::NotRun::DependencyFailed)
    );
    assert!(a.exists() && b.exists());
    // Step 5a creates the folder up front; nothing may land in it.
    assert!(common::tree(&env.output.join("duplicates")).is_empty());
}

/// Review finding L2R1-1: the kept copy in OUT is rewritten in place after
/// planning with the same size (new bytes, new mtime). The input copy must
/// survive `--delete-duplicates`.
#[test]
fn delete_keeps_source_when_kept_copy_is_rewritten_after_planning() {
    let env = Env::new();
    let src = env.add("dto.jpg", "a/copy.jpg");
    let kept = env.output.join("2017/elsewhere.jpg");
    fs::create_dir_all(kept.parent().unwrap()).unwrap();
    fs::copy(common::fixture("dto.jpg"), &kept).unwrap();
    let cfg = config(&[
        "--move".as_ref(),
        "--delete-duplicates".as_ref(),
        env.input.as_os_str(),
        env.output.as_os_str(),
    ]);
    let pools = oxisort::Pools::new(2, 2).unwrap();
    let progress = Progress::disabled();
    let (_, plan) = oxisort::plan_run(&cfg, &pools, &Stop::new(), &progress);

    let mut bytes = fs::read(&kept).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    fs::write(&kept, &bytes).unwrap();
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
    fs::File::options()
        .write(true)
        .open(&kept)
        .unwrap()
        .set_modified(later)
        .unwrap();

    let outcomes = exec::execute(
        &plan,
        cfg.mode,
        Default::default(),
        &pools.io,
        &Stop::new(),
        &progress,
    )
    .outcomes;
    assert!(
        matches!(&outcomes[0], Outcome::Failed(m) if m.contains("changed since planning")),
        "{outcomes:?}"
    );
    assert_eq!(
        fs::read(&src).unwrap(),
        fs::read(common::fixture("dto.jpg")).unwrap()
    );
}

/// Review finding L2R1-3: with nothing to place, OUT isn't walked. The
/// probe: an unreadable OUT folder is only reported by the walk.
#[test]
fn out_walk_is_skipped_when_no_input_can_be_a_duplicate() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new();
    env.add("dto.jpg", "a.jpg");
    let locked = env.output.join("locked");
    fs::create_dir_all(&locked).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let filtered = env.run(&["--file-type", "video"]);
    let walked = env.run(&["--dry-run"]);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(filtered.status.code(), Some(0), "{}", stderr(&filtered));
    assert!(
        !stderr(&filtered).contains("not checked for duplicates"),
        "{}",
        stderr(&filtered)
    );
    // Control: with a candidate, the walk runs and reports the folder.
    assert!(
        stderr(&walked).contains("not checked for duplicates"),
        "{}",
        stderr(&walked)
    );
}
