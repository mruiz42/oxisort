//! Shared helpers for the integration tests: a temp input/output tree built
//! from `tests/fixtures`, a binary runner that pins the time zone, and a
//! sorted listing of the output tree.

#![allow(dead_code)] // each test file uses a different subset

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use assert_cmd::Command;

/// A fresh temp folder with `in/` (populated by the test) and `out/`.
pub struct Env {
    pub dir: tempfile::TempDir,
    pub input: PathBuf,
    pub output: PathBuf,
}

pub fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

impl Env {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        // Canonical, so paths printed by oxisort match what tests build.
        let root = fs::canonicalize(dir.path()).expect("canonicalize");
        let input = root.join("in");
        fs::create_dir(&input).expect("mkdir in");
        Env {
            output: root.join("out"),
            input,
            dir,
        }
    }

    /// Copies a fixture into the input tree at `rel` (mtime preserved).
    pub fn add(&self, fixture_name: &str, rel: &str) -> PathBuf {
        let dst = self.input.join(rel);
        fs::create_dir_all(dst.parent().unwrap()).unwrap();
        fs::copy(fixture(fixture_name), &dst).unwrap();
        dst
    }

    /// Writes arbitrary bytes into the input tree at `rel`.
    pub fn write(&self, rel: &str, bytes: &[u8]) -> PathBuf {
        let dst = self.input.join(rel);
        fs::create_dir_all(dst.parent().unwrap()).unwrap();
        fs::write(&dst, bytes).unwrap();
        dst
    }

    /// The oxisort command with `TZ=UTC` and `--tz +00:00` pinned.
    pub fn cmd(&self, extra: &[&str]) -> Command {
        let mut c = self.cmd_unpinned(extra, "UTC");
        c.args(["--tz", "+00:00"]);
        c
    }

    /// The oxisort command with only `TZ` set (exercises `Zone::System`).
    pub fn cmd_unpinned(&self, extra: &[&str], tz: &str) -> Command {
        let mut c = Command::cargo_bin("oxisort").unwrap();
        c.env("TZ", tz)
            .args(extra)
            .arg(&self.input)
            .arg(&self.output);
        c
    }

    pub fn run(&self, extra: &[&str]) -> Output {
        self.cmd(extra).output().unwrap()
    }

    /// Sorted relative paths of every file and symlink under the output.
    pub fn out_tree(&self) -> Vec<String> {
        tree(&self.output)
    }

    pub fn in_tree(&self) -> Vec<String> {
        tree(&self.input)
    }
}

pub fn tree(root: &Path) -> Vec<String> {
    let mut v: Vec<String> = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| !e.file_type().is_dir())
        .map(|e| e.path().strip_prefix(root).unwrap().display().to_string())
        .collect();
    v.sort();
    v
}

pub fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

pub fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// stdout lines, sorted (order is nondeterministic with --io-jobs > 1).
pub fn sorted_lines(o: &Output) -> Vec<String> {
    let mut v: Vec<String> = stdout(o).lines().map(str::to_owned).collect();
    v.sort();
    v
}
