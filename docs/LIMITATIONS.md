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

Milestones 1 to 9 deliver device access, imaging, partition discovery,
deleted-entry recovery for NTFS, FAT12/16/32, exFAT and ext2/3/4 (through the
JBD2 journal), signature carving with validators, fragment reassembly,
Green/Yellow/Red scoring, resumable carving and previews, SQLite deleted-row
recovery, and logical mobile extraction (Android over adb, iOS backups, host
backups, the companion bridge). Recovered files are written out with
`rc restore` (filesystem entries) and `rc extract` (carved candidates, with
`--reassemble` for fragmented ones), only through the output sink that refuses
the device being read. A desktop app (Tauri) wraps the engine, and an Android
companion app recovers what a phone itself can reach.

**APFS and HFS+ are detection only.** Both are recognised and their headers
read; `rc list-deleted` then says what the volume is and that deleted-file
recovery from it is not implemented, and points at `rc carve`. No catalog
B-tree, object map, older checkpoint or snapshot is read. The reason is
evidence, not effort: no tool on this machine can make an APFS or HFS+ volume
with files on it (no macOS, no hfsplus module in the WSL kernel, and
`mkfs.apfs`/`mkfs.hfsplus` make empty volumes only), and a deleted-record
parser checked against nothing would be a stub pretending to be a feature. The
header parsers themselves are checked only against hand-built headers laid out
from Apple's specifications.

### 3.1c ext4 recovery depends on the journal, and the journal is short

Measured on the ext4 fixture (Linux 6.6 kernel): unlink zeroes a file's extent
tree and size, and its name does not survive in the live directory block. So
without the journal an ext4 deletion leaves nothing to recover but that an inode
was used. Every one of the fixture's 111 deleted files is a journal-only
recovery: name from a logged directory block, extents from a logged pre-deletion
inode, content byte-exact (`rc-fs/tests/ext4_journal.rs`).

- The journal is a ring (16 MiB on a 512 MiB volume, up to 1 GiB on large
  ones). Once later transactions overwrite a deletion's predecessors, that file
  cannot be located through metadata at all; carve instead. On a busy root
  filesystem that can be minutes.
- Only metadata is journalled in the default `ordered` mode, so the content is
  read from the blocks the old extents name. If those blocks were reused, the
  bytes are the new file's; scoring reads them and rates accordingly.
- Deleted directories are recovered when a copy of their inode survives; files
  inside a deleted directory whose inode copy has gone get no path.
- Tested kernels: one (6.6, WSL2). The slack after live directory entries is
  read for deleted names too, but on this fixture no name survived there, so
  that path is exercised only by a unit test.
- Block maps (ext2/ext3), holes and symlinks are checked on a volume made by
  `mke2fs -d` with nothing deleted (`ext3-indirect`); deletion recovery on a
  block-mapped volume is untested, and ext3 zeroes block maps on delete too.
- Not handled: `inline_data` directories, encrypted (`fscrypt`) names and
  content, `bigalloc` clusters, external journal devices, and fast-commit
  blocks. An encrypted directory's names come out as ciphertext.

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

### 3.6b A fragmented file can still validate, in the modes that are not decoded

Until Milestone 4, the carver reported fragmented files as complete. On the
fragmented fixtures every JPEG came back `Valid` with an established length and
the wrong bytes:

| File | Restart interval | Reported | Really |
|---|---|---|---|
| large_b.jpg | every 50 MCUs | Valid, 198,254 bytes | 99,950 |
| large_f.jpg | none | Valid, 267,933 bytes | 136,861 |
| large_a.jpg | every 4 MCUs | Valid, 77,824 bytes | 61,440 |

A user would have been handed corrupt files marked GREEN.

The reason is worth stating plainly, because it is not obvious: **markers
cannot see insertion.** When a file is split, the filler sits *in the middle*
of its entropy stream and the file's own data continues on the far side, so the
next restart marker carries exactly the phase the sequence expects. Nothing in
the marker sequence is out of order, and nothing is missing. The byte bound
that catches a long gap is the format's worst case (512 bytes per block, versus
10-30 in real data), so at an interval of 50 MCUs it allows 153,600 bytes of
anything.

