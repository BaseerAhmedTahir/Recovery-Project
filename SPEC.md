# SPEC.md — Project RECOVERY-CORE

> Drop this file at the root of an empty git repo, open your editor in that
> directory, and it will be loaded automatically as project context every
> session. Then paste the "Session 1 kickoff" prompt at the bottom.

---

## 0. Mission

Build a self-contained, offline, cross-platform data recovery suite for
**personal use on hardware I own**. It must replace commercial tools
(Recuva, Disk Drill, EaseUS, R-Studio, Dr.Fone) for my own drives, SD cards,
USB sticks, and my own phones.

Three deliverables:

1. **`recovery-core`** — a native engine + CLI for laptop/desktop drive recovery.
2. **`recovery-mobile-bridge`** — desktop-side non-root extraction from my
   Android/iOS devices over USB, plus carving of host-stored backups.
3. **`recovery-companion`** — an Android app (and a limited iOS agent) that runs
   on the phone itself and streams candidate files to the desktop engine.

A GUI wraps 1 and 2. The CLI is the source of truth; the GUI is a thin client
over the same library.

**Legal boundary that is not negotiable:** this tool is for devices I own or
have explicit authorization to examine. Do not add any feature whose only
purpose is covert access to someone else's device — no lockscreen bypass, no
exploit chains, no silent/background collection, no hiding the app from the
device owner. Everything must require the device to be unlocked and
developer/trust mode explicitly enabled by the person holding it.

---

## 1. Reality constraints — read before designing anything

Do not write code that pretends these limits don't exist. Where a limit exists,
the UI must tell me honestly instead of showing a fake progress bar.

### 1.1 What actually recovers well

| Medium | Realistic outcome |
|---|---|
| SD cards, USB flash, camera cards | **Excellent.** Usually no TRIM. Signature carving works well. |
| Mechanical HDD | **Excellent.** Data persists until overwritten. |
| External USB SSD / internal SSD **with TRIM** | **Poor to impossible.** After TRIM the controller returns zeros. Recovery only works for very recent deletions or TRIM-disabled volumes. |
| Windows internal NVMe, BitLocker on | Needs the volume unlocked; carve the decrypted volume, not the raw device. |
| macOS Apple Silicon internal SSD, FileVault on | **Effectively impossible.** Sealed system volume + hardware key. Say so; don't try. |
| Non-rooted Android/iOS internal NAND | **Block carving is impossible** (see 1.2). Logical extraction only. |

### 1.2 Mobile: why block-level carving is off the table

Android 10+ and iOS 14+ use hardware-backed file-based encryption. Deleting a
file purges its per-file key from the secure keystore (crypto-shredding), and
the flash controller TRIMs the blocks. Even with root you would recover
ciphertext with no key. Without root you cannot read the block device at all.

So the mobile side is a **logical forensic extraction and cache-carving**
problem, not a carving problem. The things that genuinely work:

- **Android trash retention (Android 11+):** `MediaStore` rows with
  `is_trashed = 1`, and on-disk files renamed to `.trashed-<expiry>-<name>`.
  These survive ~30 days. This is the single highest-yield source.
- **Thumbnails and derivatives:** `/sdcard/DCIM/.thumbnails/`, app cache dirs,
  WhatsApp/Telegram media caches, `Android/media/*`. Often the only surviving
  copy of a deleted photo, at reduced resolution.
- **SQLite freelist / WAL / journal carving:** deleted rows frequently remain in
  freelist pages and `-wal` files of app databases you can legitimately pull
  (your own exported DBs, backup contents). Recover SMS, call logs, chats.
- **iOS backups:** trigger a local backup over `libimobiledevice`, then parse
  `Manifest.db` and `Photos.sqlite`. "Recently Deleted" assets persist ~30 days
  and often longer in derivative caches.
- **Host-side backups already on my laptop:** `MobileSync/Backup/`, Samsung
  Smart Switch, Google Drive / OneDrive / iCloud local sync caches
  (DriveFS content-cache stores). These are often the richest source and
  require no phone at all.
- **`adb backup`:** deprecated and a no-op on Android 12+. Implement it but
  detect the API level and fall back gracefully. Do not build the product
  around it.

### 1.3 Non-goals

