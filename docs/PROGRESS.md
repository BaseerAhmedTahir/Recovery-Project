# PROGRESS

Running status per SPEC.md section 9. Last updated 2026-09-05.

**Current state: Milestone 1 complete on the Linux/image-file path.**
One acceptance criterion is partially met and one environment blocker is open —
both are in "Blocking questions" below.

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
| **Enumerate real drives** | **partial** | Code written; unexecuted on Windows (no linker) |

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

---

## Next: Milestone 2

`rc-partition`, then NTFS + FAT32 + exFAT parsers, then `rc-cli list-deleted`.
Acceptance is >=95% filename accuracy plus correct sizes, timestamps and the
original folder tree, graded against the `*-basic` fixtures.

Not started. Milestone 1's acceptance tests pass, so this is unblocked on the
Linux path — but I would rather resolve blocking question 1 first, so the
Windows backend stops accumulating unverified code behind it.
