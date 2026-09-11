# PROGRESS

Running status per SPEC.md section 9. Last updated 2026-09-11.

**Current state: Milestones 1, 2 and 3 complete and verified on Windows.
Milestone 4 not started - stopping here to report, per the working agreement.**

| Milestone 3 criterion | Result |
|---|---|
| Carve 20+ formats from a quick-formatted fixture | **22 recovered byte-exactly**, 38 located, from a real quick-formatted NTFS volume |
| Byte-exact recovery of contiguous files | **228 of 228**, emitting 1.0 candidates per file recovered |
| Report MB/s | **336 MiB/s** unbuffered from NVMe, median of 5; CPU-bound against a measured 1256 MiB/s disk ceiling |
| Peak RSS < 2 GB | **25 MiB** at 500,000 candidates; 10x the rows costs 1.28x the peak, and a control proves the test can fail |

403 tests across the workspace, clippy clean with `-D warnings`. The hardware
sector-read path that was the last Milestone 1 gap was cleared on 2026-09-06
against a real SD card; `docs/LIMITATIONS.md` section 1.1 has the output.

What is **not** covered, so it is not mistaken for done: fragmented files
(Milestone 4 - contiguous-only carving should recover none of
`fragmented-jpeg.img`), 16 formats the carver can locate but not size,
throughput on anything slower than NVMe, and worker scaling, which reaches only
about 1.8x at eight threads.

---

## Milestone 2 acceptance

> `rc-partition`, NTFS + FAT32 + exFAT parsers, `rc-cli list-deleted`.
> Recover deleted filenames, sizes, timestamps and original folder tree from
> fixtures; >=95% name accuracy.

| Filesystem | Name | Path | Size |
|---|---|---|---|
| NTFS  | **16/16 (100%)** | 16/16 | 16/16 |
| FAT32 | **16/16 (100%)** | 16/16 | 16/16 |
| exFAT | **16/16 (100%)** | 16/16 | 16/16 |

The graded set is deliberately adversarial rather than a flat directory of
short ASCII names, which would measure nothing:

- nine levels of directory nesting
- four non-ASCII names across CJK, Cyrillic+Greek, Arabic and Latin, one of
  them (emoji) using UTF-16 surrogate pairs and one written in decomposed
  (NFD) form while a sibling uses precomposed (NFC)
- a 250-character name: 20 LFN entries on FAT, the format maximum, and 17
  filename entries on exFAT
- a file deliberately fragmented before deletion

With 16 files, the 95% bar allows zero misses.

### What this score does and does not mean

**It measures metadata recovery only, which is the easy half.** Every number
above comes from directory entries and MFT records. None of it says whether
the file's *bytes* come back.

The fragmented file makes the point against the score itself: FAT32 and exFAT
both report 16/16 on size while reporting `first cluster only` for that file.
The parser knows exactly how large it was and has no idea where most of it
lives. A size column is not a recovery.

**N=16 with zero misses allowed is high-variance, not strict.** It sounds
demanding but it is a small sample of one particular on-disk layout. Before
100% is treated as a property of the parsers rather than of these fixtures,
the corpus needs to grow to a few hundred files. Tracked as a Milestone 3
prerequisite below.

Byte-level recovery is graded in Milestone 3 (contiguous carving) and
Milestone 4 (fragment reassembly). Those are the numbers that say whether this
tool works.

`rc-partition` additionally rebuilds the destroyed partition table in the
`nopart` fixture, relocating the NTFS volume at LBA 2048 by boot-sector
signature with 95% confidence, and reports it as `reconstructed` rather than
as though it had been read from a table.

### What the fragmented file shows

The same file, fragmented before deletion, reports differently per filesystem,
and each answer is the honest one:

| Filesystem | Reported layout | Why |
|---|---|---|
| NTFS | `3 extents` | the runlist survives deletion, so the layout is known exactly |
| exFAT | `first cluster only` | the FAT chain is cleared on delete |
| FAT32 | `first cluster only` | same |

FAT and exFAT deleted files therefore carry `DataLocation::FirstClusterOnly`,
which reports `is_exact() == false`, so nothing downstream can mistake the
contiguity assumption for a fact. Turning that guess into an answer is
Milestone 4's job.

---

## Milestone 1 acceptance, item by item

> Enumerate real drives on this OS; clone a fixture to `.raw` with bad-sector
> skipping and matching SHA-256; source hash unchanged.

