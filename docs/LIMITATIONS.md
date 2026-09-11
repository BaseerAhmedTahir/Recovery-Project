# LIMITATIONS

What genuinely does not work, or is not yet verified. Kept current per SPEC.md
section 9: "I would rather know than be surprised during a real recovery."

Last updated: 2026-09-05 (end of Milestone 1).

---

## 1. Verification status of the code itself

### 1.1 The Windows raw-device path is VERIFIED (cleared 2026-09-06)

The MSVC toolchain is installed, the workspace builds and tests natively on
Windows, and `rc smoke` has now been run against a physical device.

Compiling the backend for the first time found three genuine bugs that review
had not caught: a wrong `ReadFile` buffer type, a borrow error in the
bounce-buffer path, and an IOCTL constant windows-sys does not export. That is
the argument for compiling unverified code as soon as it is possible rather
than trusting that it looks right.

**Smoke test result, 2026-09-06**, against `\.\PhysicalDrive3`
(Generic Flash Disk USB Device, 29.3 GB, 61,440,000 sectors of 512 bytes):

```
  LBA 0 (aligned)            offset            0  read  4096 of  4096 bytes
  4K-unaligned byte offset   offset          515  read  1000 of  1000 bytes
  mid-device (aligned)       offset  15728640000  read   512 of   512 bytes

range sha256 (1st read): c0d1cbb45add425c1f5fc88e9f86d1ab094494ae81d635a275b0d37fc034017c
range sha256 (2nd read): c0d1cbb45add425c1f5fc88e9f86d1ab094494ae81d635a275b0d37fc034017c

unchanged across reads:      yes
unaligned offset matches:    yes
misaligned buffer matches:   yes   (bounce path exercised: yes)
```

What that establishes, all of it previously unexecuted code:

- `FILE_FLAG_NO_BUFFERING` reads work against a physical device.
- The `OVERLAPPED` offset plumbing lands on the intended sectors, including
  15.7 GB into the device, so 64-bit offsets are split correctly across
  `Offset`/`OffsetHigh`.
- **The bounce-buffer path is correct.** Reading the same LBA into a
  sector-aligned `AlignedBuf` and into a deliberately skewed `Vec` slice
  produced identical bytes. `bounce path exercised: yes` confirms the skewed
  buffer really was unaligned, so this was not a vacuous pass - a lucky
  allocation would have reported `no`.
- Reads at an unaligned byte offset agree with the aligned read covering the
  same range.
- The device was not modified: the same byte range hashed identically on a
  second read.

Re-run any time with `.\scripts
un-smoke.ps1` from an Administrator shell.

**This clears the Milestone 8 gate.**


### 1.1a On this machine, C: and D: are the same physical disk

`Get-Partition` and `rc-image::resolve` agree that both live on
`\\.\PhysicalDrive0`, a Kingston SKC3000S1024G. Two consequences:

- A real recovery scan of `\\.\PhysicalDrive0` **cannot** write its output
  anywhere on C: or D:. `OutputSink` will refuse, and that refusal is correct.
  An external destination is required for any recovery of the internal drive.
- That NVMe reports TRIM, so per SPEC.md section 1.1 recovery from it is poor
  to impossible regardless of tooling. `rc devices` says so in its output
  rather than leaving it to be discovered mid-recovery.


### 1.2 The macOS backend is unverified and uncompiled

`backend/macos.rs` has never been compiled or run. No macOS machine is
available. The `DKIOC*` ioctl constants are transcribed from `<sys/disk.h>` and
have not been checked against a real system.

### 1.3 The Linux backend enumerates real devices, but its read path is untested on one

Enumeration **is** exercised against real hardware: `rc devices` under WSL2
lists the machine's actual block devices with model, size, sector size, rotation
and TRIM, read from sysfs, and correctly reports that their sectors need root.

What is still untested there is the *read* path: `O_DIRECT` against an actual
`/dev/sdX`, and the buffered + `posix_fadvise(POSIX_FADV_DONTNEED)` fallback.
Those need a root shell and a device worth reading, so they are covered by the
same `rc smoke` opt-in as the Windows path.

---

## 2. Toolchain and environment limitations on this machine

### 2.1 Native Windows builds now work (resolved 2026-09-06)

VS Build Tools 2022 is installed at `D:\VS\BuildTools` with the Windows SDK
at `10.0.26100.0`, so `cargo build`/`test` run natively. The full suite passes
on Windows.