Only decoding closes it, and Milestone 4 decodes: a sequential Huffman scan is
Huffman-decoded and the MCUs between restart markers counted
(`validate/jpeg_entropy.rs`). All three files above are now caught at the byte
where the insertion begins.

What remains uncovered:

1. **Progressive, lossless and arithmetic-coded JPEG are not decoded.** They
   use a different entropy structure - spectral bands across passes,
   difference coding, a different coder - and fall back to the byte-level
   marker checks, which insertion defeats. 2 of the 12 foreign JPEGs in the
   independent corpus are progressive. Cameras write baseline; web images are
   often progressive.
2. **An MP4 that is not H.264, or that claims `avc1` without `avcC`.** There is
   no sample structure to check, so a spliced file can still add up.
3. **Any format whose payload carries no internal landmark**, which is the same
   gap seen from the other side: a PNG written as one huge IDAT chunk has no
   checksum until its end, and an MP4 whose `moov` follows its `mdat` has no
   sample table to check against while reading forwards.

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

### 3.8 Fragment reassembly recovers some fragmented files, not most

`rc-bifrag` (Milestone 4) stitches a file back together from fragments that
ascend on disk, judging every candidate cluster by the format's validator. What
it actually recovers, byte-exactly, on the two fragmented fixtures - measured
from each file's true header, against layouts recovered from the images rather
than assumed:

| Fixture | Recovered | Contiguous-only baseline |
|---|---|---|
| `fragmented-jpeg` (constant filler, every gap 32 KiB) | **4 of 6** | 0 of 6 |
| `fragmented-hard` (gaps hold same-encoder media, 4 KiB to 1 MiB) | **2 of 6** | 0 of 6 |

The four ways it fails:

1. **An MP4 whose `moov` box follows its `mdat`.** Reading forwards from the
   header there is no index, so no cluster can be verified against anything and
   the search has nothing to work with. This is what phone and camera
   recorders write, because they cannot know the index until recording stops.
   Both fixtures contain one and neither recovers it. Closing this needs
   format-driven pointer following (SPEC.md 5.6.4): find the `moov`
   separately, then place samples by their recorded chunk offsets.
2. **Sparse landmarks and cost together, which stop the PNG.** ImageMagick
   writes this 916 KiB image as 28 IDAT chunks of 32 KiB (measured, not
   assumed: an earlier note here said 8 KiB from libpng's documented default,
   which is not what came out). A checksum only every eight clusters means a
   fragment boundary inside a chunk cannot be confirmed until the whole chunk
   is assembled, so the search must guess a gap right before anything tells it
   so. Each wrong guess costs a full re-validation, because every candidate
   re-validates the whole file so far - the work is (file size) x (candidates
   tried). 4 GiB of validation reaches 885,305 of 916,513 bytes on the easy
   fixture and does not cross the first gap on the hard one. A validator that
   could resume from its last landmark instead of restarting at byte zero
   would change the arithmetic; it is not implemented.
3. **Decoys that pass the junction check by chance.** For a decoy cluster to
   be accepted, its own restart interval must end where ours is due and its
   restart phase must match: reasoning from the structure, roughly one
   candidate in 32 at an interval of 4 MCUs. That rate is an estimate from the
   two conditions, not a measurement. What is measured is the consequence: with
   gaps up to 1 MiB there are hundreds of candidates per boundary, a false one
   is usually met before the true one, and `large_a.jpg` on the hard fixture
   spends its whole budget after placing 2 of its 7 pieces. Ranking candidates
   by DC continuity - how well the decoded DC coefficients continue the image
   above them - rather than taking the first that fits, is the known next
   step.
4. **Fragments that are not in ascending order.** The whole method assumes they
   are. FAT allocates forwards and all 12 recorded layouts in the fixtures
   ascend, but a file extended long after it was written, or one on a heavily
   churned NTFS volume, can have a fragment before its first. Not handled, and
   not detected either: such a file simply fails to reassemble.

What holds regardless, and is asserted by the test: **no file that fails to
come back byte-exactly is reported `Valid`.** A wrong file labelled complete is
the outcome that matters most, because it is the one a user would keep.

### 3.9 Scoring rates on evidence, and some damage leaves none

