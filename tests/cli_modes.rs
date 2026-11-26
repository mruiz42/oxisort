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

#[test]
fn sidecars_follow_their_media_with_collision_suffix() {
    let env = Env::new();
    // Sorted walk: A/first.arw is planned before IMG_1.ARW, so the latter
    // (and its sidecar) get -2.
    let mut first = fs::read(fixture("fake.arw")).unwrap();
    first.push(1);
    env.write("A/first.arw", &first);
    env.add("fake.arw", "IMG_1.ARW");
    env.write("IMG_1.ARW.xmp", b"<xmp with ext/>");
    // a.tiff is byte-identical to fake.arw; keep it distinct so this is
    // two different files sharing IMG_1.xmp, not a duplicate.
    let mut tiff = fs::read(fixture("a.tiff")).unwrap();
    tiff.push(2);
    env.write("IMG_1.JPG", &tiff);
    env.write("IMG_1.xmp", b"<shared stem/>");
    env.add("dto.jpg", "solo.jpg");
    env.write("solo.XMP", b"<stem only/>");
    env.write("orphan.xmp", b"<nobody/>");

    let out = env.run(&["--io-jobs", "1"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        env.out_tree(),
        [
            "2017/01/02/20170102-030405123.jpg",
            "2017/01/02/20170102-030405123.xmp",
            "2019/03/04/20190304-050607-2.arw",
            "2019/03/04/20190304-050607-2.arw.xmp",
            "2019/03/04/20190304-050607.arw",
            "2019/03/04/20190304-050607.jpg",
        ]
    );
    assert_eq!(
        fs::read(env.output.join("2019/03/04/20190304-050607-2.arw.xmp")).unwrap(),
        b"<xmp with ext/>"
    );
    let err = stderr(&out);
    assert!(
        err.contains("IMG_1.xmp matches 2 files; left in place"),
        "{err}"
    );
    assert!(
        err.contains("orphan sidecars 1, shared sidecars 1"),
        "{err}"
    );
    assert!(stdout(&out).contains("  + "), "sidecar lines are indented");
}

#[test]
fn moved_sidecars_leave_the_input() {
    let env = Env::new();
    let src = env.add("dto.jpg", "x/dto.jpg");
    let side = env.write("x/dto.jpg.xmp", b"<x/>");
    let out = env.run(&["--move"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(!src.exists() && !side.exists());
    assert!(
        env.output
            .join("2017/01/02/20170102-030405123.jpg.xmp")
            .exists()
    );
}

#[test]
fn filters() {
    let env = Env::new();
    for f in [
        "dto.jpg",
        "cd_only.jpg",
        "a.heic",
        "base.jpg",
        "apple.mov",
        "notes.txt",
    ] {
        env.add(f, f);
    }
    let out = env.run(&[
        "--from",
        "2017-01-02",
        "--to",
        "2018-02-03",
        "--file-type",
        "image",
    ]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        env.out_tree(),
        [
            "2017/01/02/20170102-030405123.jpg", // boundary day: included
            "2018/02/03/20180203-040506.jpg",    // boundary day: included
            "unknown/base.jpg",                  // undated: not range-filtered
        ]
    );
    let err = stderr(&out);
    assert!(
        err.contains("skipped:          type 2, date range 1, unknown 0"),
        "{err}"
    );

    let env2 = Env::new();
    env2.add("dto.jpg", "dto.jpg");
    env2.add("base.jpg", "base.jpg");
    let out = env2.run(&["--skip-unknown"]);
    assert_eq!(env2.out_tree(), ["2017/01/02/20170102-030405123.jpg"]);
    assert!(common::stdout(&out).contains("(no date)"));
}

#[test]
fn library_packages_and_system_folders_are_untouched() {
    let env = Env::new();
    let inside = env.add("dto.jpg", "Photos.photoslibrary/originals/a.jpg");
    let trash = env.add("dto.jpg", ".Trashes/501/b.jpg");
    env.add("a.heic", "real.heic");
    let out = env.run(&["--move"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(inside.exists() && trash.exists());
    assert_eq!(env.out_tree(), ["2019/03/04/20190304-050607.heic"]);
}

#[test]
fn max_depth_limits_the_walk() {
    let env = Env::new();
    env.add("dto.jpg", "top.jpg");
    env.add("a.heic", "sub/deep.heic");
    let out = env.run(&["--max-depth", "0"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(env.out_tree(), ["2017/01/02/20170102-030405123.jpg"]);
}

#[test]
fn delete_duplicates_only_verified_and_never_same_inode() {
    let env = Env::new();
    let a = env.add("dto.jpg", "a/one.jpg");
    let copy = env.add("dto.jpg", "b/copy.jpg");
    let link = env.input.join("c/hardlink.jpg");
    fs::create_dir_all(link.parent().unwrap()).unwrap();
    // A hard link to the first file: same inode, so never deleted.
    fs::hard_link(&a, &link).unwrap();
    let out = env.run(&["--move", "--delete-duplicates", "--io-jobs", "1"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(!a.exists(), "moved");
    assert!(!copy.exists(), "verified duplicate deleted");
    assert!(link.exists(), "same inode kept");
    assert_eq!(env.out_tree(), [DST]);
    assert!(stdout(&out).contains("dup-deleted "));

    // Without the flag, duplicates are moved into the duplicates folder.
    let env2 = Env::new();
    env2.add("dto.jpg", "a/one.jpg");
    let dup = env2.add("dto.jpg", "b/copy.jpg");
    assert_eq!(env2.run(&["--move"]).status.code(), Some(0));
    assert!(!dup.exists());
    assert_eq!(env2.out_tree(), [DST, "duplicates/b/copy.jpg"]);
}

#[test]
fn delete_duplicates_against_existing_output() {
    let env = Env::new();
    env.add("dto.jpg", "first/dto.jpg");
    assert_eq!(env.run(&["--move"]).status.code(), Some(0));
    let again = env.add("dto.jpg", "second/dto.jpg");
    let out = env.run(&["--move", "--delete-duplicates"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(!again.exists());
    assert_eq!(env.out_tree(), [DST]);
}

#[test]
fn rmdirs_removes_emptied_and_junk_only_folders() {
    let env = Env::new();
    env.add("dto.jpg", "emptied/x.jpg");
    env.write("emptied/.DS_Store", b"junk");
    env.add("a.heic", "nested/deeper/y.heic");
    env.add("cd_only.jpg", "kept/z.jpg");
    env.write("kept/notes.xmp", b"orphan stays");
    env.add("base.jpg", "top.jpg");

    let dry = env.run(&["--move", "--rmdirs", "--dry-run"]);
    assert_eq!(dry.status.code(), Some(0));
    let mut dry_rm: Vec<String> = common::sorted_lines(&dry)
        .into_iter()
        .filter(|l| l.contains("rmdir") || l.contains("   - "))
        .map(|l| l.replace("[dry-run] would ", ""))
        .collect();
    dry_rm.sort();

    let out = env.run(&["--move", "--rmdirs"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(env.in_tree(), ["kept/notes.xmp"]);
    assert!(env.input.exists(), "INPUT itself is never removed");
    let mut real_rm: Vec<String> = common::sorted_lines(&out)
        .into_iter()
        .filter(|l| l.contains("rmdir") || l.starts_with("  - "))
        .collect();
    real_rm.sort();
    assert_eq!(dry_rm, real_rm, "dry-run predicts the removals");
    assert_eq!(real_rm.iter().filter(|l| l.starts_with("rmdir")).count(), 3);
    assert!(stderr(&out).contains("folders removed:  3 (junk files removed 1)"));
}

/// I18 (decision A1-Q1 = B): duplicates are filed in the run's own mode.
#[test]
fn link_and_symlink_duplicates_follow_q1() {
    let env = Env::new();
    env.add("dto.jpg", "a/one.jpg");
    let copy = env.add("dto.jpg", "b/copy.jpg");
    let out = env.run(&["--link", "--io-jobs", "1"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let filed = env.output.join("duplicates/b/copy.jpg");
    let ino = |p: &std::path::Path| fs::metadata(p).unwrap().ino();
    assert_eq!(ino(&filed), ino(&copy), "a hard link, no bytes copied");
    assert!(stdout(&out).contains("dup-link "), "{}", stdout(&out));
    assert!(stderr(&out).contains("(linked into duplicates/: 1)"));

    let env = Env::new();
    env.add("dto.jpg", "a/one.jpg");
    let copy = env.add("dto.jpg", "b/copy.jpg");
    let out = env.run(&["--symlink", "--io-jobs", "1"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let filed = env.output.join("duplicates/b/copy.jpg");
    assert!(
        fs::symlink_metadata(&filed)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read_link(&filed).unwrap(), copy);
    assert!(stderr(&out).contains("(symlinked into duplicates/: 1)"));
}

/// I18 (decision A1-Q2 = A): a second --link run writes nothing. The kept
/// file's inode is in OUT, so it is in place; its duplicate is already
/// linked into the duplicates folder.
#[test]
fn link_rerun_is_in_place() {
    let env = Env::new();
    env.add("dto.jpg", "a/one.jpg");
    env.add("dto.jpg", "b/copy.jpg");
    assert_eq!(env.run(&["--link"]).status.code(), Some(0));
    let before = env.out_tree();
    let again = env.run(&["--link", "--io-jobs", "1"]);
    assert_eq!(again.status.code(), Some(0), "{}", stderr(&again));
    assert_eq!(env.out_tree(), before, "nothing new written");
    let lines = common::sorted_lines(&again);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(
        lines.iter().any(|l| l.starts_with("in-place ")),
        "{lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("(already in ")),
        "{lines:?}"
    );
}

/// F3: the move-only flags are usage errors with --link/--symlink, and such
/// a run touches neither INPUT nor OUTPUT.
#[test]
fn move_only_flags_refused_with_link_and_symlink() {
    let env = Env::new();
    env.add("dto.jpg", "a/dto.jpg");
    env.add("dto.jpg", "b/renamed_copy.jpg");
    let before = env.in_tree();
    for mode in ["-l", "-s"] {
        for flag in ["--delete-duplicates", "--rmdirs"] {
            let out = env.run(&[mode, flag]);
            assert_eq!(
                out.status.code(),
                Some(2),
                "{mode} {flag}: {}",
                stderr(&out)
            );
            assert_eq!(env.in_tree(), before, "{mode} {flag} touched INPUT");
            assert!(!env.output.exists(), "{mode} {flag} created OUTPUT");
        }
    }
}