Skip these permanently: cloud sync, telemetry, accounts, licensing, auto-update
servers, any outbound network call, chip-off/JTAG, bootloader exploits,
proprietary SDKs with subscription keys.

---

## 2. Technology decisions (already made — don't re-litigate)

- **Core engine: Rust (stable, 2021 edition).** Memory safety matters when you
  are parsing hostile on-disk structures from a corrupt drive. No GC pauses.
  Good cross-compilation.
- **Workspace layout, not one giant crate.**
- **GUI: Tauri v2 + a virtualized table (TanStack Virtual).** Ships the same
  Rust core; handles 10M rows via windowed rendering. If Tauri fights us on the
  hex viewer, fall back to `egui` — but only after the CLI is complete.
- **Mobile companion: Kotlin + Jetpack Compose.** iOS agent is Swift and is
  deliberately minimal (see §6.3).
- **No unsafe C dependencies unless required.** Preferred crates:
  `rayon`, `memmap2`, `rusqlite` (bundled), `sha2`, `md-5`, `serde`,
  `thiserror`, `tracing`, `clap`, `gpt`, `mbrman`, `crc`, `zune-jpeg`,
  `image`, `indicatif`. Shell out to `ffmpeg` for video preview rather than
  linking it.

---

## 3. Repository layout

```
recovery-core/
├── SPEC.md                  ← this file
├── Cargo.toml                 ← workspace
├── crates/
│   ├── rc-device/             ← raw block device access + READ-ONLY harness
│   ├── rc-image/              ← ddrescue-style imaging, hashing, sparse maps
│   ├── rc-partition/          ← MBR/GPT parsing, lost-partition rebuild
│   ├── rc-fs/                 ← filesystem parsers
│   │   ├── ntfs/  fat/  exfat/  ext4/  apfs/  hfsplus/
│   ├── rc-carve/              ← signature DB, SIMD scanner, validators
│   ├── rc-bifrag/             ← fragment reassembly heuristics
│   ├── rc-score/              ← Green/Yellow/Red confidence engine
│   ├── rc-index/              ← disk-backed candidate index (SQLite)
│   ├── rc-session/            ← .recstate checkpoint save/resume
│   ├── rc-preview/            ← in-memory decode + thumbnailing
│   ├── rc-mobile/             ← ADB client, libimobiledevice bridge, backup parsers
│   ├── rc-sqlite-carve/       ← freelist/WAL row recovery
│   ├── rc-cli/                ← the primary UX; everything is reachable here
│   └── rc-gui/                ← Tauri app (added last)
├── companion-android/         ← Kotlin app
├── companion-ios/             ← Swift agent
├── testdata/
│   ├── build_fixtures.sh      ← generates synthetic disk images with ground truth
│   └── fixtures/              ← .img files + expected-results JSON (gitignored if large)
└── docs/
```

---

## 4. Safety invariants — enforce these in the type system

These are the requirements I care about most. A recovery tool that damages the
source is worse than no tool.

1. **`rc-device` exposes only `ReadOnlyDevice`.** There is no public write path.
   Opening uses `GENERIC_READ` / `O_RDONLY` only. Any code needing to write goes
   through `rc-image::OutputSink`, which *refuses* to open a path that resolves
   to any block device currently registered as a scan source.
2. **Destination validation.** Before restoring files, resolve the destination
   to its physical device and hard-fail if it is the same device being scanned.
   Include the mount-point → device resolution for all three OSes.
3. **A test asserts non-writability.** Integration test: run a full scan against
   a fixture image, then compare the image's SHA-256 before and after. Must be
   identical. This test runs in CI on every commit.
4. **No network.** Add a CI check that greps the dependency tree for
   `reqwest`, `hyper`, `tokio-net`, `curl`, sockets, etc. The only permitted
   socket is the localhost/USB-tether link to the companion app, isolated in
   `rc-mobile` behind a feature flag, bound to loopback or the ADB reverse
   tunnel only.
5. **Never auto-mount.** Never call `fsck`, never replay a journal onto the
   source, never let the OS auto-repair a volume we touched.

---

## 5. Component specifications

### 5.1 `rc-device` — raw block I/O

