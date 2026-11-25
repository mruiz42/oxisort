//! Integration tests: copy mode, naming, date sources, the no-date folder,
//! validation and exit codes.

mod common;

use std::fs;

use common::{Env, fixture, stderr, stdout};

#[test]
fn copies_dto_jpeg_with_subsec_name() {
    let env = Env::new();
    let src = env.add("dto.jpg", "dto.jpg");
    let out = env.run(&[]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(env.out_tree(), ["2017/01/02/20170102-030405123.jpg"]);
    let dst = env.output.join("2017/01/02/20170102-030405123.jpg");
    assert_eq!(
        fs::read(&dst).unwrap(),
        fs::read(fixture("dto.jpg")).unwrap()
    );
    let mtime = |p: &std::path::Path| fs::metadata(p).unwrap().modified().unwrap();
    assert_eq!(mtime(&dst), mtime(&src));
    assert!(src.exists(), "copy keeps the source");
    assert_eq!(
        stdout(&out).trim(),
        format!("copy {} -> {}", src.display(), dst.display())
    );
    assert!(stderr(&out).contains("transferred:      1 (images 1"));
}

#[test]
fn usage_errors_exit_2_and_touch_nothing() {
    let env = Env::new();
    env.add("dto.jpg", "dto.jpg");
    for bad in [
        &["--tz", "+99:00"][..],
        &["--delete-duplicates"],
        &["--move", "--link"],
        &["-d", "/abs"],
        &["--from", "2020-13-01"],
        &["--regex", "("],
        &["--date-field", "Nope"],
    ] {
        let out = env.cmd_unpinned(bad, "UTC").output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{bad:?}: {}", stderr(&out));
        assert!(!env.output.exists(), "{bad:?} must not create the output");
    }
}

#[test]
fn missing_input_is_usage_error() {
    let env = Env::new();
    let out = assert_cmd::Command::cargo_bin("oxisort")
        .unwrap()
        .arg(env.input.join("nope"))
        .arg(&env.output)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("input"));
}
