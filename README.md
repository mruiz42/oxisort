# oxisort

An extremely fast photo and video organizer, written in Rust.

oxisort sorts photos, RAW files and videos into date-based folders
(`YYYY/MM/DD`) and renames them by capture time. It needs no exiftool or
other runtime dependencies, and it runs multithreaded throughout.

Inspired by [phockup](https://github.com/ivandokov/phockup).

> [!WARNING]
> **oxisort is beta software.** It moves, renames, links and can delete files.
> **Always keep a backup of your photos before running it**, and try a dry run
> (`-n`) first. It is provided as is, with no warranty, and the authors accept
> no responsibility for lost, damaged or misplaced files. See
> [Disclaimer](#disclaimer).

```
oxisort [OPTIONS] <INPUT> <OUTPUT>

oxisort ~/Downloads/card ~/Photos            # copy into ~/Photos/YYYY/MM/DD
oxisort -m --rmdirs /Volumes/SD/DCIM ~/Photos # move, then tidy empty folders
oxisort -n ~/inbox ~/Photos                  # dry run: show what would happen
```

Files are named `YYYYMMDD-HHMMSS[subsec].ext` (lowercase extension) from the
capture date and placed in `OUTPUT/YYYY/MM/DD/`. Files without a date go to
`OUTPUT/unknown/` with their original name. **Existing files are never
overwritten**: a different file with the same name gets `-2`, `-3`, …. A file
whose content already exists (in the input or anywhere under OUTPUT) is a
duplicate and goes to `OUTPUT/duplicates/` (see [Duplicates](#duplicates)).

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
| `--duplicates-dir NAME` | Folder under OUTPUT that receives duplicates (default `duplicates`) |
| `--skip-duplicates` | Leave duplicates where they are; only report them |
| `--delete-duplicates` | With `--move`: delete a source that is a verified duplicate instead of filing it |
| `--rmdirs` | With `--move`: remove source folders left empty (folders holding only `.DS_Store`, `Thumbs.db`, `desktop.ini`, `._*` count as empty; that junk is deleted and logged) |
| `-j, --jobs N` | Threads for metadata reading (default: CPU count) |
| `--io-jobs N` | Parallel file transfers and duplicate hashing, i.e. full-file reads (default 4) |
| `--progress`, `-q, --quiet`, `-v` | Progress bar / silence / more detail |
| `--log FILE` | Append a timestamped log of every action and the summary |

Exit codes: `0` ok, `1` some files failed (or the destination filled up: the
summary then starts with `stopped: destination full`), `2` usage error
(nothing touched), `3` fatal (output not creatable, internal error), `130`
interrupted (Ctrl-C: files in flight finish, the rest are reported; a second
Ctrl-C exits at once).

### Spinning disks, SD cards and USB sticks

`--io-jobs 4` suits SSDs. **Use `--io-jobs 1` when the source or destination
is a hard disk or an SD card/USB stick**: concurrent transfers turn their
sequential reads into seeks.

## Duplicates

A file is a duplicate when another file has the **same inode, or the same
size and blake3 hash**. "Another file" is any other input or any existing
file anywhere under OUTPUT. Names and dates play no part: a re-imported photo
is found even if it was renamed or got a different date. Empty files are
never duplicates, and a file that can't be read is never one.

Which copy is kept, per set of identical files:
1. an existing file under OUTPUT (the first by folder, then `x.jpg` before
   `x-2.jpg`), so organized files are never displaced;
2. else a file already in place (organizing in place, or a re-run);
3. else the first input in sorted order, which is copied/moved as usual.

Every other copy is handled by one of three mutually exclusive policies:

| Policy | What happens |
|---|---|
| default | Filed under `OUTPUT/duplicates/<path relative to INPUT>`, original name kept, sidecars with it, in the run's own mode: copied (APFS clone where possible), moved with `-m`, hard-linked with `-l`, symlinked with `-s`. Name clashes there get `-2`, `-3`, …. |
| `--skip-duplicates` | Reported only (`dup SRC == KEPT`); source and sidecars stay. |
| `-m --delete-duplicates` | The source is deleted after re-checking that it and the kept copy still have their planned size and modification time (contents are not re-read); sidecars stay. |

- Re-runs don't pile up: in copy, link and symlink runs, a duplicate whose
  content is already in its place in the duplicates folder is reported
  `(already in …)` and nothing is written. The first copy run after
  re-importing a card that still holds old photos copies each old photo
  there once.
- An input that shares its inode with any file under OUTPUT (a second
  `--link` or `--symlink` run) is `in-place`: nothing is written or deleted.
- A duplicate is only filed or deleted after the copy it duplicates was
  written successfully.
- The duplicates folder is reserved: it is never scanned as input, never
  used as the kept copy, and never a target of the main tree. `--prefix`,
  `--no-date-dir` or a date format starting with its name are refused, as is
  an INPUT inside it, or a `duplicates` entry that is a file or a symlink.
  Use `--duplicates-dir` if you already have a folder of that name.
- Organizing in place (`oxisort -m DIR DIR`) also dedupes the library
  itself: identical organized files beyond the first move to
  `DIR/duplicates/`. Try it with `-n` first, or use `--skip-duplicates`.
- Cost: every run lists OUTPUT once (one `lstat` per file, in parallel);
  files whose size collides with an input are hashed, large ones first by
  their first and last 64 KiB, so a chance size match costs 128 KiB of reads.
  `-v` logs `dedupe: N size groups, M partial, K full hashes, X MiB read`.

## Safety

- Every destination write is exclusive: APFS clone or `O_EXCL` create for
  copies, `renamex_np(RENAME_EXCL)` (macOS) / `renameat2(RENAME_NOREPLACE)`
  (Linux) for moves. A file that appears at a planned name after planning is
  reported, never overwritten.
- **exFAT** (and other volumes without an atomic no-replace rename): moves use
  check-then-rename, with one warning per run. Copies stay fully atomic.
- A move to another volume copies, `fsync`s (F_FULLFSYNC on macOS), checks
  the size, and only then deletes the source.
- Duplicates are size-checked, then blake3-hashed (equality is decided by the
  full hash). Filing a duplicate never removes it: it is moved or copied with
  the same no-clobber primitives as any other file. Right before deleting,
  `--delete-duplicates` re-checks that the kept copy is still a regular file
  with the planned size and modification time (only the size when this run
  wrote it), and that the source still has its planned size, modification
  time and inode. Contents are not hashed again. It never deletes a file
  that shares its inode with the kept copy (`(same file, kept)`).
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

## Development

```
cargo test                                   # unit + integration tests
cargo clippy --all-targets -- -D warnings
scripts/gen-fixtures.sh                      # regenerate tests/fixtures (exiftool, ffmpeg, sips)
scripts/gen-bench.sh /tmp/set 1200           # synthetic benchmark library
```

Platforms: macOS is the verified target. The Linux code paths are written but
not yet run in CI; Windows doesn't build yet.

## Disclaimer

oxisort is **beta software** and is still under active development. Its
behavior, options and output layout may change between versions.

- **Keep a backup.** Before running oxisort on files you care about, make sure
  you have a complete, verified backup of them somewhere else. Run a dry run
  (`-n`) first and read what it plans to do.
- **No warranty.** oxisort is provided "as is", without warranty of any kind,
  express or implied, including fitness for a particular purpose. See the
  [MIT](LICENSE-MIT) and [Apache-2.0](LICENSE-APACHE) licenses for the full
  terms.
- **No liability.** You use oxisort entirely at your own risk. The authors and
  contributors accept no responsibility for any loss, damage, corruption,
  misplacement or deletion of files, or for any other consequence of using it.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in oxisort by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