`rc-score` (Milestone 5) marks a cluster lost only on evidence: allocated to a
live file, one repeated byte where the format cannot have that, no longer text
in a text file, or where the format's validator breaks. On the overwritten
fixture that gives no false alarms (86 of 86 intact files GREEN) and three
misses, asserted exactly by the test:

- **Random bytes over a format-less file** (a .bin blob) leave no evidence, so
  the file rates GREEN. Files with no validator say `no-structural-check`.
- **MP4s that are not H.264 with `avcC`** have nothing in their media to check.
  The basic corpus's MP4s are like that (random samples, a generator shortcut),
  so one-cluster and half-file damage to them rates GREEN. The same damage in a
  real ffmpeg H.264 file is caught and placed (`rc-score/tests/real_media.rs`).
  HEVC and other codecs are in the same position as the corpus files.
- **Validators confirm only the first break.** Half of a real H.264 file
  overwritten rates YELLOW, not RED, because only one lost cluster is
  confirmed. Pinned in `real_media.rs`.

Also: a deleted file whose MFT record was reused by a later file cannot be
found through metadata at all, so it is never scored - carving is the only way
back to it. And TRIM-plus-zeros detection depends on the device reporting TRIM;
image files report unknown, so the `trimmed` rule never fires on an image.

### 3.10 Resume re-does at most one checkpoint interval

A killed carve resumes from the last committed segment, so up to
`--checkpoint-secs` (default 5 s) of scanning is repeated. Ctrl+C stops at the
next segment boundary rather than instantly. Resume refuses a device whose size,
sector size, serial or first-and-last MiB differ; a disk that has been written
to *inside* its first and last MiB since the checkpoint would pass that check,
though writing to a disk under recovery is exactly what not to do. Only `rc
carve` is resumable; `rc image` resumes from its own map file (Milestone 1) and
filesystem scans are fast enough not to need it.

### 3.11 ffmpeg's sandbox does not deny filesystem access

Video previews run ffmpeg with a hard timeout and a memory cap (a Windows Job
Object, setrlimit on Unix) and give it no file paths - input on stdin, output
on stdout. The operating system does not stop ffmpeg opening files if a
malicious input exploited it; that needs an AppContainer on Windows or
namespaces on Linux, and neither is implemented. On Windows the process is
placed in the job just after it starts, so a child started in that instant
would escape the limits. ffmpeg is found on PATH, not bundled.

### 3.12 Mobile: what is verified, and against what

Nothing in Milestone 7 has touched a real phone. No phone is connected to this
machine and the Android SDK here has no emulator system image.

- **Android over adb** is tested against a *scripted* adb
  (`testdata/mobile/fake_adb.py`) that prints output in the formats adb and
  Android's toybox use. That checks rc-mobile's side: the readiness checklist,
  parsing, finding `.trashed-*` files, pulling, hashing and the manifest. It
  does not prove a real phone answers the same way. To check on your phone:
  unlock it, enable USB debugging, accept this computer's key, then
  `rc android checklist` and `rc android pull --out <new dir>`.
- **Lock state** is read from `dumpsys window`, whose fields differ between
  Android versions. When none is recognised the checklist says so and
  extraction needs `--confirm-unlocked`; a lock screen reported as showing is
  always refused.
- **MediaStore's trashed rows**: from Android 11, MediaStore hides trashed items
  from ordinary queries, and `content query` from the shell may return none of
  them. Trashed files are therefore found by listing shared storage for
  `.trashed-*` names, which does not depend on that; the MediaStore rows are
  reported when the query returns them.
- **App-private data** (`/data/data`, most chat databases) is not readable over
  adb without root, and is not attempted. `adb backup` is offered only below
  API 31 and most apps opt out of it anyway.
- **iOS backups** are tested against a *synthetic* backup
  (`testdata/mobile/make_ios_backup.py`): real SQLite and real plists laid out
  as backups are documented, not made by an iPhone. Column names in
  `Photos.sqlite` change between iOS versions; ZASSET (iOS 14+) and
  ZGENERICASSET (earlier) are handled, others fail with a message.
- **Encrypted iOS backups are refused**, not decrypted: there is no way here to
  make one to test decryption against. Pre-iOS 10 backups (`Manifest.mbdb`) are
  refused too. `rc ios backup` needs libimobiledevice's `idevicebackup2`, which
  is not installed here, and is unverified.
