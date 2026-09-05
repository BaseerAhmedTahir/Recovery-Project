# SAFETY

How the invariants in SPEC.md section 4 are actually enforced, and where to
look if you want to check the claim rather than trust it.

The premise: **a recovery tool that damages the source is worse than no tool.**

---

## Invariant 1 - `rc-device` exposes only a read path

> "`rc-device` exposes only `ReadOnlyDevice`. There is no public write path."

Enforced in four independent layers, so no single mistake defeats it.

### Layer 1: the type system

`ReadOnlyDevice` (`crates/rc-device/src/readonly.rs`) has a **private sealed
supertrait**:

```rust
pub(crate) mod sealed { pub trait Sealed {} }
pub trait ReadOnlyDevice: sealed::Sealed + Send + Sync { ... }
```

`sealed::Sealed` is `pub(crate)`, so no crate outside `rc-device` can name it,
and therefore none can implement `ReadOnlyDevice`. A downstream crate cannot
introduce a "device" whose methods secretly write.

The trait has no `&mut self` method and no method that accepts bytes to write.
The only mutable buffer anywhere in the API is the caller's own destination for
bytes being read.

### Layer 2: the operating system

Every backend opens its handle read-only:

| Backend | Open call |
|---|---|
| `backend/file.rs` | `File::open` (read-only) |
| `backend/windows.rs` | `CreateFileW(..., GENERIC_READ, ...)` |
| `backend/linux.rs` | `O_RDONLY \| O_DIRECT` |
| `backend/macos.rs` | `O_RDONLY` on `/dev/rdiskN` |

Even a logic error inside this crate cannot produce a write: the kernel would
reject it on a read-only handle. Raw handles and file descriptors are never
exposed publicly, so no caller can escalate one.

### Layer 3: cross-crate registration

Opening a device registers it in `rc-device::registry` behind an RAII guard, so
the entry disappears when the device closes even if the caller panics. Before
`rc-image::OutputSink` creates any file it consults that registry
(`crates/rc-image/src/sink.rs`).

### Layer 4: tests

`crates/rc-device/tests/immutability.rs` hashes an image, drives the **entire**
public API against it (including every error path), and re-hashes. See below.

---

## Invariant 2 - destination validation

> "Before restoring files, resolve the destination to its physical device and
> hard-fail if it is the same device being scanned."

`rc-image::sink::check_destination` runs two checks before any file is created:

1. **Direct hit.** If the destination canonicalises to an image file currently
   open as a scan source, refuse. This catches the common
   `rc image disk.img disk.img` mistake.

2. **Physical device.** `rc-image::resolve` maps the destination to its backing
   device, per platform:

   | Platform | Method |
   |---|---|
   | Windows | `GetVolumePathNameW` → `GetVolumeNameForVolumeMountPointW` → `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS` |
   | Linux | longest-prefix match in `/proc/self/mountinfo`, then partition → parent disk via `/sys/block` |
   | macOS | `statfs().f_mntfromname`, then `/dev/disk3s5` → `/dev/disk3` |

   Every registered scan source is compared against that, by hardware serial
   where available and by device path otherwise. A striped or spanned volume
   reports several disks and **any** overlap is refused.

Refusal is `ImageError::WouldWriteToSource`. There is no flag that bypasses it.

When the backing device genuinely cannot be determined (network share, unusual
FUSE mount), the result is `Backing::Unknown`: the sink warns by default, or
refuses with `--strict-destination`.

---

## Invariant 3 - the immutability test

> "Integration test: run a full scan against a fixture image, then compare the
> image's SHA-256 before and after. Must be identical. This test runs in CI on
> every commit."

`crates/rc-device/tests/immutability.rs`:

| Test | What it pins down |
|---|---|
| `scanning_never_modifies_the_source` | Hash → exercise the whole public API → re-hash, on a scratch image that always exists so CI can never silently skip |
| `generated_fixtures_are_not_modified_by_scanning` | The same, across all eight generated fixtures |
| `sequential_sweep_does_not_modify_the_source` | A full front-to-back sweep, the access pattern a real scan uses |
| `reads_return_correct_content` | That reads return the *right* bytes. An immutability test alone would pass if reads returned nothing at all |
| `open_devices_register_as_scan_sources` | Registration/unregistration, which invariant 2 depends on |
| `honours_explicit_sector_size` | 4Kn handling, and that it too leaves the image unchanged |
| `readonly_device_has_no_write_surface` | That mutating a buffer filled by a read cannot propagate back |

Error paths are exercised inside the hash-compare window as well, so a backend
that touched the device while failing would be caught.

---

## Invariant 4 - no network

> "Add a CI check that greps the dependency tree for `reqwest`, `hyper`,
> `tokio-net`, `curl`, sockets, etc."

`cargo xtask audit-deps` parses `cargo metadata --all-features` and fails if any
of ~40 networking crates appears as a non-dev dependency anywhere in the tree:
HTTP clients and servers, async runtimes with networking, raw socket layers,
websocket and RPC stacks, DNS resolvers, TLS stacks, and telemetry crates.

Dev-dependencies are exempt (they never ship). The permitted exception from
SPEC.md section 4.4 — the loopback/USB companion link inside `rc-mobile`
behind a feature flag — is expressed as `ALLOWED_NETWORK_OWNERS`, currently
empty because `rc-mobile` does not exist yet.

This runs as part of `cargo xtask ci` and in the GitHub Actions workflow.

---

## Invariant 5 - never auto-mount

> "Never call `fsck`, never replay a journal onto the source, never let the OS
> auto-repair a volume we touched."

Nothing in the codebase invokes `mount`, `fsck`, or any filesystem repair tool.
`rc-device` opens block devices directly and parses them in userspace.

The one place mounting happens is `testdata/build_fixtures.sh`, which
*constructs* fixtures and never touches a device under examination. When
Milestone 6 adds ext4 JBD2 journal replay, that replay must happen in memory
against a parsed copy, never written back.

---

## What is NOT yet proven

Read `docs/LIMITATIONS.md` section 1 before relying on any of this against real
hardware. In particular the Windows raw-device read path has never been
executed, and until `rc smoke` has been run on a real device the unbuffered
read path is unverified code.
