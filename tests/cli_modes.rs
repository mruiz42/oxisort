//! Integration tests: move, hard link, symlink, plus (later slices)
//! sidecars, filters, --delete-duplicates and --rmdirs.

mod common;

use std::fs;
use std::os::unix::fs::MetadataExt;

use common::{Env, fixture, stderr, stdout};

const DST: &str = "2017/01/02/20170102-030405123.jpg";

#[test]
fn move_removes_the_source() {
    let env = Env::new();
    let src = env.add("dto.jpg", "sub/dto.jpg");
    let out = env.run(&["--move"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(!src.exists());
    assert_eq!(env.out_tree(), [DST]);
    assert_eq!(
        fs::read(env.output.join(DST)).unwrap(),
        fs::read(fixture("dto.jpg")).unwrap()
    );
    assert!(stdout(&out).starts_with("move "));
    assert!(
        env.input.join("sub").exists(),
        "folders stay without --rmdirs"
    );
}

#[test]
fn hard_link_shares_the_inode() {
    let env = Env::new();
    let src = env.add("dto.jpg", "dto.jpg");
    let out = env.run(&["--link"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let ino = |p: &std::path::Path| fs::metadata(p).unwrap().ino();
    assert_eq!(ino(&src), ino(&env.output.join(DST)));
    // Re-running finds the link already in place.
    let again = env.run(&["--link"]);
    assert!(
        stdout(&again).starts_with("in-place "),
        "{}",
        stdout(&again)
    );
}

#[test]
fn symlink_points_to_absolute_source() {
    let env = Env::new();
    let src = env.add("dto.jpg", "dto.jpg");
    let out = env.run(&["--symlink"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let link = env.output.join(DST);
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read_link(&link).unwrap(), src);
    assert!(src.exists());
    let again = env.run(&["--symlink"]);
    assert!(
        stdout(&again).starts_with("in-place "),
        "{}",
        stdout(&again)
    );
    assert_eq!(env.out_tree(), [DST]);
}

#[test]
fn move_dry_run_changes_nothing() {
    let env = Env::new();
    let src = env.add("dto.jpg", "dto.jpg");
    let out = env.run(&["--move", "--dry-run"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(src.exists());
    assert!(!env.output.exists());
    assert!(stdout(&out).starts_with("[dry-run] would move "));
}
