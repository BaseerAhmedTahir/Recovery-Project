//! `rc-device` - read-only raw block device access for RECOVERY-CORE.
//!
//! This crate is the *only* way the engine reaches a scan source, and it
//! deliberately has no write path at all. SPEC.md section 4 calls that out as
//! the invariant that matters most: a recovery tool that damages the source is
//! worse than no tool.
//!
//! How that is enforced, in layers:
//!
//! 1. **Type system.** [`ReadOnlyDevice`] is sealed, so no crate outside this
//!    one can implement it. There is no `WriteableDevice` counterpart and no
//!    method that accepts bytes to write.
//! 2. **Operating system.** Every backend opens its handle with read-only
//!    access rights (`GENERIC_READ`, `O_RDONLY`). Even a logic error here
//!    cannot produce a write, because the kernel would reject it. Raw handles
//!    and file descriptors are never exposed publicly.
//! 3. **Cross-crate.** Opening a device registers it in [`registry`]. Before
//!    `rc-image::OutputSink` creates any file, it resolves the destination and
//!    refuses if it lands on a registered scan source.
//! 4. **Tests.** `tests/immutability.rs` hashes a fixture image, drives the
//!    entire public API against it, and asserts the hash is unchanged. It runs
//!    in CI on every commit.
//!
//! ```no_run
//! use rc_device::{open, ReadOnlyDevice, Lba};
//!
//! let dev = open(std::path::Path::new("disk.img"), None)?;
//! let mut buf = vec![0u8; dev.sector_size().as_usize()];
//! dev.read_at(Lba(0), &mut buf)?;
//! # Ok::<(), rc_device::DeviceError>(())
//! ```

pub mod align;
pub mod backend;
pub mod error;
pub mod geometry;
pub mod readonly;
pub mod registry;

/// Fault-injecting device wrapper for tests. Behind a feature flag because
/// [`ReadOnlyDevice`] is sealed, so a test double cannot live in a consuming
/// crate's tests. It is still read-only: it only makes reads fail.
#[cfg(feature = "test-util")]
pub mod testutil;

pub use align::AlignedBuf;
pub use error::{DeviceError, Result};
pub use geometry::{DeviceId, DeviceInfo, DeviceKind, Lba, SectorSize, TrimSupport};
pub use readonly::ReadOnlyDevice;
pub use registry::{
    is_registered_source, registered_sources, registered_sources_detailed, ScanSource,
    ScanSourceGuard,
};

use std::path::Path;

/// Open a device or image file read-only.
///
/// Accepts `\\.\PhysicalDrive0`, `/dev/sda`, `/dev/rdisk2`, and `.dd`/`.raw`/
/// `.img` files transparently (SPEC.md section 5.1).
pub fn open(path: &Path, sector_size: Option<SectorSize>) -> Result<Box<dyn ReadOnlyDevice>> {
    backend::open(path, sector_size)
}

/// List the machine's block devices.
///
/// This performs metadata queries only and never reads device contents, so it
/// succeeds without elevation on all three platforms. Devices whose *data*
/// cannot be read without elevation are still listed, with
/// [`DeviceInfo::readable`] false and a human-readable
/// [`DeviceInfo::access_note`], so the CLI can say why instead of silently
/// omitting them.
pub fn enumerate_devices() -> Result<Vec<DeviceInfo>> {
    backend::enumerate()
}

/// True when this process can open raw devices for reading (Administrator on
/// Windows, root on Unix).
pub fn is_elevated() -> bool {
    backend::is_elevated()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_extensions_are_recognised() {
        for p in ["a.img", "b.dd", "c.raw", "d.IMG"] {
            assert!(
                backend::looks_like_image(Path::new(p)),
                "{p} should be treated as an image"
            );
        }
        assert!(!backend::looks_like_image(Path::new(
            "\\\\.\\PhysicalDrive0"
        )));
    }

    #[test]
    fn sector_size_rejects_invalid_values() {
        for bad in [0u32, 1, 100, 513, 1000, 65536] {
            assert!(SectorSize::new(bad).is_err(), "{bad} should be rejected");
        }
        for good in [512u32, 1024, 2048, 4096, 8192] {
            assert!(SectorSize::new(good).is_ok(), "{good} should be accepted");
        }
    }

    #[test]
    fn sector_alignment_math() {
        let s = SectorSize::S512;
        assert_eq!(s.align_down(0), 0);
        assert_eq!(s.align_down(511), 0);
        assert_eq!(s.align_down(512), 512);
        assert_eq!(s.align_down(1000), 512);
        assert_eq!(s.align_up(0), 0);
        assert_eq!(s.align_up(1), 512);
        assert_eq!(s.align_up(512), 512);
        assert_eq!(s.align_up(513), 1024);
    }

    #[test]
    fn lba_byte_offsets() {
        assert_eq!(Lba(0).byte_offset(SectorSize::S512), 0);
        assert_eq!(Lba(1).byte_offset(SectorSize::S512), 512);
        assert_eq!(Lba(2048).byte_offset(SectorSize::S512), 1_048_576);
        assert_eq!(Lba(1).byte_offset(SectorSize::S4096), 4096);
    }
}
