# oxisort

Sort photos and videos into date-based folders, fast. A pure-Rust redesign
inspired by [phockup](https://github.com/ivandokov/phockup): same output
layout and naming, no exiftool at runtime, and multithreaded throughout.

```
oxisort [OPTIONS] <INPUT> <OUTPUT>

oxisort ~/Downloads/card ~/Photos            # copy into ~/Photos/YYYY/MM/DD
oxisort -m --rmdirs /Volumes/SD/DCIM ~/Photos # move, then tidy empty folders
oxisort -n ~/inbox ~/Photos                  # dry run: show what would happen
```

Files are named `YYYYMMDD-HHMMSS[subsec].ext` (lowercase extension) from the
capture date and placed in `OUTPUT/YYYY/MM/DD/`. Files without a date go to
`OUTPUT/unknown/` with their original name. **Existing files are never
overwritten**: a different file with the same name gets `-2`, `-3`, …; an
identical one is reported as a duplicate.

## How a date is chosen

1. `--date-field` order (default `DateTimeOriginal CreateDate ModifyDate
   MediaCreateDate`):
   - EXIF times are the camera's wall clock; offsets are ignored. Sub-second
     digits are appended verbatim (`20170102-030405123.jpg`).
   - Videos: an Apple `com.apple.quicktime.creationdate` keeps its own local
     time. A plain `mvhd`/MKV date is UTC and is converted to the system time
     zone, or to `--tz` (`UTC`, `Z`, `±HH:MM`, `±HHMM`).
2. The filename, for media files (default pattern matches `IMG_20160915_123456`
   and `20160915_123456`; change it with `--regex`).
3. With `--timestamp`, the file's modification time.

Supported for metadata: JPEG, HEIC/HEIF, AVIF, PNG, WebP, TIFF and TIFF-based
RAW (ARW, NEF, CR2, DNG, PEF, …), CR3, RAF, MOV/MP4/M4V/3GP, MKV/WebM.
**ORF and RW2 are recognised as images but their EXIF is not read** (neither
Rust metadata crate supports their header); they fall back to the filename,
`--timestamp`, or `unknown/`. Real Sony ARW files were only tested via
TIFF-structured samples; a parse failure always falls back, never crashes.

## Options

| Option | Meaning |
|---|---|
| `-m, --move` / `-l, --link` / `-s, --symlink` | Move, hard-link (same volume) or symlink (absolute target) instead of copying |
| `-n, --dry-run` | Print `[dry-run] would …` lines; change nothing |
| `-d, --date-format FMT` | Folder layout, default `YYYY/MM/DD`. Tokens `YYYY YY MM M m DDD DD U W`; other text is literal |
| `-o, --original-names` | Keep file names (still in dated folders) |
| `--prefix PATH`, `--suffix PATH`, `--no-date-dir NAME` | Extra folders around the date folders; name of the no-date folder |
| `-f, --date-field LIST` | Date sources and their order |
| `-r, --regex RE` | Filename pattern (named groups `year month day`, optional `hour minute second`) |
| `-t, --timestamp` | Fall back to mtime |
| `--tz ZONE` | Zone for UTC-only dates (video, mtime) |
| `--file-type image\|video` | Only one kind (other files are skipped) |
| `--from DATE`, `--to DATE` | Inclusive capture-day range (`YYYY-MM-DD`); undated files pass |
| `--skip-unknown` | Skip files without a date |
| `--max-depth N` | `0` = only files directly in INPUT |
| `--delete-duplicates` | With `--move`: delete a source that is a verified duplicate |
| `--rmdirs` | With `--move`: remove source folders left empty (folders holding only `.DS_Store`, `Thumbs.db`, `desktop.ini`, `._*` count as empty; that junk is deleted and logged) |
| `-j, --jobs N` | Threads for metadata reading (default: CPU count) |
| `--io-jobs N` | Parallel file transfers and duplicate hashing, i.e. full-file reads (default 4) |
| `--progress`, `-q, --quiet`, `-v` | Progress bar / silence / more detail |
| `--log FILE` | Append a timestamped log of every action and the summary |

Exit codes: `0` ok, `1` some files failed, `2` usage error (nothing touched),
`3` fatal (output not creatable, internal error), `130` interrupted (Ctrl-C:
files in flight finish, the rest are reported; a second Ctrl-C exits at once).

### Spinning disks, SD cards and USB sticks

`--io-jobs 4` suits SSDs. **Use `--io-jobs 1` when the source or destination
is a hard disk or an SD card/USB stick**: concurrent transfers turn their
sequential reads into seeks.

## Safety

- Every destination write is exclusive: APFS clone or `O_EXCL` create for
  copies, `renamex_np(RENAME_EXCL)` (macOS) / `renameat2(RENAME_NOREPLACE)`
  (Linux) for moves. A file that appears at a planned name after planning is
  reported, never overwritten.
- **exFAT** (and other volumes without an atomic no-replace rename): moves use
  check-then-rename, with one warning per run. Copies stay fully atomic.
- A move to another volume copies, `fsync`s (F_FULLFSYNC on macOS), checks
  the size, and only then deletes the source.
- Duplicates are size-checked, then blake3-hashed. `--delete-duplicates`
  re-verifies the kept copy right before deleting and never deletes a file
  that shares its inode with the kept copy.
- Symlinks in the input are not followed. System folders (`.Trashes`,
  `.Spotlight-V100`, `@eaDir`, …) and app libraries (`*.photoslibrary`,
  `*.lrlibrary`, …) are never entered. The output folder (if inside the
  input) and the `--log` file are excluded. INPUT may equal OUTPUT (organize
  in place).
- Copies close their destination explicitly and fail (removing the partial
  file) if `close` reports an error, which is where network filesystems
  (SMB/NFS) surface delayed write errors.
- Known limits: a crash or power cut mid-copy can leave a truncated file under
  its final name, because copies write straight to the final name to keep
  no-clobber atomic (accepted risk R2). The source is kept (in move mode it
  is deleted only after a verified copy). A re-run gives the real file a `-2`
  name and leaves the truncated one in place; delete it by hand. An Apple
  video date stored with offset exactly `+00:00` is indistinguishable from a
  UTC `mvhd` date and gets converted with the zone. Hostile files can make the
  metadata parser allocate up to 1 GiB per thread.

## Coming from phockup

| phockup | oxisort |
|---|---|
| `-y, --dry-run` | `-n, --dry-run` |
| `-c, --max-concurrency` | `--io-jobs` (transfers), `--jobs` (metadata) |
| `--maxdepth` | `--max-depth` |
| `--output_prefix` / `--output_suffix` | `--prefix` / `--suffix` |
| `--movedel` (needed `--skip-unknown`) | `--delete-duplicates` (no coupling) |
| `-d, --date` | `-d, --date-format` |
| `--from-date` / `--to-date` | `--from` / `--to` (old names still accepted) |

### Differences from phockup (by design)

`scripts/parity.sh` runs both tools on the fixtures and diffs the trees.
Every difference must be one of these:

1. `DateTimeOriginal` wins over `CreateDate` when they differ.
2. Videos are named by local wall clock, not UTC.
3. MKV dates ending in `Z` are parsed instead of going to `unknown`.
4. The XMP collision suffix goes before `.xmp` (`x-2.jpg.xmp`, not `x.jpg-2.xmp`).
5. `._*`, `desktop.ini` and system folders are ignored.
6. No maker-note TimeZone shift.
7. Undated media keep their name verbatim (phockup lowercases it).
8. `.XMP` is treated as a sidecar.
9. The default regex also matches `YYYYMMDD_HHMMSS` with no prefix.
10. Dry-run detects collisions within the input.
11. `--delete-duplicates` and `--rmdirs` are never forced on.
12. ORF/RW2 EXIF isn't read (phockup reads it through exiftool).
13. `ModifyDate` is a default fallback (phockup's default fields stop at
    `CreateDate`/`DateTimeOriginal`).
14. Sub-seconds come from the `SubSecTime*` tag paired with the chosen field,
    also with `-f DateTimeOriginal`.

## Development

```
cargo test                                   # unit + integration tests
cargo clippy --all-targets -- -D warnings
scripts/gen-fixtures.sh                      # regenerate tests/fixtures (exiftool, ffmpeg, sips)
scripts/gen-bench.sh /tmp/set 1200           # synthetic benchmark library
scripts/bench.sh /tmp/set /tmp/scratch [/Volumes/SSD/dir]
scripts/parity.sh                            # phockup vs oxisort on the fixtures
```

Platforms: macOS is the verified target. The Linux code paths are written but
not yet run in CI; Windows doesn't build yet.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in oxisort by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
