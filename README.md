# RECOVERY-CORE

An offline data-recovery suite for your own drives, cards and phones: a Rust
engine, a command line, a Windows desktop app, and an Android companion app.
Nothing it does needs an internet connection, an account, or a licence key, and
it never writes to the disk it is reading.

> For devices you own or are authorised to examine. There is nothing here for
> getting into someone else's phone: everything requires the device unlocked
> and USB debugging or backup trust enabled by the person holding it.

## What it recovers, honestly

| Source | What you can expect |
|---|---|
| SD cards, USB sticks, camera cards | **Very good.** Deleted files by name where the filesystem still remembers, and by signature where it does not. |
| Mechanical hard disks | **Very good**, until something overwrites the space. |
| SSDs and NVMe (internal or USB) | **Usually nothing.** The drive erases deleted blocks itself (TRIM), often within seconds. Recent deletions only. |
| Android internal storage | **Trash and caches only.** Every file is encrypted with its own key, which is destroyed on delete. No app or tool recovers past that — treat claims otherwise as false. |
| Android SD card | Take the card out and scan it here: full recovery. |
| iPhone | Through an unencrypted backup: "Recently Deleted" photos and videos, and their thumbnails. |

Filesystems: NTFS, FAT12/16/32, exFAT and ext2/3/4 (deleted names, folder tree
and content). APFS and HFS+ are recognised and described, but deleted-file
recovery from them is not implemented — carving still works there.

Every recovered file is rated **GREEN / YELLOW / RED** with the reasons shown,
so you know which ones are trustworthy before you look at them.

## Getting it

Build it once on a machine with internet, then copy the folder anywhere:

```powershell
powershell -ExecutionPolicy Bypass -File packaging\build-windows.ps1   # rc.exe + rc-gui.exe
powershell -ExecutionPolicy Bypass -File packaging\build-android.ps1   # the phone app
```

Details, and what is deliberately not bundled, in [packaging/README.md](packaging/README.md).

## Using it

**The desktop app** (`rc-gui.exe`): pick a drive or image, *Find deleted files*
or *Carve by signature*, then filter the results, look at previews and hex, tick
what you want and restore it to a folder **on a different disk**.

**The command line** is the same engine and can do everything:

```bash
rc devices                                     # what this machine can see
rc list-deleted \\.\PhysicalDrive2             # deleted files, with the folder tree
rc score  \\.\PhysicalDrive2                   # ...with ratings and the reasons
rc restore \\.\PhysicalDrive2 --out D:\out     # write them out, with hashes
rc carve  \\.\PhysicalDrive2 --index D:\c.idx  # by signature; resumable
rc extract \\.\PhysicalDrive2 --index D:\c.idx --out D:\out --reassemble
rc image  \\.\PhysicalDrive2 --output D:\disk.img   # failing drive: image it first
rc sqlite messages.db                          # deleted rows from a database
rc android checklist                           # phone over USB
rc ios backups                                 # iPhone backups on this PC
rc host-backups                                # phone backups and sync caches here
```

Reading a whole physical disk needs Administrator. Image files do not.

**If a drive is failing** (clicking, read errors, disappearing): image it first
with `rc image`, then recover from the image. Every extra read of a dying disk
is a read you may not get again.

## Safety

- Devices are opened read-only. There is no code path that writes to them.
- Recovered files go through one writer that refuses any destination on the
  disk being read.
- A test scans a disk image and compares its SHA-256 before and after; it runs
  on every build.
- No network. `cargo xtask audit` fails the build if a networking library
  appears anywhere, if a socket appears outside the USB bridge to your own
  phone, or if the app's web view is allowed to load anything from outside.

## Repository

| Path | What it is |
|---|---|
| `crates/` | The engine: device access, imaging, partitions, filesystems, carving, reassembly, scoring, sessions, previews, SQLite recovery, phones, restore, and the CLI. |
| `gui/` | The desktop app (Tauri). Its own workspace. |
| `companion-android/` | The phone app (Kotlin/Compose). |
| `testdata/` | Fixture builders: real filesystems made by `mkfs`, media from ImageMagick and ffmpeg, with recorded ground truth. |
| `docs/LIMITATIONS.md` | **What does not work, measured.** Read this before trusting a result. |
| `docs/PROGRESS.md` | What each milestone actually achieved, with numbers. |
| `docs/BRIDGE.md` | The USB protocol between the phone app and the desktop. |

## State

Milestones 1–9 of [SPEC.md](SPEC.md) are complete on Windows, and the
Windows folder, installer and Android APK all build from source here. One thing
cannot be verified on the machine this was built on and is marked as such
throughout: anything that needs a real phone, since there is no device and no
emulator image available. The macOS and Linux device backends compile but have
not been run.
