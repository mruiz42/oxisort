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
    let (scanned, plan) = oxisort::plan_run(&cfg, &pools, &progress);

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
