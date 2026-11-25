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

const ALL_FIXTURES: &[&str] = &[
    "20160915_123456.jpg",
    "IMG_20160915_123456.jpg",
    "a.3gp",
    "a.heic",
    "a.mkv",
    "a.orf",
    "a.png",
    "a.tiff",
    "a.webp",
    "apple.mov",
    "base.jpg",
    "cd_only.jpg",
    "dto.jpg",
    "fake.arw",
    "modify_only.jpg",
    "mvhd_only.mp4",
    "nodate.mp4",
    "notes.txt",
    "zero_date.jpg",
];

#[test]
fn every_fixture_lands_in_the_right_place() {
    let env = Env::new();
    for f in ALL_FIXTURES {
        env.add(f, f);
    }
    let out = env.run(&["--io-jobs", "1"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        env.out_tree(),
        [
            // Sorted input order: "2…" before "I…", so the bare name wins.
            "2016/09/15/20160915-123456-2.jpg",
            "2016/09/15/20160915-123456.jpg",
            "2017/01/02/20170102-030405123.jpg", // DTO + subsec, offset ignored
            "2018/02/03/20180203-040506.jpg",    // CreateDate only
            "2018/03/04/20180304-050607.jpg",    // ModifyDate only
            "2019/03/04/20190304-050607.arw",    // TIFF data, RAW extension
            "2019/03/04/20190304-050607.heic",
            "2019/03/04/20190304-050607.png",
            "2019/03/04/20190304-050607.tiff",
            "2020/05/06/20200506-070809.3gp",
            "2020/05/06/20200506-070809.mkv", // "…Z" date parsed
            "2021/06/07/20210607-080910.mov", // Apple key wall clock
            "2022/07/08/20220708-021011.mp4", // mvhd UTC with --tz +00:00
            "2023/01/02/20230102-030405.webp",
            "unknown/a.orf", // no EXIF read for ORF (decision Q7)
            "unknown/base.jpg",
            "unknown/nodate.mp4", // mvhd = 0 → 1904 → no date
            "unknown/notes.txt",
            "unknown/zero_date.jpg",
        ]
    );
    // The filename-dated pair must not be mixed up.
    assert_eq!(
        fs::read(env.output.join("2016/09/15/20160915-123456.jpg")).unwrap(),
        fs::read(fixture("20160915_123456.jpg")).unwrap()
    );
    let err = stderr(&out);
    assert!(err.contains("(images 13, videos 5, other 1)"), "{err}");
    assert!(err.contains("no date:          5"), "{err}");
}

#[test]
fn tz_override_shifts_only_utc_sources() {
    let env = Env::new();
    for f in ["apple.mov", "mvhd_only.mp4", "dto.jpg"] {
        env.add(f, f);
    }
    let out = env
        .cmd_unpinned(&["--tz", "+02:00"], "UTC")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        env.out_tree(),
        [
            "2017/01/02/20170102-030405123.jpg",
            "2021/06/07/20210607-080910.mov",
            "2022/07/08/20220708-041011.mp4",
        ]
    );
}

#[test]
fn system_zone_is_dst_correct() {
    // No --tz: mvhd 2022-07-08 02:10:11Z in New York (EDT, UTC-4) is the
    // evening before.
    let env = Env::new();
    env.add("mvhd_only.mp4", "v.mp4");
    env.add("apple.mov", "a.mov");
    let out = env.cmd_unpinned(&[], "America/New_York").output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        env.out_tree(),
        [
            "2021/06/07/20210607-080910.mov",
            "2022/07/07/20220707-221011.mp4"
        ]
    );
}

#[test]
fn date_field_order_and_selection() {
    let env = Env::new();
    env.add("dto.jpg", "dto.jpg");
    env.add("apple.mov", "apple.mov");
    let out = env.run(&["--date-field", "CreateDate,DateTimeOriginal"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        env.out_tree(),
        ["2017/01/02/20170102-030406.jpg", "unknown/apple.mov"],
        "CreateDate wins; videos without MediaCreateDate have no date"
    );
}

#[test]
fn custom_regex_and_timestamp() {
    let env = Env::new();
    env.add("base.jpg", "holiday 15.09.2016.jpg");
    let plain = env.add("base.jpg", "plain.jpg");
    env.write("IMG_20160915_123456.txt", b"not media");
    let t = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_420_167_845);
    fs::File::options()
        .write(true)
        .open(&plain)
        .unwrap()
        .set_modified(t)
        .unwrap();

    let out = env.run(&[
        "--regex",
        r"(?P<day>\d{2})\.(?P<month>\d{2})\.(?P<year>\d{4})",
        "--timestamp",
    ]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        env.out_tree(),
        [
            "2015/01/02/20150102-030405.jpg",  // mtime, --tz +00:00
            "2016/09/15/20160915-000000.jpg",  // custom regex, no time groups
            "unknown/IMG_20160915_123456.txt", // non-media never uses the name
        ]
    );
}