| Criterion | Status | Evidence |
|---|---|---|
| Workspace skeleton | done | 4 crates, builds clean, no warnings |
| Test fixture generator | done | 8 fixtures x 512 MiB with `expected.json` |
| Read-only harness | done | Sealed trait + read-only OS handles + registry |
| **Immutability test** | **done, passing** | 7 tests over all 8 fixtures; SHA-256 identical |
| `rc-image` ddrescue clone | done | 4-pass copy/trim/scrape/retry |
| Bad-sector skipping | done | Fault injection; byte-exact either side of damage |
| Matching SHA-256 | done | Clone is byte-identical; streaming digest agrees with re-read |
| Source hash unchanged | done | Asserted in both crates' test suites |
| `rc-cli devices/image/verify` | done | Plus `smoke` |
| **Enumerate real drives** | **done, Linux and Windows** | `rc devices` lists real drives unelevated on both, with honest access and TRIM reporting. macOS remains uncompiled |

End-to-end acceptance was also demonstrated through the CLI itself, not only
through the test suite: `rc image` cloned the 512 MiB `fat32-basic` fixture and
the source SHA-256, the clone SHA-256 and the hash recorded in `expected.json`
at fixture-build time were all identical; `rc verify` passed against the
generated manifest; and both safety refusals fired (cloning a fixture onto
itself, and `rc smoke` without its opt-in).

Observed throughput was ~18 MiB/s, which is the `/mnt/d` drvfs bridge rather
than the engine -- the same clone is CPU-idle. Milestone 3 requires a real
throughput benchmark; it needs to run against native storage to mean anything.

Full gate, run under WSL2 targeting Linux:

```
cargo fmt --all -- --check      clean
cargo clippy -D warnings        clean
cargo xtask audit-deps          PASS (92 packages, no networking crate)
cargo test --workspace          60 passed, 0 failed

  rc-cli    unit                 3
  rc-device unit                14
  rc-device immutability         7    (168.7s -- 4 GiB of fixtures hashed twice)
  rc-device doc                  1
  rc-image  unit                26
  rc-image  clone integration    9    (125.3s -- includes a 512 MiB fixture clone)
  xtask     unit                 0
```

---

## What exists

### `crates/rc-device`
Read-only raw block I/O. Sealed `ReadOnlyDevice` trait, read-only OS handles on
all four backends, RAII scan-source registry, sector-aligned buffers for
unbuffered I/O, `.dd`/`.raw`/`.img` files treated transparently as devices.
Windows/Linux/macOS backends written; only the file backend and the Linux
backend are compiled and exercised here.

### `crates/rc-image`
ddrescue-compatible `.map` files, streaming SHA-256 + MD5 with a `.hash`
manifest, four-pass rescue strategy, sparse output, resume, and `OutputSink` —
the only write path in the project, which refuses destinations resolving to a
device being scanned.

### `crates/rc-cli`
`rc devices | image | verify | smoke`, all with `--json`.

### `crates/xtask`
`audit-deps` (the no-network guarantee, ~40 forbidden crates) and `ci`.

### `testdata/`
`make_corpus.py` builds a 17-file corpus from the Python standard library alone,
reproducible on a given machine (SQLite bytes vary with the Python's SQLite
build, so `expected.json` records what was actually written) — including a hand-written baseline JPEG encoder with
restart markers, and an MP4 whose `stco` chunk offsets point at real positions
inside `mdat`. Both were built deliberately so Milestones 3 and 4 have known
internal structure to test against. The JPEGs and PNGs were verified by an
independent decoder (.NET `System.Drawing`), and the ZIP/SQLite/PDF/MP4 files by
structural walks.

`build_fixtures.sh` produces: `ntfs-basic`, `fat32-basic`, `exfat-basic`,
`ext4-basic`, `fragmented-jpeg`, `quickformat`, `overwritten`, `nopart`.

---

## Blocking questions

### 1. C: is full, which blocks native Windows builds

C: has ~250 MB free. Installing the Visual Studio C++ build tools needs several
GB staged on C: even when the install target is D:, so **that install will
likely fail until space is freed**.

Until then there is no `link.exe`, so nothing can be built or tested natively on
Windows — not even `cargo check`, because build scripts and proc macros must
link in order to run. All verification so far ran inside WSL2 targeting Linux.

**What I need from you:** free space on C: (a few GB), then install the build
tools. After that, `rc smoke` can clear the biggest caveat in
`docs/LIMITATIONS.md`.

I also consumed roughly 300 MB of C: installing a C compiler into WSL so that
anything could be compiled at all. If that is a problem, say so and I will
remove it.

### 2. The Windows raw-device path is unverified

`backend/windows.rs` has never been executed or even compile-checked. Once the
toolchain works, please run the smoke test against a **throwaway** USB stick or
SD card:

```bash
RC_SMOKE_ALLOW=1 rc smoke --device \\.\PhysicalDrive2
```

It refuses to run without both the env var and an explicit `--device`, refuses
the system drive, reads only a handful of sectors, and asserts the hash of the
range it read is unchanged.

### 3. Fixture size

Fixtures are 512 MiB each, 4 GiB total, which makes the immutability test take
about four minutes over drvfs. `RC_FIXTURE_SIZE=128M` would cut that to about a
minute. Worth doing, or keep the larger fixtures for more realistic
fragmentation?

---

## Gate: the Windows sector-read path before Milestone 8 - DISCHARGED

**Raised 2026-09-05, cleared 2026-09-06.**

`rc smoke` has been run against a physical USB device and passed. Full result
in `docs/LIMITATIONS.md` section 1.1. The whole Windows raw-device path is now
verified: enumeration, destination resolution, elevation detection, the
smoke-test guards, `FILE_FLAG_NO_BUFFERING` reads, the `OVERLAPPED` offset
plumbing at 15.7 GB into a device, and the bounce buffer.

The bounce-buffer check is the one worth singling out. An earlier version of
the test only varied the read *offset*, which never triggers the bounce path -
that is triggered by an unaligned destination *address*. The test now reads the
same LBA into an aligned buffer and a deliberately skewed one and compares, and
reports whether the skewed buffer really landed unaligned so a lucky allocation
cannot produce a vacuous pass. It reported `bounce path exercised: yes`.

**What this cost to leave open:** compiling the backend for the first time
found three real bugs in reviewed code, and running it found two more in the
reporting layer. Every one of those would have been discovered through a GUI at
Milestone 8 instead. The lesson is recorded here deliberately: "fine, just
unverified" is not a state to leave code in when verifying it is cheap.

---

## Decisions made this session

