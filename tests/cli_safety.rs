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
