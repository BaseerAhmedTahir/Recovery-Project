# PROGRESS

Running status per SPEC.md section 9. Last updated 2026-09-06.

**Current state: Milestones 1 and 2 complete. Milestone 3 in progress.**

The toolchain blocker is resolved: C: was freed, VS Build Tools 2022 is
installed, and the workspace now builds and passes all 196 tests **natively on
Windows** as well as under WSL. Compiling the Windows backend for the first
time found three real bugs (a wrong `ReadFile` buffer type, a borrow error in
the bounce-buffer path, and an unexported IOCTL constant).

Most of the Windows backend is now verified against real hardware. The one
remaining gap is the sector read itself, which needs Administrator rights and a
throwaway device; `docs/LIMITATIONS.md` section 1.1 has the exact command.

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

## Gate: the Windows sector-read path before Milestone 8

**Decided 2026-09-05, mostly discharged 2026-09-06.**

The toolchain now builds natively, and enumeration, destination resolution,
elevation detection and the smoke-test guards have all been executed against
real hardware. Compiling the previously-unbuildable backend immediately found
three real bugs, which is the argument for closing this kind of gap early
rather than at Milestone 8.

**What remains gated:** the raw sector read (`read_unbuffered`,
`FILE_FLAG_NO_BUFFERING`, `OVERLAPPED` offsets, buffer alignment) has still
never executed. Milestone 8 does not start until `rc smoke` has run against a
throwaway USB stick from an Administrator shell. The command is in
`docs/LIMITATIONS.md` section 1.1 and takes about ten seconds.

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
| OOXML sniffed rather than emitted as .zip | signature entry defers to the zip validator; validator not written yet |
| Fixtures rebuilt on the adversarial corpus | done, all eight |
| quickformat verified to retain content | done, checked empirically at build time |
| Precision measured alongside recall | not started, part of the acceptance test |
| Throughput measurable | see below |
| RSS ceiling testable | see below |

Built so far: the signature database (44 formats spanning image, video, audio,
document, archive and database) and its loader, with 11 tests. The scanner,
the validators and `rc-index` are next.

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

### Throughput and RSS, given what this machine can actually measure

Native Windows builds now work, which changes both answers.

**Throughput** should be measured natively on Windows against a real drive,
not under WSL. Every read from `/mnt/d` crosses the drvfs bridge at roughly
20 MiB/s regardless of file size, so no fixture built there can produce a
device throughput number - the bottleneck is the bridge, not the page cache
and not the scanner. Measuring on the actual target platform is both cheaper
and more meaningful than building a multi-gigabyte fixture inside WSL.

**Peak RSS** is graded two ways, because the literal criterion is not testable
here. This host has 7.8 GB of RAM and WSL is capped at 2.9 GB, so a "< 2 GB"
assertion could only fail by OOM, and passing it against a 512 MiB fixture
would prove nothing about whether the index is disk-backed. So the benchmark
reports peak RSS (satisfying the letter of SPEC.md section 5.8) *and*
asserts the property that actually matters: that RSS stays roughly flat as the
candidate count grows tenfold. A design that held candidates in RAM fails the
second test on any fixture size.

---

## Next: Milestone 3 (continued)

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
   `.zip`.

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