- **Rust toolchain on D:** `RUSTUP_HOME`/`CARGO_HOME` at `D:\Rust\`, because C:
  cannot hold them.
- **Fixture generation via WSL2 with narrow passwordless sudo.** A
  `sudoers.d` drop-in grants exactly `losetup`, `mount`, `umount`, `mkfs.*`,
  `chown`, `blockdev`, `sfdisk`, `sync`. Not a blanket NOPASSWD.
- **Corpus built from the Python standard library only.** No ffmpeg or Pillow,
  because installing them risked filling C:, and hand-building the formats gives
  exact control over internal structure for later milestones.
- **exFAT mounts through FUSE** — the WSL2 kernel has no exfat driver.
- **`nopart` uses a loop offset, not partition scanning** — WSL2 runs the loop
  driver with `max_part=0`.
- **Dependencies are optimised in dev builds** (`[profile.dev.package."*"]`).
  Unoptimised SHA-256 made the test suite take ~30x longer.
- **A fault-injecting device lives behind a `test-util` feature inside
  `rc-device`**, because the sealed trait deliberately prevents test doubles
  from being written in consuming crates.
- **The fixtures directory is locked, not just documented.** Tests take a
  shared advisory lock on `testdata/fixtures/.lock`; `build_fixtures.sh` takes
  the exclusive side and blocks. This was added after a concurrent rebuild made
  the immutability test report that scanning had modified a fixture. The
  diagnosis was correct, but a safety test that can fail for reasons unrelated
  to a write is worse than no test: it teaches everyone to dismiss the one
  check guarding the invariant that matters most. Verified by running the test
  against a live rebuild, where it blocked for 19 minutes and then passed
  instead of failing.
- **Added a crate not in SPEC.md section 3: `crates/xtask`.** Section 4.4
  requires a CI check that greps the dependency tree for networking crates.
  A Rust xtask parses `cargo metadata` and works identically on all three
  platforms, where a shell script would not. It is `publish = false` and is
  never a dependency of the engine.

---

## Milestone 3 progress

Prerequisites (all of which gate the carving grade being meaningful):

| Prerequisite | Status |
|---|---|
| Corpus grown to a few hundred files | done: 315 files, 111 deleted, 204 retained as precision distractors |
| Every signature carries a max_size | done and load-time enforced; 37 of 44 formats are footerless and depend on it |
| OOXML sniffed rather than emitted as .zip | done: 42 of 42 corpus docx files refine from the zip container |
| Fixtures rebuilt on the adversarial corpus | done, all eight |
| An NTFS fixture from a driver other than ntfs-3g | done: `ntfs-windows.img`, built by the Microsoft NTFS driver |
| quickformat verified to retain content | done, checked empirically at build time |
| Validators doing real structural decoding | done: 23 distinct validators covering 28 of 44 signatures; 19 graded by a foreign encoder, 4 unavailable here with reasons |
| Precision measured alongside recall | done for both: validators on two corpora, scanner on the quickformat fixture |
| Multi-threaded scanner with a SIMD prefilter | done; prefilter threshold set by measurement |
| Byte-exact recovery of contiguous files | done: 228 of 228 on the quick-formatted fixture |
| Carve 20+ formats from a quick-formatted fixture | done: 38 located, 22 recovered byte-exactly, from a real quick-formatted NTFS volume |
| Disk-backed candidate index (`rc-index`) | done: 10x the rows costs 1.28x the peak, against a 2.0x bar |
| Peak RSS under 2 GB | done: 25 MiB at 500,000 candidates |
| Report MB/s | done: 336 MiB/s unbuffered from NVMe (median of 5), CPU-bound, with the disk's 1256 MiB/s ceiling measured alongside |
| Throughput measurable | done: unbuffered image reads bypass the page cache, and a test proves the OS enforces it |
| RSS ceiling testable | done: the memory test grows candidates tenfold, with a deliberately resident control |

Built so far: the signature database (44 formats spanning image, video, audio,
document, archive and database) with its loader, and the validators. The
scanner and `rc-index` are next.

### Validators: what they check and what the corpus run showed

Ten validators - `jpeg`, `png`, `bmp`, `pdf`, `zip`, `mp4`, `sqlite`,
`riff_wav`, `riff_avi`, `riff_webp` - each walking the format's own internal
accounting rather than matching a header. Two tests in `validate/mod.rs` tie
them to the signature database in both directions, so a typo in
`signatures.json` cannot silently disable one and leave every candidate of
that format unvalidated.

Measured against the 315-file corpus (`crates/rc-carve/tests/corpus.rs`):

| Measure | Result |
|---|---|
| Corpus files accepted | 228 of 228 that have a validator, all `Valid` |
| Reported length exactly equal to file size | 228 of 228 |
| Length still exact with 4 KiB of junk appended | 228 of 228 |
| docx refined from the zip container | 42 of 42 |
| Cross-format trials (validator vs. a format that is not its own) | 2922 |
| False positives among them | 0 |

The 87 `text` and `binary` corpus files are the negative control: they are not
any carvable format, and all ten validators reject every one of them.

### Every validator is now graded by a foreign encoder

The results above were measured against a corpus this project wrote. Eight of
the ten validators carried correlated-error risk from that: `make_corpus.py`
builds jpeg, png, pdf and mp4 with hand-written encoders, and bmp, wav, avi and
webp had no corpus file at all. A misreading of a spec shared between encoder
and validator produces a clean 100% that means nothing.

`testdata/corpus/make_independent.py` closes it. ImageMagick 7 and ffmpeg 9
write 29 samples across all eight formats; neither knows this project exists.
Four formats get two different encoders, so neither tool is a single point of
agreement.

| Validator | Graded by |
|---|---|
| `ico` | ImageMagick |
| `pe` | the system linker (a real signed-format binary) |
| `jpeg` | ImageMagick, ffmpeg |
| `png` | ImageMagick, ffmpeg |
| `bmp` | ImageMagick, ffmpeg |
| `riff_webp` | ImageMagick, ffmpeg |
| `pdf` | ImageMagick |
| `mp4` | ffmpeg |
| `riff_wav` | ffmpeg |
| `riff_avi` | ffmpeg |
| `zip` | Python `zipfile` |
| `sqlite` | Python `sqlite3` |

All 41 accepted with exact lengths, 902 cross-format trials, zero false
positives - and the corpus earned its keep twice more: a real signed Windows
binary showed the PE validator missing 2661 bytes of overlay, and real
ImageMagick TIFFs showed it stopping 70 bytes short, because a directory value
larger than four bytes is stored past the image data and my own hand-written
sample had none. The variants move the structures the validators actually
walk rather than just adding files: progressive vs baseline JPEG, interlaced
and 16-bit PNG, three BMP bit depths, lossy vs lossless WebP, and - confirmed
by reading the box order back out - an MP4 written `ftyp, moov, free, mdat`
against three written `ftyp, free, mdat, moov`.

A test asserts this coverage rather than describing it, so the claim cannot
quietly stop being true. What remains untested is real files from real cameras
and real word processors, and that closes only against actual recovered media.

These samples are validated as bytes and are **not** in the disk images.
Validator grading does not need a filesystem; the Milestone 3 carving
acceptance test does, so folding them into `make_corpus.py` is a prerequisite
for that and will require rebuilding the fixtures.

### The fixture generator now retries until fragmentation is real

Growing the corpus to 315 files made NTFS stop fragmenting the target: with
more files on the volume, freeing "every other filler file by name" coalesced
into contiguous runs instead of scattered holes, because ntfs-3g does not
allocate in creation order. The build warned rather than silently shipping a
fixture that claimed a fragmentation it did not have.

`fragment_one` now frees a seeded-random subset instead of every Nth file, and
verifies the result against the image, retrying with progressively finer hole
fields. NTFS needed the third attempt (8 KiB holes, three files freed in every
four); the other three filesystems succeed on the first. That difference is
itself worth knowing: ntfs-3g is markedly better at finding contiguous free
space than vfat, exfat-fuse or ext4.

### The corpus is a single sample, however large it gets

111/111 is a stronger result than 16/16, but it does not address correlated
error. Every one of those 315 files was written by one script through one
`mkfs`/`ntfs-3g` path. If there is a systematic blind spot - an
`$ATTRIBUTE_LIST` layout, an index-allocation pattern or a resident-attribute
threshold that Microsoft's NTFS driver produces and `ntfs-3g` never does - this
corpus cannot surface it at any size, because the sample is not independent.

Native Windows builds now work, so an independent sample is available for the
first time: create a VHD, format and populate it with **Microsoft's own NTFS
driver**, delete a subset, and parse the result. Any disagreement between that
fixture and the `ntfs-3g`-built one is a genuine finding about the parser.

Both steps need Administrator (creating and attaching a VHD is privileged), so
this is queued alongside the smoke test rather than done automatically.

### Validator requirements worth writing down before implementing

Two formats where a header match is actively misleading:

- **SQLite.** `SQLite format 3` followed by a NUL byte is sixteen bytes of literal ASCII, so it
  matches any text file that happens to contain the string - documentation
  about SQLite, a hex dump, a source file. The validator must read the header
  fields back: page size a power of two in 512..65536, page count consistent
  with the candidate's length, and a freelist page count that does not exceed
  the page count.
- **MP4.** A `ftyp` box with an implausible size field is a common false
  positive out of compressed data, where four arbitrary bytes precede the
  literal `ftyp`. The box size must be bounded before any atom walking begins,
  or the walk chases a garbage length into the rest of the volume.

### Throughput and RSS, given what this machine can actually measure

Native Windows builds now work, which changes both answers.

**Throughput** should be measured natively on Windows, not under WSL. Every
read from `/mnt/d` crosses the drvfs bridge at roughly 20 MiB/s regardless of
file size, so no fixture built there can produce a device throughput number -
the bottleneck is the bridge, not the page cache and not the scanner.

But native is not automatically honest either. Reading a fixture file through
NTFS still has the page cache in front of it, so a figure taken that way
measures the cache, not a device. Three ways to report a number, in descending
order of what it is worth:

1. **A real device.** Scan a physical drive once the smoke test has proved the
   raw read path works. This is the only true device number and it is what the
   benchmark should ultimately quote.
2. **A file read with `FILE_FLAG_NO_BUFFERING`,** which bypasses the cache. The
   imaging path already opens devices this way; the benchmark can open its
   fixture the same way. Closer to a device number, still mediated by the
   filesystem.
3. **A cached file read,** which is what a naive `cargo bench` against a
   512 MiB fixture produces. Useful only for detecting regressions in the
   scanner's own CPU cost, and it must be **labelled cache-bound** wherever it
   is reported.

Anything reported must say which of the three it is. A cache-bound figure
recorded as a device throughput number would be worse than reporting nothing.

**Peak RSS** is graded two ways, because the literal criterion is not testable
here. This host has 7.8 GB of RAM and WSL is capped at 2.9 GB, so a "< 2 GB"
assertion could only fail by OOM, and passing it against a 512 MiB fixture
would prove nothing about whether the index is disk-backed. So the benchmark
reports peak RSS (satisfying the letter of SPEC.md section 5.8) *and*
asserts the property that actually matters: that memory use grows sublinearly
in the candidate count.

The pass condition is stated as a ratio, not as "flat", because a disk-backed
index is not flat: SQLite keeps a page cache, a write-ahead log and its own
buffers, all of which grow somewhat. Asserting flatness would produce failures
that are technically correct and operationally meaningless.

> **Pass condition.** Growing the candidate count by 10x must grow peak RSS by
> no more than 2x, measured after the index has been flushed. An in-RAM index
> grows about 10x and fails clearly; a disk-backed one grows by well under 2x
> and passes with margin. The ratio is deliberately loose enough that ordinary
> cache and WAL growth cannot trip it, and tight enough that storing candidates
> in memory cannot pass.

---

## Milestone 3: notes kept for the record

`rc-carve` signature engine plus validators, `rc-index`, and a throughput
benchmark. Acceptance: carve 20+ formats from the quick-formatted fixture,
byte-exact recovery of contiguous files, report MB/s, peak RSS under 2 GB.

Four things to settle first, all of which decide whether the carving grade
means anything:

0. **Grade precision, not just recall.** Signature carving produces enormous
   false-positive volume: `FF D8 FF` occurs by chance, inside other files, and
   legitimately inside every JPEG carrying an EXIF thumbnail. A carver that
   emits 80,000 candidates and happens to include all 16 targets scores 100%
   recall and is useless. The acceptance test must report both numbers plus
   the rank of the graded files among candidates. The 14 files that were *not*
   deleted are free precision distractors and must not appear in deleted-only
   mode.

0b. **Grow the corpus to a few hundred files** before treating any accuracy
   figure as a property of the code.

0c. **Every footerless format needs a per-format maximum size**, or one
   spurious header swallows gigabytes. And OOXML files carve as bare ZIPs, so
   sniff `[Content_Types].xml` to emit `.docx`/`.xlsx`/`.pptx` rather than
   `.zip`. *Both done - the ceilings are load-time enforced, and the zip
   validator refines docx/xlsx/pptx/jar/apk/epub from the member names.*

Note that items 0 and 0c are settled for the validators but **not for the
scanner**, which does not exist yet.

### Scanner results on the quick-formatted fixture

| Measure | Value |
|---|---|
| Scanned | 512 MiB, 8 threads, byte-table prefilter |
| Header matches (the scanner's raw output) | 4203 = **8406 per GB** |
| Rejected by a validator | 3891 |
| Suppressed as contained | 84 |
| **Candidates emitted** | **228** |
| **Byte-exact recovery** | **228 of 228** carvable deleted files |
| Precision at full recall | **1.0 candidates examined per file recovered** |
| Validators' contribution | removed 94.6% of raw header matches |

By extension the output is exactly the corpus: docx 42, jpg 47, mp4 27, pdf 47,
png 49, sqlite 16. Nothing else is emitted at all.

**What this does not license.** The fixture is 98.6% zeros, which is why
`00 00 01 00` matched 3782 times; a real drive carries far more entropy and
will generate a different, probably larger, false-positive population. Every
file here is contiguous, so this says nothing about fragmentation - that is
Milestone 4. And 8406 header matches per GB is a property of this fixture, not
a constant. The number to watch on real hardware is the ratio, not the totals.

Getting here required three bugs to be found by the measurement rather than by
review, all in the same family: a validator reporting the size of the buffer it
was handed as if it were the length of the file. See the commit log for
`502d305`. The general test - feed each validator a truncated file with three
different amounts of padding and fail if the reported length moves - is what
turned one found instance into four fixed ones.

Two validators were written *because* of this measurement rather than from the
format list: `ico` (3782 of 4060 candidates before it existed) and `pe` (50, in
a fixture containing no executables).

### Throughput, measured

Milestone 3 says "report MB/s". Every figure before this came from a 512 MiB
fixture that fits in RAM, so every one measured memory bandwidth. Two changes
made the number mean something:

* **`rc_device::open_unbuffered`** opens an image with `FILE_FLAG_NO_BUFFERING`
  (Windows) or `O_DIRECT` (Linux), and `rc carve --unbuffered` uses it. Its
  read path is checked against the buffered one across 606 combinations of
  offset, length and buffer alignment - and separately checked to be *really*
  uncached, by confirming the OS rejects a misaligned read on the handle, since
  every equivalence test would pass identically if the flag were ignored.
* **A four-tier benchmark**, `tests/throughput.rs`, median of five runs:

| Tier | MiB/s | Range |
|---|---|---|
| 0. raw unbuffered read, no scanning | **1256** | 1237-1262 |
| 1. engine: prefilter + header match, one thread, no I/O | 189 | |
| 2. full scan, from the page cache | 349 | 333-377 |
| 3. **full scan, unbuffered** | **336** | 315-358 |

**The scan is CPU-bound on this NVMe drive**: it consumes 27% of what the disk
delivers. On a USB stick or SD card at tens of MB/s it will be I/O-bound
instead. The benchmark decides which by comparing the scan against the raw-read
ceiling, not by assertion.

**It asserts correctness, not speed.** Throughput is machine-dependent and a
threshold that passes here fails on a slower runner and teaches everyone to
ignore it. What it asserts is that the unbuffered scan finds exactly what the
buffered scan finds.

Three things the benchmark found that nothing else would have:

1. **The engine ran at 16 MiB/s**, because `ico` and `mpeg_ps` both begin with
   a zero and the fixture is 98.6% zeros - 532 million full header comparisons
   in a 537 MB scan. Signatures now anchor on their rarest byte; prefilter hits
   fell to 543,206, the engine rose to ~200 MiB/s, and recovery was unchanged
   at 228 of 228. This matters beyond the benchmark: the free space of a
   formatted drive is zeros and erased flash is `0xFF` - the start of every
   JPEG - so the media this tool suits best were exactly where the prefilter
   degraded to checking every byte.
2. **A single run is not a benchmark.** Two consecutive runs reported the
   cached scan at 509 and then 332 MiB/s, and a verdict computed from one of
   them flipped on noise alone. Hence medians with ranges.
3. **The workers scale poorly** - eight threads reach about 1.8 times the
   one-thread rate. On fast storage that is now the limit. Not required by
   Milestone 3; recorded in LIMITATIONS.

Along the way, `scan.rs` stated the scan was "I/O-bound on any real medium" and
the benchmark's own first version blamed a queue depth of one without
measuring it. Both were wrong and both are corrected in place.

### Format coverage: 38 located, 22 recovered

Milestone 3 asks for 20+ formats. `testdata/corpus/formats.py` builds one
ordinary file of every format the signature database claims to know - 38
distinct formats - and `tests/formats.rs` lays them at cluster-aligned offsets
and carves them back.

Measured against a real NTFS volume that was formatted, populated and then
quick-reformatted - not a synthetic image.

| Measure | Value |
|---|---|
| **Located** (candidate at the file's true offset, right signature) | **38 of 38** |
| **Recovered byte-exactly** (carved bytes hash to the original) | **22 of 38** |
| Header matches | 572 = 1144 per GB |
| Candidates emitted | 43 |

Those are two different claims and both are reported, because locating a file
and recovering it are not the same thing: establishing a file's *end* needs a
validator, and a signature without one leaves the carver knowing where a file
starts and nothing more. Reporting only the first number would be the "eighty
thousand candidates with perfect recall" failure in a different costume.

Recovery rose from 10 to 22 by writing validators for the formats that state
their own length in a header - 7z, CAB, GIF, RTF, TIFF, EVTX, registry hives,
ELF, PSD and AIFF. The 16 still unsized need decoding rather than a header
read: gzip and bzip2 and xz would have to be decompressed, MP3 and FLAC and Ogg
walked frame by frame, and OLE2, MKV, FLV, ASF and MPEG-PS walked through
nested structures. `eml` has no length at all - a mail message ends where the
next thing begins.

Where the bytes come from is split deliberately. gzip, bzip2 and xz come from
zlib, libbzip2 and liblzma; WAV from Python's `wave`; ZIP from `zipfile`; the
SQLite WAL from SQLite. Those are foreign implementations and their output is
evidence. The rest are hand-written from the specs, which proves the signature
matches something shaped like the format and nothing more - validator
correctness is established separately by the ImageMagick and ffmpeg corpus.

Every sample is at least 8 KiB, because NTFS stores a smaller file's data
*inside* its MFT record rather than in a cluster. A quick format discards the
MFT, so a resident file is unrecoverable by carving however good the carver is,
and a corpus of 200-byte samples would have measured nothing. The generator's
self-check enforces that floor and caught three samples that were under it.

Bugs this turned up, in the order they appeared:

* `vhd` read "connecti" for a cookie that is "conectix" and could never have
  fired. Sweeping all 44 headers the same way then found `sqlite_wal` matching
  only one of the two legal WAL magics.
* Every `.m4a` carved as `.mp4`. Both signatures match the same bytes - `ftyp`
  at offset 4 and `ftypM4A ` at offset 4 - so one suppressed the other as
  contained, by database order. The longer header is the more specific claim
  and now wins, the same rule that keeps a `.docx` from coming back as `.zip`.
* The WAL sample was not reproducible: SQLite randomises Salt-1 and Salt-2 in
  every header and checksums over them, so 207 bytes moved between runs. The
  corpus self-check now builds every sample twice and compares.
* Writing the validators found three of my own samples wrong - an EVTX
  declaring a 1-byte header block, an ELF segment claiming more bytes than its
  file held, and a 7z whose `NextHeaderOffset` was written absolute when the
  field is relative to the end of the signature header.
* `validate/mod.rs` contained three raw NUL bytes, written by a heredoc that
  collapsed `\x00` into an actual zero byte. Rust accepts that inside a byte
  string, so it compiled and 130 tests passed while the file was binary to
  every text tool. Only that one file was affected; the whole tree was checked.

### rc-index: the memory criterion, measured

SPEC.md section 5.8 requires peak RSS under ~2 GB regardless of drive size.
The pass condition is sublinear with a named ratio: **ten times the candidates,
no more than twice the peak.**

| Candidates | Peak RSS growth |
|---|---|
| 50,000 | +14.3 MiB |
| 500,000 | +18.3 MiB (25.0 MiB absolute) |
| **Ratio for 10x the rows** | **1.28x** (bar: 2.0x) |

Absolute peak is 25 MiB against a 2 GB ceiling.

**The test has a control.** A memory test that passes is worthless unless it
could have failed, so the identical measurement runs against a deliberately
resident index - a plain `Vec<Candidate>` - and asserts that one *does* breach
the ratio. It comes out at 21.15x. If the control ever stops failing, the
measurement has lost its power and the real result means nothing.

The first run failed at 3.94x, and the cause was not the index holding rows: it
was the SQLite page cache I had set to 32 MiB, which had not finished filling
at 50,000 candidates and had by 500,000. A sweep across cache sizes confirmed
it - 32,000 KiB grew 8.3 then 26.2 MiB, while 8,000 KiB and below added nothing
- so `PAGE_CACHE_KIB` is 8 MiB with the measurement written next to it. A carve
is write-heavy; a large read cache buys little during the scan itself.

The no-network audit still passes with SQLite in the tree: 117 packages, 40
forbidden names, clean.

### The scanner's denominator

**Validator figures must not be quoted anywhere near the scanner's.** Everything measured so far - 228/228 accepted, 0
false positives in 2922 cross-format trials, 29/29 on the independent corpus -
was measured with known file boundaries handed to the validator. The scanner
faces a completely different distribution:

* every three-byte coincidence in gigabytes of unstructured sectors,
* compressed streams whose bytes happen to read as `ftyp` boxes or `BM`
  headers,
* EXIF thumbnails, which are whole valid JPEGs inside other JPEGs,
* JPEGs and PNGs inside docx files inside the same volume,
* and slack space holding fragments of files deleted long before these.

Validator precision does not predict scanner precision. The scanner acceptance
test therefore reports:

1. **Candidates generated per GB scanned**, by format.
2. **Where the 111 known-deleted files rank** among those candidates.
3. **Precision at the recall point** - how many candidates must be examined to
   reach all 111.
4. The count surviving validation, as a separate line from the count generated,
   so the validators' contribution is visible rather than assumed.

**Carried past Milestone 3** (none required by its acceptance criteria):

* ~~A throughput number that describes something.~~ Done - see "Throughput,
  measured" below.
* **Sizing the remaining 16 formats.** Each needs decoding rather than a header
  read; see the format-coverage section. None is required by Milestone 3, and
  the number is reported rather than hidden.
* **`rc-cli carve`**, wiring the scanner to the index. SPEC.md is explicit
  that the CLI is the source of truth and everything must be reachable there.

Then:

1. **The throughput benchmark is not currently measurable.** Two separate
   problems. Over `/mnt/d` the engine reports ~20 MiB/s while the CPU idles,
   which describes the WSL drvfs bridge. And a 512 MiB image fits in RAM, so
   any figure from it measures the page cache, not a device. A real number
   needs a fixture several times larger than RAM with caches dropped between
   runs, or it must be recorded plainly as cache-bound with the real
   measurement deferred to the hardware smoke test. It must not be written
   down as a device throughput number.
2. **The 2 GB RSS ceiling needs a fixture that can breach it.** Half a
   gigabyte passes trivially whether the candidate index is disk-backed or
   entirely in RAM, so the current fixtures prove nothing about the thing the
   requirement exists to prove. This needs a sparse multi-GB image seeded with
   enough candidates to create real index pressure.

3. **Confirm `quickformat` is genuinely a quick format.** `mkfs.ntfs -F -Q` is
   what the generator uses, and the build now verifies empirically that a known
   corpus file's bytes survive the reformat rather than trusting the flag. A
   fixture whose volume had been zeroed would pass every carving test by
   testing nothing.

4. **Four fixtures carried the pre-Milestone-2 corpus.** `quickformat`,
   `overwritten`, `fragmented-jpeg` and `nopart` were built before the
   adversarial names were added. Each one's `expected.json` records what was
   actually written, so they are self-consistent and the current tests are
   valid, but they were rebuilt before starting Milestone 3 so carving is
   graded against the adversarial corpus.