- **Host backups**: discovery is checked on a fake profile. DriveFS cache chunks
  are identified by signature only; mapping them back to file names is not
  implemented.
- **The bridge** is tested with a Rust client over loopback. Wi-Fi mode is not
  implemented.

### 3.13 SQLite row recovery

`rc sqlite` recovers deleted rows from freeblocks, unallocated page space,
freelist pages, and older page versions in a WAL. Measured on databases written
by SQLite 3.50.4 with Android's `sms` schema: **41 of 41** deleted rows whose
bytes survive in a rollback-journal database and **34 of 34** in a WAL
database, every column exact, no live row reported, nothing invented; nothing
from a `secure_delete=ON` database. Of the 74 rows deleted from the rollback
database, 33 had already been overwritten by SQLite and are gone.

Not recovered: rows whose payload spilled onto overflow pages (only records that
fit on their page are carved from free space), rows in freeblocks where the
four-byte freeblock header reached past the rowid alias column into other
serial types, `WITHOUT ROWID` tables, and rollback journal files (`-journal`).
Rows are matched to tables by column count and declared types, so two tables
with the same shape can be confused. The whole database is read into memory.

### 3.14 The desktop app: what is measured, and what it hands to the CLI

The grid is measured, at 10 million rows, in two halves.

- **Backend** (`rc-results/tests/ten_million.rs`): a screen of 60 rows comes
  back in a **median of 0.8 ms** unfiltered, 1.8 ms filtered and 4.5 ms after a
  sort, with 99th percentiles of 2.3 / 5.0 / 8.3 ms on an idle machine - inside
  one 16 ms frame. Under load those p99s drift (10-17 ms seen while the machine
  was compiling), so the test asserts the median against the frame and puts a
  100 ms ceiling on the p99 rather than being flaky. Building a *filtered* view
  is a table scan: **7 s** for a band-and-type filter, **17 s** for a text
  search with a sort, at 10 million rows. That runs off the UI thread and the
  grid says "Filtering…"; it is not instant and is not presented as such.
  Inserting 10 million synthetic rows takes 71-114 s.
- **Frontend** (measured in a browser against a mock backend): at most **42
  rows** exist in the DOM at any time, every visible row was filled within
  60 ms on **200 of 200** random jumps, and the last row is reachable. Frames
  during jumps: 16 ms median, 30 ms at p99. Smooth wheel scrolling: **29 ms
  median, 59 ms p99** - two to four frames, because each scroll event re-renders
  the visible rows. Usable, not perfectly smooth.

Browsers cap an element's height near 33 million pixels and 10 million rows is
280 million, so the scrollbar is scaled: at that size one pixel of scrollbar is
about eight rows, and the keyboard (arrows, Page Up/Down, Home/End) is the way
to move row by row.

The default screen is a five-step wizard (what, where, scan, choose, recover)
with a picture grid of thumbnails decoded from the recovered bytes; each new
wizard scan starts from an empty result list. Video thumbnails need ffmpeg and
show a placeholder without it. The window's own file and folder dialogs come
from Tauri's dialog plugin, the one capability the app has beyond its own
commands (`cargo xtask audit-gui` checks that list).

Screens that call the engine directly: sources and devices, filesystem scan,
carve, the results grid, ratings and their reasons, preview, hex, and restore.
Everything else - phones, iOS backups, host backups, SQLite row recovery,
imaging and verification - is a form that runs the `rc` binary beside the app
with `--json` and shows what it printed. That is deliberate (the CLI is the
source of truth) and visible: the tab is named for it.

Not implemented in the GUI: a device tree of partitions to pick from (the
whole device is scanned), and pausing or resuming a carve from the window
(`rc carve --resume` does it). The wizard's save and done screens were checked
against the browser stand-in; in the real window the flow was driven as far as
the results grid on the ext4 fixture (46 photos and videos, thumbnails
rendered), and restore itself is covered by `rc-results/tests/operations.rs`.

### 3.14a Packaging is built and run here; installing it is not

`packaginguild-windows.ps1` produces a runnable folder and a 3.3 MB NSIS
installer, and `build-android.ps1` produces a 9.5 MB debug APK. The folder's
`rc-gui.exe` has been launched here and the APK's permissions read back from
the built package. Neither the installer nor the APK has been *installed*: this
machine's C: drive has under 1 GB free, and no phone is attached. The APK is
signed with the SDK's debug key; a release APK is left unsigned for you to sign.

