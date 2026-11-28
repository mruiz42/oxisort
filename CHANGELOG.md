# Changelog

All notable changes to oxisort are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## Unreleased

### Changed
- Linux release binaries are now static musl builds for x86_64 and aarch64,
  replacing the glibc builds. They run on any Linux, including older
  distributions and Alpine.

### Added
- Capture dates are read from Olympus ORF and Panasonic RW2 raw files
  (`IIRO`, `IIRS`, `MMOR` and `IIU\0` headers). They used to fall back to
  the filename, `--timestamp` or `unknown/`.
