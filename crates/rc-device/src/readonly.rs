//! The read-only device interface.
//!
//! SPEC.md section 4.1 requires that `rc-device` expose *only* a read path.
//! Three things enforce that here:
//!
//! 1. [`ReadOnlyDevice`] is a sealed trait. Code outside this crate cannot
//!    implement it, so no downstream crate can introduce a "device" whose
//!    methods secretly write.
//! 2. No method takes `&mut self` and none accepts data to write. The only
//!    mutable buffer in the API is the caller's own destination for read bytes.
//! 3. Every backend opens its handle with read-only OS access rights
//!    (`GENERIC_READ`, `O_RDONLY`), so even a bug here cannot produce a write:
//!    the kernel refuses it. The raw handle is deliberately never exposed.

use crate::align::AlignedBuf;
use crate::error::{DeviceError, Result};
use crate::geometry::{DeviceInfo, Lba, SectorSize, TrimSupport};

pub(crate) mod sealed {
    /// Private supertrait: only this crate can name it, so only this crate can
    /// implement [`super::ReadOnlyDevice`].
    pub trait Sealed {}
}

/// Read-only access to a block device or image file.
///
/// There is intentionally no counterpart trait for writing. Anything that needs
/// to produce bytes goes through `rc-image::OutputSink`, which refuses
/// destinations that resolve to a registered scan source.
pub trait ReadOnlyDevice: sealed::Sealed + Send + Sync {
    /// Static description of the device. Never triggers I/O.
    fn info(&self) -> &DeviceInfo;

    /// Read whole sectors starting at `lba` into `buf`.
    ///
    /// `buf.len()` must be a non-zero multiple of [`Self::sector_size`].
    /// Returns the number of bytes read, which is short only at end of device.
    ///
    /// Implementations must not retry indefinitely on a bad sector; the
    /// ddrescue-style retry policy lives in `rc-image`, which needs to see
    /// individual failures to build its error map.
    fn read_at(&self, lba: Lba, buf: &mut [u8]) -> Result<usize>;

    // --- provided conveniences -------------------------------------------

    fn sector_size(&self) -> SectorSize {
        self.info().sector_size
    }

    fn total_sectors(&self) -> u64 {
        self.info().total_sectors
    }

    fn total_bytes(&self) -> u64 {
        self.info().total_bytes()
    }

    fn serial(&self) -> Option<&str> {
        self.info().serial.as_deref()
    }

    fn is_rotational(&self) -> Option<bool> {
        self.info().rotational
    }

    fn trim_supported(&self) -> TrimSupport {
        self.info().trim
    }

    /// Read exactly `buf.len()` bytes of sectors, erroring on a short read.
    fn read_exact_at(&self, lba: Lba, buf: &mut [u8]) -> Result<()> {
        let got = self.read_at(lba, buf)?;
        if got == buf.len() {
            Ok(())
        } else {
            Err(DeviceError::OutOfRange {
                lba: lba.0,
                sectors: (buf.len() / self.sector_size().as_usize()) as u64,
                total: self.total_sectors(),
            })
        }
    }

    /// Read an arbitrary byte range, without alignment constraints.
    ///
    /// Filesystem parsers and carvers work in byte offsets that rarely land on
    /// sector boundaries, while unbuffered reads require alignment. This reads
    /// the covering aligned window into a bounce buffer and copies out the
    /// requested slice, so callers never have to think about it.
    fn read_bytes_at(&self, offset: u64, out: &mut [u8]) -> Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let ss = self.sector_size();
        let ssz = ss.get() as u64;
        let total = self.total_bytes();
        if offset >= total {
            return Ok(0);
        }

        let want = (out.len() as u64).min(total - offset);
        let start = ss.align_down(offset);
        let end = ss.align_up(offset + want);
        let span = (end - start) as usize;

        let mut bounce = AlignedBuf::new(span, ss.as_usize());
        let got = self.read_at(Lba(start / ssz), &mut bounce[..span])?;

        let skip = (offset - start) as usize;
        if got <= skip {
            return Ok(0);
        }
        let avail = (got - skip).min(want as usize);
        out[..avail].copy_from_slice(&bounce[skip..skip + avail]);
        Ok(avail)
    }

    /// Validate that a read of `sectors` sectors at `lba` stays in bounds.
    fn check_range(&self, lba: Lba, sectors: u64) -> Result<()> {
        let total = self.total_sectors();
        if lba.0.saturating_add(sectors) > total {
            Err(DeviceError::OutOfRange {
                lba: lba.0,
                sectors,
                total,
            })
        } else {
            Ok(())
        }
    }
}

/// Shared validation used by every backend's `read_at`.
pub(crate) fn validate_read(info: &DeviceInfo, lba: Lba, buf: &[u8]) -> Result<usize> {
    let ss = info.sector_size;
    if buf.is_empty() || buf.len() % ss.as_usize() != 0 {
        return Err(DeviceError::Misaligned {
            len: buf.len(),
            sector_size: ss.get(),
        });
    }
    let sectors = (buf.len() / ss.as_usize()) as u64;
    if lba.0 >= info.total_sectors {
        return Err(DeviceError::OutOfRange {
            lba: lba.0,
            sectors,
            total: info.total_sectors,
        });
    }
    // Clamp rather than fail at the tail: the last read of a scan routinely
    // asks for a full block that runs past the end of the device.
    let avail = (info.total_sectors - lba.0).min(sectors);
    Ok((avail * ss.get() as u64) as usize)
}