- **Windows:** `CreateFileW` on `\\.\PhysicalDriveN` and `\\.\Volume{GUID}`,
  `GENERIC_READ`, `FILE_SHARE_READ | FILE_SHARE_WRITE`,
  `FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH`. Requires elevation.
  Honor sector-size alignment for unbuffered reads (`IOCTL_DISK_GET_DRIVE_GEOMETRY_EX`).
  Optionally dismount the volume with `FSCTL_LOCK_VOLUME` to stop cache interference.
- **macOS:** `/dev/rdiskN` (raw character device, bypasses the UBC). Needs Full
  Disk Access granted to the terminal/app. Detect and clearly report when the
  target is an encrypted APFS container we cannot read.
- **Linux:** `open(O_RDONLY | O_DIRECT)` on `/dev/sdX`, `/dev/nvmeXn1`. Fall back
  to buffered + `posix_fadvise(POSIX_FADV_DONTNEED)` if `O_DIRECT` alignment
  fails.
- Uniform API: `enumerate_devices()`, `read_at(lba, buf)`, `sector_size()`,
  `total_sectors()`, `serial()`, `is_rotational()`, `trim_supported()`.
- Also accept `.dd` / `.raw` / `.img` files as devices, transparently.

### 5.2 `rc-image` — forensic imaging

- Bit-stream clone to `.raw`/`.dd`, plus a sidecar `.map` file.
- ddrescue-style strategy: fast pass with large blocks skipping errors,
  then trimming, then scraping bad regions in both directions with shrinking
  block sizes. Configurable retry limit and per-read timeout.
- Streaming SHA-256 + MD5 computed during the copy. Write a `.hash` manifest.
- Sparse output support; resumable from the `.map` file.
- Report read-error map to the UI so I can see which regions are unreadable.

### 5.3 `rc-partition`

- MBR + GPT parsing including protective MBR, backup GPT recovery.
- **Lost partition rebuild:** scan the whole device for filesystem
  superblock/boot-sector signatures (NTFS `$Boot` at partition start, FAT BPB,
  EXT4 superblock at +1024, APFS container superblock `NXSB`, HFS+ `H+`/`HX`)
  and reconstruct a plausible partition table in memory. Never write it to disk.

### 5.4 `rc-fs` — parsers (deleted-entry focus)

Each parser implements a common trait returning `DeletedEntry { name, path,
size, timestamps, data_runs, confidence_inputs }`. Active/allocated files are
enumerated too, but only so we can mark their clusters as "occupied" for the
scoring engine — they are hidden from the default view.

- **NTFS:** parse `$MFT`; record flags at offset 0x16 (`0x00` = deleted file,
  `0x01` = allocated file, `0x02` = deleted dir, `0x03` = allocated dir).
  Decode `$FILE_NAME` (0x30) and `$DATA` (0x80) attributes, resident and
  non-resident. Decode runlists into extents. Cross-reference `$Bitmap` for
  cluster allocation state. Parse `$LogFile` and `$UsnJrnl` ($J stream) for
  recently deleted names even when the MFT record is reused. Rebuild the full
  original directory tree via parent reference sequence numbers.
- **FAT32:** directory entries with first byte `0xE5`; recover the lost first
  character heuristically or mark it `_`. LFN entry chains. FAT cluster chains
  are zeroed on delete for the *chain*, so reconstruct by assuming contiguity
  first, then hand off to `rc-bifrag`.
- **exFAT:** entry type `0x05` (deleted file entry, i.e. `0x85` with the
  in-use bit cleared); stream extension entry gives valid data length and the
  `NoFatChain` flag. Read the Allocation Bitmap.
- **EXT4:** inode table walk, `i_dtime != 0` indicates deletion. Note that ext4
  zeroes extent trees on unlink in most configurations — so also replay the
  **JBD2 journal** to recover pre-deletion extent trees and unlinked directory
  entries. This journal replay is where the real yield is.
- **APFS:** parse the container superblock, checkpoint descriptor area, and
  walk *older checkpoints* / object map snapshots for pre-deletion B-tree
  states. This is the hardest parser — schedule it last and treat partial
  support as acceptable.
- **HFS+:** Catalog B-tree leaf scan for unreferenced thread records; parse the
  Allocation File.

### 5.5 `rc-carve` — signature carving

- Signature database as a data file (TOML/JSON), not hardcoded, so I can extend
  it. Start with ~40 high-value formats, grow toward 350+.
  Each entry: header bytes + mask, optional footer, max size, extension,
  MIME category, and an optional validator id.
