//! Safety guards end to end: OUTPUT == INPUT refusal (F7), and later the
//! crash-safety and verification behaviour.

mod common;

use std::fs;
use std::path::Path;

use common::*;

/// Every file under `root` with its bytes, sorted: a cheap manifest.
fn snapshot(root: &Path) -> Vec<(String, Vec<u8>)> {
    tree(root)
        .into_iter()
        .map(|rel| {
            let bytes = fs::read(root.join(&rel)).unwrap_or_default();
            (rel, bytes)
        })
        .collect()
}

/// Runs oxisort with explicit INPUT and OUTPUT arguments.
fn run_paths(env: &Env, extra: &[&str], input: &Path, output: &Path) -> std::process::Output {
    assert_cmd::Command::cargo_bin("oxisort")
        .unwrap()
        .env("TZ", "UTC")
        .args(["--tz", "+00:00"])
        .args(extra)
        .arg(input)
        .arg(output)
        .current_dir(env.dir.path())
        .output()
        .unwrap()
}

#[test]
fn output_equal_to_input_is_refused_for_copy_link_symlink() {
    let env = Env::new();
    env.add("dto.jpg", "dto.jpg");
    env.add("a.heic", "sub/a.heic");
    let alias = env.dir.path().join("alias");
    std::os::unix::fs::symlink(&env.input, &alias).unwrap();
    let before = snapshot(&env.input);

    let mut spellings = vec![env.input.clone(), env.input.join("sub/.."), alias];
    // Another case spelling names the same folder only on a
    // case-insensitive volume (APFS default); skip it elsewhere.
    let upper = env.input.with_file_name("IN");
    if upper.exists() {
        spellings.push(upper);
    }
    for mode in [&[][..], &["-l"], &["-s"]] {
        for dry in [false, true] {
            for out in &spellings {
                let mut extra = mode.to_vec();
                if dry {
                    extra.push("-n");
                }
                let o = run_paths(&env, &extra, &env.input, out);
                assert_eq!(
                    o.status.code(),
                    Some(2),
                    "{extra:?} {out:?}: {}",
                    stderr(&o)
                );
                assert!(stderr(&o).contains("is the input folder"), "{}", stderr(&o));
                assert_eq!(snapshot(&env.input), before, "{extra:?} {out:?}");
            }
        }
    }
}

#[test]
fn move_in_place_is_still_allowed() {
    let env = Env::new();
    env.add("dto.jpg", "dto.jpg");
    let o = run_paths(&env, &["-m"], &env.input, &env.input);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(env.in_tree(), ["2017/01/02/20170102-030405123.jpg"]);
}

#[test]
fn refusal_happens_before_the_log_file_is_created() {
    let env = Env::new();
    env.add("dto.jpg", "dto.jpg");
    let log = env.input.join("x.log");
    let o = run_paths(
        &env,
        &["--log", log.to_str().unwrap()],
        &env.input,
        &env.input,
    );
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
    assert!(!log.exists(), "nothing may be touched");
}

/// Issue #1: temp files from an interrupted run, in INPUT or OUTPUT, are
/// listed in one warning, left untouched, and never sorted or filed.
#[test]
fn leftover_temp_files_are_reported_and_never_touched() {
    let env = Env::new();
    env.add("dto.jpg", "dto.jpg");
    let in_left = env.write(".oxisort-tmp-2-0-0", b"partial input");
    let out_dir = env.output.join("2017/01/02");
    fs::create_dir_all(&out_dir).unwrap();
    let out_left = out_dir.join(".oxisort-tmp-1-0-0");
    fs::write(&out_left, b"partial output").unwrap();

    let o = env.run(&[]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let err = stderr(&o);
    assert!(
        err.contains("2 leftover temp files from an interrupted run"),
        "{err}"
    );
    assert!(err.contains(&in_left.display().to_string()), "{err}");
    assert!(err.contains(&out_left.display().to_string()), "{err}");
    assert!(err.contains("leftover temp files 2"), "{err}");
    assert_eq!(fs::read(&in_left).unwrap(), b"partial input");
    assert_eq!(fs::read(&out_left).unwrap(), b"partial output");
    assert_eq!(
        env.out_tree(),
        [
            "2017/01/02/.oxisort-tmp-1-0-0",
            "2017/01/02/20170102-030405123.jpg"
        ]
    );
}

/// A source folder holding only a leftover is not "empty" for --rmdirs.
#[test]
fn rmdirs_keeps_a_folder_holding_a_leftover() {
    let env = Env::new();
    env.add("dto.jpg", "sub/dto.jpg");
    let left = env.write("sub/.oxisort-tmp-3-0-0", b"partial");
    env.write("sub/.DS_Store", b"junk");
    let o = env.run(&["-m", "--rmdirs"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(fs::read(&left).unwrap(), b"partial");
    assert_eq!(env.out_tree(), ["2017/01/02/20170102-030405123.jpg"]);
}

/// Issue #2: deleting a duplicate re-hashes both files, and the summary
/// says so; `--no-verify` says verification was skipped.
#[test]
fn delete_duplicates_reports_verification() {
    for (extra, line) in [
        (&[][..], "verification:     1 files checked (blake3)"),
        (
            &["--no-verify"][..],
            "verification:     skipped (--no-verify)",
        ),
    ] {
        let env = Env::new();
        env.add("dto.jpg", "a/dto.jpg");
        env.add("dto.jpg", "b/dto.jpg");
        let mut args = vec!["-m", "--delete-duplicates"];
        args.extend_from_slice(extra);
        let o = env.run(&args);
        assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
        assert!(stderr(&o).contains(line), "{}", stderr(&o));
        assert!(stderr(&o).contains("(deleted 1)"), "{}", stderr(&o));
        assert!(env.in_tree().is_empty(), "{:?}", env.in_tree());
    }
}

/// A plain copy run deletes nothing, so it has no verification line.
#[test]
fn copy_run_has_no_verification_line() {
    let env = Env::new();
    env.add("dto.jpg", "dto.jpg");
    let o = env.run(&[]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert!(!stderr(&o).contains("verification:"), "{}", stderr(&o));
}
