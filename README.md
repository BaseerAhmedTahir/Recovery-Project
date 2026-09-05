# RECOVERY-CORE

An offline, cross-platform data recovery engine for hardware I own. See
[`SPEC.md`](SPEC.md) for the full specification, and
[`docs/PROGRESS.md`](docs/PROGRESS.md) for what is actually built.

**Status: Milestone 1 (device access + imaging).** No filesystem parsing, no
carving, no scoring, no mobile support yet.

> Read [`docs/LIMITATIONS.md`](docs/LIMITATIONS.md) before pointing this at
> anything you care about. In particular, the Windows raw-device read path has
> not yet been executed against real hardware.

---

## What it does today

```
rc devices                          # list drives (works without elevation)
rc image <source> <output.raw>      # ddrescue-style clone with .map + .hash
rc verify <image.raw>               # check an image against its .hash manifest
rc smoke --device <path>            # opt-in real-hardware read check
```

Every subcommand accepts `--json` for machine-readable output, because the GUI
in Milestone 8 is a thin client over exactly this.

## The one rule

`rc-device` has no write path at all. It is a sealed trait with read-only OS
handles, and every byte the project emits goes through `rc-image::OutputSink`,
which refuses any destination resolving to a device currently being scanned.

This is enforced in the type system, by the kernel, across crates, and by tests
that hash a fixture before and after a full API pass.
[`docs/SAFETY.md`](docs/SAFETY.md) explains how to verify each claim.

---

## Building

Requires Rust stable (2021 edition).

```bash
cargo build --workspace
cargo test --workspace
cargo xtask ci          # fmt, clippy, no-network audit, tests
```

### On this machine specifically

C: is effectively full and has no MSVC linker, so builds and tests currently run
inside WSL2 with everything on D::

```bash
export RUSTUP_HOME=/mnt/d/Rust/wsl-rustup
export CARGO_HOME=/mnt/d/Rust/wsl-cargo
export CARGO_TARGET_DIR=/mnt/d/Rust/wsl-target/recovery
export PATH="$CARGO_HOME/bin:$PATH"
cargo test --workspace
```

To build natively on Windows, free space on C: and install the Visual Studio
C++ build tools. See `docs/LIMITATIONS.md` section 2.

---

## Test fixtures

Ground truth is generated, not downloaded. `testdata/build_fixtures.sh` builds
eight 512 MiB images with a recorded `expected.json` for each:

| Fixture | What it exercises |
|---|---|
| `ntfs-basic`, `fat32-basic`, `exfat-basic`, `ext4-basic` | Deleted-entry recovery per filesystem |
| `fragmented-jpeg` | Fragment reassembly (Milestone 4) |
| `quickformat` | Signature carving with no metadata (Milestone 3) |
| `overwritten` | Green/Yellow/Red classification (Milestone 5) |
| `nopart` | Lost-partition rebuild (Milestone 3) |

The corpus is built by `testdata/corpus/make_corpus.py` from the Python standard
library alone, so the JPEG, PNG, PDF, MP4, DOCX and SQLite files have exactly
known internal structure and are reproducible on a given machine. (SQLite output
depends on the SQLite build inside the running Python, so it differs between
Windows and WSL; `expected.json` always records the bytes actually written, so
ground truth stays self-consistent either way.)

```bash
wsl -d Ubuntu -- ./testdata/build_fixtures.sh          # all fixtures
wsl -d Ubuntu -- ./testdata/build_fixtures.sh --list
```

---

## Layout

```
crates/
  rc-device/   read-only raw block I/O; the only path to a scan source
  rc-image/    ddrescue-style cloning, hashing, and the only write path
  rc-cli/      the `rc` binary; the source of truth for all capability
  xtask/       the no-network dependency audit and the CI gate
testdata/      fixture generator and deterministic corpus
docs/          PROGRESS, LIMITATIONS, SAFETY
```
