# Changelog

All notable changes to oxisort are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## Unreleased

### Changed
- Linux release binaries are now static musl builds for x86_64 and aarch64,
  replacing the glibc builds. They run on any Linux, including older
  distributions and Alpine.
- Copy, link and symlink modes refuse an OUTPUT that is the INPUT folder
  (compared by identity, so symlinks and other spellings are caught) and
  exit 2 before touching anything. They used to add a sorted second copy of
  the input to itself. `-m` still organizes a folder in place.

### Fixed
- Crash-safe copies: a data copy (including the copy half of a cross-volume
  move) is written to a hidden `.oxisort-tmp-*` file and renamed into place
  only once complete. A crash used to leave a truncated file under its final
  name, which a rerun kept. Leftover temp files are listed in a warning and
  never deleted automatically.
- Copies to exFAT now get the same once-per-run "check-then-rename" warning
  as moves.

### Added
- `--durable` fsyncs every written file and its folder, so a power cut can't
  lose a file the run reported as done. Off by default (3-15x slower copies).
- Capture dates are read from Olympus ORF and Panasonic RW2 raw files
  (`IIRO`, `IIRS`, `MMOR` and `IIU\0` headers). They used to fall back to
  the filename, `--timestamp` or `unknown/`.