### 3.16 Drive letters and folders

A drive letter (`\\.\D:`) is read exactly like a disk: it begins with its
filesystem's boot sector, which the partition step recognises as one whole
filesystem without a sweep. Reading one needs Administrator, like a disk.
Listing them does not. Every NTFS, FAT and exFAT fixture is laid out this way
already, so the parsers are exercised on this shape by every test; what is not
covered here is the Windows volume handle itself, because a test run has no
Administrator rights. Known specifics: a volume handle answers the geometry
IOCTL with the *disk's* size, so the length comes from
`IOCTL_DISK_GET_LENGTH_INFO`, and NTFS keeps a backup boot sector past the end
of the filesystem, which needs `FSCTL_ALLOW_EXTENDED_DASD_IO` to read.

**Where recovered files may be written changed with this.** A scanned *disk*
still refuses every destination on that disk. A scanned *volume* refuses
destinations on that volume only: another partition is a different set of
sectors and writing there cannot overwrite what is being recovered. The volume
is registered under its `\\?\Volume{GUID}` name, which is what a destination
resolves to; `rc-image/tests/volume_guard.rs` asserts both halves. Before that
registration existed, scanning `\\.\D:` and writing to `D:\out` was allowed -
the destination resolved to `\\.\PhysicalDriveN`, which never equals `\\.\D:`.

"Scan a folder" is a whole-drive read narrowed to the files whose recorded
path was inside that folder. Carved files have no original path, so a folder
scan reports only what the filesystem still remembers, and the deep scan is
switched off for it. Folders on network drives and cloud placeholders are
refused: there is no volume to read.

### 3.16a Administrator, and what the app does about it

Reading any drive letter or disk needs Administrator on Windows. The app now
asks the engine whether a source can be read *before* it starts a scan, so a
drive that needs Administrator produces a panel with a **Restart as
Administrator** button rather than a scan that fails at the first read. The
restart carries the request - source, folder, category and whether a deep scan
was wanted - as command-line arguments, and the new window starts that scan
straight away. Arguments are wrapped in double quotes inside PowerShell's
single-quoted argument list: without that a path containing a space arrives at
the new window split in two, which is how it first behaved.

Drives that need Administrator are listed and clickable rather than greyed out;
choosing one leads to the restart. Every scan screen has a way back, and no
screen shows a progress bar for something that is not running.

Windows offers no way to read a raw volume without Administrator, so this is a
prompt, not a limitation that can be engineered away. What does work without
it: disk image files.

### 3.17 What the phone side deliberately will not do

`rc android screen/tap/swipe/key/type` show a phone's display on this computer
and send touches back, for a phone whose screen is broken. They need USB
debugging to have been switched on and this computer authorised *before* the
damage; a phone that was never set up that way cannot be reached, and nothing
here changes that. `type` types the text it is given, one string per call.
There is no loop that tries codes, and there will not be: that is the feature
that turns a repair tool into a tool for stolen phones, and Android erases some
phones after repeated wrong codes anyway.

Not implemented, and not planned: lock-screen or FRP bypass (every real method
is either a wipe, which destroys the data being recovered, or a chipset
exploit), and firmware flashing to "repair" a phone that will not boot (it
needs a download, and it writes to the device this engine promises never to
write to). For a locked phone the honest path is the manufacturer's own -
Google Find My Device, Samsung Find My Mobile, Apple recovery - followed by a
restore from a backup; `rc host-backups` finds the backups already on this
computer.

None of the screen commands has run against a real phone: there is no device
here (3.15).

### 3.15 The companion app has never run on a phone

No Android phone and no emulator system image was available on this machine, so
the companion app is built and unit-tested but has not run on real hardware.
What *is* checked: the app's bridge protocol against the desktop receiver,
through a recorded session (`protocol-golden.bin`) that the Kotlin test writes
and the Rust test replays, so both ends are held to one description.

Unverified on hardware: the MediaStore trash query, the system restore dialog,
the cache scan, and the USB connection itself. The app needs Android 11 for the
trash; below that it says so and offers only caches and leftovers.

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