- Scanner: multi-threaded producer/consumer over a ring buffer. One reader
  thread does large sequential aligned reads; N worker threads match.
  Use `memchr`-style SIMD prefilter on the first signature byte, then verify.
  Target sustained throughput near device speed; measure it and print MB/s.
- Skip clusters known-allocated to live files when in "deleted only" mode.
- Validators (real decode, not just header match): JPEG (SOI/EOI + entropy
  scan + restart markers), PNG (chunk CRCs), PDF (`%PDF` → `%%EOF` + xref),
  ZIP/OOXML (local headers → central directory), MP4/MOV (`ftyp` → atom walk,
  `moov`/`mdat` reconciliation), SQLite (page header + freelist sanity),
  RIFF (WAV/AVI chunk sizes).

### 5.6 `rc-bifrag` — fragment reassembly

This is the feature that separates this from free tools. Implement in this
order, and be honest in commit messages about the hit rate:

1. **Contiguous first.** Always try the simple case; validate; done if it passes.
2. **Bifragment gap carving (Garfinkel).** For a header at H and a known/likely
   footer at F, search gap sizes and split points such that the concatenated
   halves pass the format validator. Bound the search with a max gap.
3. **Entropy boundary detection.** Shannon entropy per 4 KiB cluster. A sharp
   transition (e.g. compressed 7.9 bits/byte → text 4.5) marks a probable
   fragment boundary. Use to seed candidate split points instead of brute force.
4. **Format-driven pointer following.** For MP4, read `moov` atom sample tables
   and follow chunk offsets; if an offset lands outside the current run, search
   for the cluster whose content satisfies the expected sample size/type. For
   ZIP, walk the central directory backwards to locate each member's expected
   offset.
5. **Decode-until-failure (JPEG).** Decode entropy-coded data cluster by
   cluster; when a macroblock decode fails or DC coefficients go wild, try each
   unallocated candidate cluster in a bounded window as the next fragment and
   continue if decoding recovers cleanly at the next restart marker.

Every reassembled file records *how* it was assembled, and that feeds scoring.

### 5.7 `rc-score` — Green / Yellow / Red

Compute a 0–100 score from explicit, inspectable inputs — no magic constants
buried in code. Store the input vector alongside the score so the UI can
explain the rating.

- **GREEN (90–100):** metadata intact, all data runs land exclusively in
  currently-unallocated clusters, header + structure + footer validate, zero
  checksum errors, decoder produced a full image/stream.
- **YELLOW (45–89):** header intact but fragmented and reassembled
  heuristically; or missing footer but entropy-consistent; or some clusters
  partially overwritten; or the preview decodes with visible degradation.
- **RED (0–44):** downstream clusters are allocated to live files
  (overwritten); container index (ZIP central directory, MP4 `moov`) missing;
  device reports TRIM and the region reads as all zeros.

Add a rule: if the source device has TRIM enabled and the candidate region is
all zeros, mark RED with reason `trimmed` rather than pretending.

### 5.8 `rc-index` + `rc-session`

- Candidate index is **disk-backed SQLite** (WAL mode) in a scratch dir, not in
  RAM. Peak RSS must stay under ~2 GB regardless of drive size — verify this in
  a benchmark against a large fixture.
- `.recstate` checkpoint written every N seconds and on SIGINT/SIGTERM:
  current LBA, device serial + GUID + size (to refuse resuming against the
  wrong disk), processed-cluster bitmap, scanner phase, config hash, and the
  index file path.
- `resume` verifies device identity, then continues from the exact LBA.
  Test this by killing the process mid-scan and resuming.

### 5.9 `rc-preview`

- Images decoded in-memory to a bounded thumbnail; never write temp files to
  the source.
- Video/audio: pipe the carved bytes to a bundled `ffmpeg` to render the first
  ~10 s or extract a keyframe. Sandbox it: no filesystem access beyond stdin,
  hard timeout, memory cap.
- Hex viewer backend: return arbitrary offset ranges with annotation spans
  (header, footer, fragment boundaries, partition structures).

---

## 6. Mobile subsystem

### 6.1 `rc-mobile` — Android path (desktop side)

