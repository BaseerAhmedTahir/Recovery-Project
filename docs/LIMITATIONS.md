# LIMITATIONS

What genuinely does not work, or is not yet verified. Kept current per SPEC.md
section 9: "I would rather know than be surprised during a real recovery."

Last updated: 2026-09-05 (end of Milestone 1).

---

## 1. Verification status of the code itself

### 1.1 The Windows sector-read path is still unverified; the rest of the backend now is

**Updated 2026-09-06.** The MSVC toolchain is installed, so the Windows backend
now compiles and most of it has been executed against real hardware.

Compiling it for the first time found three genuine bugs that review had not
caught: a wrong `ReadFile` buffer type, a borrow error in the bounce-buffer
path, and an IOCTL constant that windows-sys does not export. That is the
argument for compiling unverified code early, rather than trusting that it
looks right.

**Verified against real hardware, unelevated:**

- `CreateFileW` with zero access rights, and the metadata IOCTLs behind it:
  `IOCTL_DISK_GET_DRIVE_GEOMETRY_EX`, `IOCTL_STORAGE_QUERY_PROPERTY` for model
  and serial, the seek-penalty descriptor and the TRIM descriptor. `rc devices`
  correctly identifies this machine's NVMe as an SSD reporting TRIM.
- Elevation detection, and the honest "requires Administrator to read sector
  data" reporting that depends on it.
- Destination resolution (SPEC.md section 4.2): `GetVolumePathNameW` to a
  volume GUID to `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS` resolved `D:` to
  `\\.\PhysicalDrive0`, matching Windows' own `Get-Partition` answer.
- The `rc smoke` guards: refused without the opt-in variable, and refused the
  system drive.
- The full 196-test suite, run natively on Windows.

**Still unverified: the sector read itself.** `read_unbuffered`, and everything
`FILE_FLAG_NO_BUFFERING` implies - destination-address alignment, transfer
length alignment, and the `OVERLAPPED` offset plumbing - has never executed,
because reading raw sectors needs Administrator rights and a device that is
safe to read. Until it has run, treat that one function as untested code.

This remains a hard gate on Milestone 8 (see `docs/PROGRESS.md`).

To clear it, from an **Administrator** PowerShell with a throwaway USB stick or
SD card plugged in:

```powershell
.\target\debug\rc.exe devices          # find the stick's PhysicalDriveN
$env:RC_SMOKE_ALLOW = "1"
.\target\debug\rc.exe smoke --device \\.\PhysicalDriveN
```

It reads a handful of sectors including a deliberately 4K-unaligned offset,
checks the unaligned read agrees with the aligned read of the same bytes, and
asserts the range hashes identically on a second read. Then record the result
here.

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
  ceiling from Milestone 3 cannot be tested against 512 MiB fixtures.

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