One trap remains: MSYS2 ships a `link` command (GNU coreutils) that cargo will
pick up and mistake for MSVC's `link.exe`, producing a confusing
`extra operand` error. **Build from PowerShell, not from the MSYS bash shell**,
or keep the MSVC toolchain ahead of MSYS on `PATH`.

### 2.2 The C: drive was full; it is not any more (resolved 2026-09-06)

C: fell to ~250 MB free during Milestone 1, which blocked the toolchain
install. It now has ~14 GB. `RUSTUP_HOME`, `CARGO_HOME`, the build target
directory, the WSL toolchain and all fixtures still live on D:, and should
stay there.

### 2.2a Long filenames need extended-length paths on Windows

The corpus deliberately contains a 250-character filename, which is legal on
NTFS, FAT and exFAT alike but pushes a full path past Windows' 260-character
MAX_PATH as soon as it sits in any non-trivial directory.

This is not theoretical: it has now broken two separate pieces of the project's
own tooling. `make_corpus.py` could not create the file until it was taught to
emit the `\\?\` extended-length prefix, and `make-windows-fixture.ps1` failed
on its first run because `Copy-Item` cannot open such a path even though
`Get-ChildItem` will happily enumerate it.

The rule for any Windows-side tooling here: **use the .NET file APIs with a
`\\?\`-prefixed path, not the PowerShell cmdlets.** `Copy-Item`,
`Remove-Item` and `Test-Path` are unreliable past MAX_PATH in PowerShell 5.1.
`[System.IO.File]::Copy`, `::Delete` and `::Exists` honour the prefix.

Worth noting this says nothing about the *engine*, which reads raw bytes and
never opens a recovered file by path. It matters for the fixture generators and
it will matter again in Milestone 3, when carved files are written out to a
destination directory under their recovered names.

### 2.3 exFAT needs a FUSE driver under WSL2

The WSL2 kernel has no exfat driver, so `mount -t exfat` fails. Fixture
generation installs `exfat-fuse` and mounts via `mount -t exfat-fuse`. This
affects fixture *generation* only; parsing exFAT is our own code and does not
depend on a kernel driver.

### 2.4 Loop devices under WSL2 cannot expose partitions

`/sys/module/loop/parameters/max_part` is 0, so `losetup --partscan` never
creates `/dev/loopNpM` nodes. The `nopart` fixture works around this by writing
the GPT directly into the image file with `sfdisk` and then attaching a loop
device windowed onto the partition with `--offset`/`--sizelimit`. Same bytes on
disk, but be aware the workaround exists if you port fixture generation
elsewhere.

### 2.5 Fixture generation requires WSL and passwordless sudo

`testdata/build_fixtures.sh` must run under Linux. It relies on
`/etc/sudoers.d/recovery-core-fixtures`, which grants NOPASSWD for exactly
`losetup`, `mount`, `umount`, `mkfs.*`, `chown`, `blockdev`, `sfdisk` and
`sync`. Reinstall it with:

```bash
wsl -d Ubuntu -u root -- bash testdata/lib/install_sudoers.sh
```

### 2.6 Fixture builds are slow over `/mnt/d`

Images are written directly to the Windows D: drive because the WSL distro
itself lives on the full C:. drvfs is slow for many small files: the
`fragmented-jpeg` fixture writes ~16,000 filler files and takes about five
minutes. A full `build_fixtures.sh` run takes roughly ten.

---

### 2.6a The test suite needs the scanner optimised even in debug

`Cargo.toml` sets `opt-level = 2` for `rc-carve`, `rc-index` and `rc-image` in
the dev profile, not just for third-party dependencies. This is not tuning for
its own sake: `rc-carve`'s inner loop runs over every byte of a 512 MiB fixture
several times, and unoptimised that measured **3175 seconds** for one test
binary against roughly 127 seconds optimised - a 25x difference, with identical
results.

A suite nobody will sit through stops being run, which fails the same way a
suite that silently skips does. Debug assertions stay on for all three crates:
they parse structures off a damaged disk, and an overflow check is worth more
than a readable stack frame.

The consequence to remember is the one that already cost time here: **any
performance number measured in a debug build is meaningless**, because
dependencies are optimised and workspace crates were not. That is how the
prefilter's memchr/table threshold came to be set wrongly the first time. See
section 3.6.

---

### 2.7 The corpus is reproducible per toolchain, not across toolchains

`make_corpus.py` is fully deterministic on a given machine: two consecutive
runs produce byte-identical output for all 315 files, and a run today matched
the manifest that populated the Windows VHD earlier. It is *not* reproducible
across toolchains. Comparing the WSL-built `ntfs-basic` corpus against the
Windows-built `ntfs-windows` corpus, 58 of 315 files differ in content while
every one of them has an identical size:

| Kind | Files differing | Cause |
|---|---|---|
| `sqlite` | 16 of 16 | `SQLITE_VERSION_NUMBER` is stored at header offset 96. WSL had SQLite 3.46.1, Windows had 3.50.4. Page size, page count and freelist count are identical in both. |
| `docx` | 42 of 42 | DEFLATE output differs between zlib versions. The ZIP timestamps are already pinned to a fixed DOS epoch, so the timestamps are not the cause. |

Consequences, in order of how easy each is to get wrong:

1. **Each `expected.json` is only ground truth for its own image.** They are
   generated together and are internally consistent, so byte-exact recovery
   checks remain valid per image. Never validate one image against another
   image's manifest.
2. **A cross-image comparison must be made on paths, sizes, states and
   structure - not on content hashes** for the `sqlite` and `docx` files.
3. **The `.gitignore` comment calling fixtures "reproducible" is true only on
   the machine that built them.** Regenerating a manifest to repair a lost one
   works only if the same Python, zlib and SQLite versions are still installed.

**This is now recorded rather than remembered.** `make_corpus.py --provenance`
emits the toolchain - Python, SQLite library, zlib, platform, the generator's
own SHA-256, and the git commit - and every `expected.json` writer embeds it
under `generator.provenance`. The ground-truth writers refuse to re-describe an
*unchanged* image with a manifest built by a different toolchain, which is the
one operation that produces a correct `image_sha256` alongside 58 silently
wrong per-file hashes. A genuine rebuild, which produces a new image and a new
image hash, passes through untouched.

The seven fixtures built under WSL predate the field and report `NONE
RECORDED`; rebuilding through `build_fixtures.sh` adds it. `ntfs-windows` has
it.

This is not a defect to fix. Format diversity is the point of having an
independent sample: the Windows fixture genuinely contains SQLite databases
written by a different library version and ZIP members deflated by a different
compressor, which is more coverage than a bit-identical copy would give.

---

## 3. Functional limitations by design

### 3.1 Not yet implemented

Milestones 1 and 2 deliver device access, imaging, partition discovery and
deleted-entry recovery for NTFS, FAT12/16/32 and exFAT. There is still **no**
signature carving, **no** fragment reassembly, **no** scoring and **no** mobile
support. `rc` has five subcommands: `devices`, `image`, `verify`, `smoke`,
`list-deleted`.

**ext4, APFS and HFS+ are not implemented.** `rc list-deleted` says so
explicitly rather than reporting zero deleted files, which would be a lie.
ext4 (with JBD2 journal replay) is Milestone 6.

### 3.1a A deleted FAT or exFAT file's layout is a guess, not a fact

FAT and exFAT both clear a file's cluster chain when it is unlinked, keeping
only the starting cluster in the directory entry. The rest of the layout is
**not recoverable from the filesystem**. Assuming contiguity is a guess, and it
is wrong exactly when the file was fragmented.

This is modelled explicitly: such files carry
`DataLocation::FirstClusterOnly`, whose `is_exact()` is false, and
`rc list-deleted` prints "first cluster N only (chain lost on delete)" rather
than a run count. Nothing downstream may treat it as a known layout until
`rc-bifrag` lands in Milestone 4.

NTFS is different: the runlist lives in the MFT record and survives deletion,
so a fragmented NTFS file reports its exact extents.

### 3.1b FAT timestamps have no timezone

FAT stores local time with two-second granularity and records no UTC offset at
all, so an exact instant is not recoverable - only whatever the writing
machine's clock was set to. `Timestamps::utc_known` is false for FAT and the
CLI appends `?` to those times. exFAT records an explicit UTC offset byte and
NTFS uses UTC throughout, so both are exact.

Milestone 8's deletion-date filter must not treat FAT timestamps as UTC.

### 3.2 Recovery outlook is genuinely poor on some media

Restating SPEC.md section 1.1, because these are not tool limitations but
physical ones:

| Medium | Reality |
|---|---|
| SD cards, USB flash, camera cards | Excellent. Usually no TRIM. |
| Mechanical HDD | Excellent. Data persists until overwritten. |
| SSD with TRIM active | Poor to impossible. The controller returns zeros. |
| Windows NVMe with BitLocker on | Requires the volume unlocked; carve the decrypted volume. |
| macOS Apple Silicon internal SSD | Effectively impossible. Sealed volume, hardware key. |
| Non-rooted Android/iOS internal NAND | Block carving impossible. Logical extraction only. |

`rc devices` prints a per-device outlook derived from what the device actually
reports, rather than implying every drive is recoverable.

### 3.3 Destination resolution is best-effort

`rc-image::resolve` maps an output path to its backing physical device so the
sink can refuse to write onto a device being scanned. It can legitimately fail
to resolve (network shares, unusual FUSE mounts, container overlays), in which
case it returns `Backing::Unknown` and the sink continues rather than refusing.
`rc image` prints the reason on its own line before starting. Pass
`--strict-destination` to make an unresolvable destination a hard error instead.

This is not hypothetical on this machine: a destination under `/mnt/d` resolves
to `Backing::Unknown`, because drvfs is not a block device. Writing a clone
there is safe -- it cannot be the device being scanned -- but it does mean the
physical-device comparison contributes nothing in that configuration, and only
the direct same-file check is doing work.

The direct case — writing onto the very image file being scanned — is always
caught regardless, and is covered by a test.

### 3.4 An incomplete clone's hash does not describe the source device

If any region was unreadable, the clone contains fill bytes where real data
should be. The `.hash` manifest records `complete: false` and `bad_bytes`, and
`rc verify` says so explicitly. Do not compare such an image's hash against the
physical device and conclude anything from the mismatch.

---

## 3.5 The Milestone 2 accuracy score measures metadata, not recovery

`docs/PROGRESS.md` records 100% name, path and size accuracy for NTFS, FAT32
and exFAT. Read that narrowly:

- It is derived entirely from directory entries and MFT records. It says
  nothing about whether a file's **content** can be recovered.
- FAT32 and exFAT score 16/16 on size for a file whose layout they report as
  `first cluster only`. The size is exact; the location of most of the bytes
  is unknown. Those two facts sit in the same row of the same table.
- The sample is 16 files on one on-disk layout per filesystem. Zero misses
  allowed is high-variance, not strong evidence.

Byte-level recovery is what Milestones 3 and 4 grade.

---

### 3.6 The carving result is a fixture result, not a drive result

The scanner recovers 228 of 228 carvable files byte-exactly from
`quickformat.img`, emitting exactly 228 candidates - one per file, nothing
else. That is a real result and it is also a *fixture* result. Four reasons not
to read it as a claim about a real drive:

1. **The fixture is 98.6% zeros.** That is why `00 00 01 00` matched 3782 times
   before the ICO validator existed. A used drive carries far more entropy in
   its free space, and entropy is what generates plausible-looking headers.
   Expect more false positives there, not fewer.
2. **Every file in it is contiguous.** Carving a fragmented file is Milestone 4
   and none of this measures it. The `fragmented-jpeg` fixture exists precisely
   because contiguous-only carving should recover *none* of its files
   byte-exactly.
3. **Every file is one of eight formats this corpus generates.** A real drive
   holds formats the database does not know, formats it knows without a
   validator, and half-overwritten remnants of files deleted months earlier.
4. **8406 header matches per GB is a property of this image.** The ratio is
   worth watching across scans; the absolute number is not transferable.

The honest summary is that the carve pipeline is correct on data whose ground
truth is known, and untested on data whose ground truth is not. That second
category is every drive anyone would actually run this on.

### 3.6a A carved PE loses any overlay data

A Windows executable may carry an "overlay": bytes appended after the last
section, which the loader ignores and which no header describes. The PE
validator walks the section table and adds the Authenticode certificate table
when data directory entry 4 names one - that entry is unusual in holding a file
offset rather than a virtual address - but an overlay that is neither is
unbounded from inside the file.

The Python interpreter in the independent corpus is exactly this case: 10
sections ending at 98816 bytes, no signature, and 2661 bytes of overlay, for a
101477-byte file. A carve of it recovers a binary that will run and is not
byte-identical.

This is a genuine limit rather than a bug, and it was found by a real file:
hand-built PE vectors have no overlay because I would not have thought to add
one. Bounding it needs content heuristics and belongs with `rc-bifrag` in
Milestone 4.

### 3.7 Twenty-eight of the 44 signatures have a validator; 16 do not

Counted, not estimated: 28 entries name a validator, drawn from 23 distinct
implementations - the three RIFF forms share a walk, MP4 covers HEIC and M4A,
TIFF covers both byte orders, GIF covers 87a and 89a, and RAR covers 4 and 5.

A signature without a validator is a header match and nothing more: it is
emitted with no length and a `Partial` status saying why. That is not a
cosmetic gap. On the format fixture, 38 of 38 formats are located and only 22
are recovered byte-exactly, and the difference is exactly the formats whose end
the carver cannot establish.

The 16 remaining need decoding rather than a header read, which is why they are
still open:

| Format | Why the end is hard |
|---|---|
| `gzip`, `bzip2`, `xz` | the length is only known after decompressing, or - for bzip2 - by finding a *bit*-aligned end-of-stream magic |
| `mp3_id3`, `flac`, `ogg` | frame or page walks; Ogg additionally needs its own CRC variant, which is not the CRC-32 used by PNG and ZIP |
| `ole2`, `mkv`, `flv`, `asf_wmv`, `mpeg_ps` | nested structures with no total in the header |
| `rar`, `rar5` | a block chain, encoded differently in each version |
| `sqlite_wal` | frames chain by checksum; the count is not stored |
| `vhd` | a dynamic VHD's size comes from its block allocation table |
| `eml` | a mail message has no length at all - it ends where the next thing begins |

Earlier drafts of this section named ICO, PE, GIF, TIFF, 7z and RAR as the next
targets. Those are done; the list above is what is genuinely left.

---

## 4. Test-coverage gaps

- **No bad-sector hardware test.** The trim and scrape passes are exercised by
  a fault-injecting device wrapper in `rc-image`'s tests, not by a physically
  failing drive. Real drives fail in messier ways: timeouts of tens of seconds,
  resets that drop the device node, partial reads that return garbage rather
  than an error.
- **No 4Kn native device test.** `SectorSize::S4096` is unit-tested against an
  image, never against real 4Kn hardware.
- **No concurrent-access test.** Reading a device while the OS is writing to it
  is a real scenario (`FILE_SHARE_WRITE` is deliberately set) and is untested.
- **`FSCTL_LOCK_VOLUME` is not implemented.** SPEC.md section 5.1 lists
  optional volume dismount to stop cache interference; not done.
- **No test for a source larger than 2 TB**, where 32-bit offset bugs would
  surface.
- **No carving precision measurement yet.** Recall without precision is
  meaningless for a signature carver; see `docs/PROGRESS.md`.
- **The candidate index has never been under memory pressure.** The 2 GB RSS
  ceiling from Milestone 3 cannot be tested against 512 MiB fixtures. The
  scanner's own buffers are bounded - block buffers are recycled through a
  channel and the validation buffer is reused - but the candidate list is held
  in memory and grows with the number of hits, which is what `rc-index` is for.
- **Carving throughput is measured on one machine's NVMe only.** The
  benchmark reads the fixture unbuffered, so it describes a real disk rather
  than the page cache - 336 MiB/s, CPU-bound, against a raw read ceiling of
  1256 MiB/s - but it has never been run against a USB stick, an SD card or a
  spinning disk, where the balance should flip to I/O-bound. The earlier claim
  that the scan was "I/O-bound on any real medium" was wrong and is corrected.
- **The scan workers scale poorly.** Eight threads reach roughly 1.8 times the
  one-thread rate. On storage fast enough to outrun the engine, that - not the
  disk - is the limit. The shared block channel is the first suspect; it is not
  investigated yet.
- **The independent corpus is not in the disk images.** ImageMagick and ffmpeg
  now supply real files for all eight formats the generated corpus could not
  vouch for, but those samples are validated as bytes rather than written into
  the fixtures. Validator grading does not need a filesystem; the Milestone 3
  *carving* acceptance test does, and folding them in is a prerequisite for it.
- **GIF and TIFF have signatures but no validator**, so they are header-match
  only. ImageMagick can produce both when that changes.

---

## 5. Benchmarking is not meaningful on this setup yet

Cloning a 512 MiB fixture through `/mnt/d` runs at roughly 18 MiB/s while the
CPU sits idle, because every read crosses the WSL drvfs bridge. Any throughput
number measured this way describes the bridge, not the engine.

Milestone 3 requires a real MB/s figure. To produce one, the fixtures and the
scan must live on native storage -- either a native Windows build (blocked, see
2.1) or fixtures inside the WSL ext4 filesystem (blocked by C: free space, see
2.2).

---

## 6. Cleared limitations

- **2026-09-05 -- "enumerate real drives" on Linux.** `rc devices` lists the
  machine's real block devices from an unelevated shell, with model, size,
  sector size, rotation and TRIM, and reports "requires root to read sector
  data" rather than hiding devices it cannot read. Verified by running it under
  WSL2. This does **not** clear 1.1: the Windows path is still unexecuted.