- Embed an ADB client speaking the ADB wire protocol directly over USB
  (`usb-rs`/`nusb`) *or* drive a bundled `adb` binary. Start with the bundled
  binary; go native only if it proves necessary.
- Require: device unlocked, USB debugging enabled, RSA key authorized on the
  phone. Show me a checklist UI if any is missing.
- Extraction targets, in yield order:
  1. `content query --uri content://media/external/images/media` with
     `is_trashed=1`; same for video/audio/downloads.
  2. `find /sdcard -name ".trashed-*"` and pull.
  3. `/sdcard/DCIM/.thumbnails/`, `Android/media/**/cache`, WhatsApp/Telegram
     media dirs, `.nomedia` directories.
  4. App-accessible SQLite DBs + their `-wal`/`-journal` companions → hand to
     `rc-sqlite-carve`.
  5. `adb backup` if API level < 31.
- Pull everything into a working directory on the laptop, then run the same
  carving/scoring pipeline on it.

### 6.2 `rc-mobile` — iOS path (desktop side)

- Use `libimobiledevice` (`idevice_id`, `idevicebackup2`, AFC). Requires the
  device unlocked and "Trust This Computer" accepted.
- Trigger a local backup, or parse an existing one. Handle encrypted backups
  when I supply the password (needed for the richer data set).
- Parse `Manifest.db` → map hashed filenames to real paths.
- Parse `Photos.sqlite`: `ZASSET` rows where `ZTRASHEDSTATE = 1` (Recently
  Deleted), plus `ZADDITIONALASSETATTRIBUTES` for original filenames. Pull
  full-size originals and `Derivatives/` renders.
- Extract app container DBs and unlinked attachment blobs.

### 6.3 Host-stored backups and sync caches (highest yield, no phone needed)

Auto-discover and carve:

- `~/Library/Application Support/MobileSync/Backup/` (macOS)
- `%APPDATA%\Apple Computer\MobileSync\Backup\` and the Microsoft Store
  variant under `%USERPROFILE%\Apple\MobileSync\Backup\` (Windows)
- Samsung Smart Switch backup directories
- Google Drive File Stream / DriveFS content cache
  (`~/Library/Application Support/Google/DriveFS/*/content_cache/`,
  `%LOCALAPPDATA%\Google\DriveFS\`) — chunked binary blobs; carve by signature
  and cross-reference the local metadata SQLite for original filenames.
- OneDrive local cache, iCloud Drive `.icloud` placeholders + evicted caches.

### 6.4 `companion-android`

Kotlin + Compose. Scope it honestly — an unrooted app cannot do miracles.

- Request `MANAGE_EXTERNAL_STORAGE` (with a clear in-app explanation) or work
  within Scoped Storage + `MediaStore.createTrashRequest` APIs.
- Features: list trashed MediaStore items and untrash them in one tap; sweep
  hidden/`.nomedia`/orphaned files; recover thumbnails and app caches; recover
  voice notes and messaging media caches.
- **Desktop bridge:** a loopback socket server reachable over
  `adb reverse tcp:PORT tcp:PORT` (USB) or a Wi-Fi mode requiring a pairing code
  and TLS with a pinned self-signed cert generated per session. Streams files
  and SQLite DBs to the desktop engine for heavy carving.
- No ads, no analytics, no network permission except the local bridge.

### 6.5 `companion-ios`

Deliberately minimal — sandboxing makes anything else impossible. It can
enumerate and restore "Recently Deleted" via PhotoKit, expose the app's own
File Provider, and act as the bridge endpoint. Do not overpromise in the UI.

---

## 7. Test harness — build this in Milestone 1, before the parsers

Note: you cannot verify this project against my real drives, so
**generate ground truth**. `testdata/build_fixtures.sh` must:

1. Create sparse image files (e.g. 512 MiB) and format them: NTFS, FAT32,
   exFAT, EXT4 (via `mkfs.*` on Linux, or `hdiutil`/`diskutil` on macOS).
2. Loop-mount, copy in a known corpus (JPEG, PNG, PDF, MP4, DOCX, SQLite,
   TXT), record each file's SHA-256 and path in `expected.json`.
3. Delete a defined subset. Unmount.
4. Produce deliberately hard variants: a fragmented-JPEG image (write filler
   files to force fragmentation, then delete the filler), a
   quick-formatted image, a partially-overwritten image, and a
   partition-table-wiped image.

Then integration tests assert: recovered file count, byte-exact SHA-256 match
for recoverable files, correct Green/Yellow/Red classification, and source
image hash unchanged. **A milestone is not complete until its tests pass
against these fixtures.**

---

## 8. Milestones

Work milestone by milestone. Do not start the next one until the previous one's
acceptance tests pass. Commit at each green test run.

| # | Scope | Acceptance |
|---|---|---|
| **1** | Workspace skeleton, `rc-device`, `rc-image`, test fixture generator, read-only harness + immutability test | Enumerate real drives on this OS; clone a fixture to `.raw` with bad-sector skipping and matching SHA-256; source hash unchanged |
| **2** | `rc-partition`, NTFS + FAT32 + exFAT parsers, `rc-cli list-deleted` | Recover deleted filenames, sizes, timestamps and original folder tree from fixtures; ≥95% name accuracy |
| **3** | `rc-carve` signature engine + validators, `rc-index`, throughput benchmark | Carve 20+ formats from a quick-formatted fixture; byte-exact recovery of contiguous files; report MB/s; peak RSS < 2 GB |
| **4** | `rc-bifrag` reassembly | Correctly reassemble the fragmented-JPEG and fragmented-MP4 fixtures; measurable improvement over contiguous-only baseline |
| **5** | `rc-score`, `rc-session` pause/resume, `rc-preview` | Classification matches expectations on the overwritten fixture; kill -9 mid-scan and resume to identical results; previews render without temp files |
| **6** | EXT4 (+ JBD2 journal replay), APFS/HFS+ best-effort | Recover from EXT4 fixture including journal-only recoveries |
| **7** | `rc-mobile`: ADB path, iOS backup path, host backup/sync-cache carving, `rc-sqlite-carve` | Recover trashed media from my Android over ADB; parse an iOS backup and list Recently Deleted assets; carve deleted SMS rows from a SQLite fixture |
| **8** | Tauri GUI: drive selector, virtualized results grid, filters, previews, hex viewer, progress | Grid stays responsive at 10M synthetic rows; all CLI capability reachable |
| **9** | `companion-android`, bridge, packaging, offline audit | APK installs and restores trashed media; bridge streams to desktop; dependency/network audit passes clean |

---

## 9. Working agreement

- **Ask before assuming.** If a design decision has real trade-offs, stop and
  ask me rather than picking silently.
- **Small, tested commits.** Every commit must build and pass tests.
- **Write down what doesn't work.** Keep `docs/LIMITATIONS.md` current: which
  filesystems, which devices, which scenarios genuinely fail. I would rather
  know than be surprised during a real recovery.
- **No stubs pretending to be features.** If APFS parsing is 30% done, the CLI
  says so and the GUI greys it out.
- **Benchmarks are part of the definition of done** for Milestones 3 and 4.
- Keep a running `docs/PROGRESS.md` with what's done, what's next, and any
  blocking questions for me.

---

## Session 1 kickoff prompt

Paste this in after placing `SPEC.md` in an empty repo:

> Read SPEC.md fully before writing any code.
>
> We're starting Milestone 1. Before you write anything, do three things:
>
> 1. Tell me my host OS, whether I have the toolchain (rustc, cargo, and the
>    fixture tools: mkfs.ntfs/mkfs.vfat/mkfs.exfat/mkfs.ext4 or the macOS
>    equivalents, plus ffmpeg and adb), and what's missing.
> 2. List any design decisions in Milestone 1 where you'd otherwise be
>    guessing, and ask me about them.
> 3. Propose the exact file list you'll create for Milestone 1, with a
>    one-line purpose for each.
>
> Then, once I approve, implement Milestone 1 in this order:
> the Cargo workspace skeleton → `testdata/build_fixtures.sh` and the
> integration test that asserts a fixture image's SHA-256 is unchanged after a
> scan → `rc-device` with the read-only harness → `rc-image` with ddrescue-style
> bad-sector handling and streaming hashes → `rc-cli` with `devices`, `image`,
> and `verify` subcommands.
>
> The immutability test must exist and pass before `rc-device` gains its first
> real read path. Commit after each green test run. Stop and report when
> Milestone 1's acceptance criteria are met — do not roll into Milestone 2.
