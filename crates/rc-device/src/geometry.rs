//! Device identity and geometry.

use crate::error::{DeviceError, Result};
use std::fmt;
use std::path::PathBuf;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// A logical block address, in sectors.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Lba(pub u64);

impl Lba {
    pub const ZERO: Lba = Lba(0);

    #[inline]
    pub fn byte_offset(self, sector_size: SectorSize) -> u64 {
        self.0 * sector_size.get() as u64
    }

    #[inline]
    pub fn saturating_add(self, sectors: u64) -> Lba {
        Lba(self.0.saturating_add(sectors))
    }
}

impl fmt::Display for Lba {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A validated sector size: a power of two between 512 and 32768 bytes.
///
/// Constructing this through [`SectorSize::new`] is the only way to get one, so
/// downstream alignment arithmetic can rely on the power-of-two invariant.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct SectorSize(u32);

impl SectorSize {
    pub const S512: SectorSize = SectorSize(512);
    pub const S4096: SectorSize = SectorSize(4096);

    pub fn new(bytes: u32) -> Result<Self> {
        if bytes.is_power_of_two() && (512..=32768).contains(&bytes) {
            Ok(SectorSize(bytes))
        } else {
            Err(DeviceError::InvalidSectorSize(bytes))
        }
    }

    #[inline]
    pub const fn get(self) -> u32 {
        self.0
    }

    #[inline]
    pub const fn as_usize(self) -> usize {
        self.0 as usize
    }

    /// Round `offset` down to the containing sector boundary.
    #[inline]
    pub const fn align_down(self, offset: u64) -> u64 {
        offset & !((self.0 as u64) - 1)
    }

    /// Round `offset` up to the next sector boundary.
    #[inline]
    pub const fn align_up(self, offset: u64) -> u64 {
        let m = self.0 as u64 - 1;
        (offset + m) & !m
    }
}

impl Default for SectorSize {
    fn default() -> Self {
        SectorSize::S512
    }
}

impl fmt::Display for SectorSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Whether the device reports TRIM/UNMAP support.
///
/// This feeds the Milestone 5 scoring rule: a zero-filled candidate region on a
/// TRIM-capable device is classified RED with reason `trimmed` rather than being
/// reported as recoverable (SPEC.md section 5.7).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "lowercase"))]
pub enum TrimSupport {
    Yes,
    No,
    #[default]
    Unknown,
}

impl fmt::Display for TrimSupport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            TrimSupport::Yes => "yes",
            TrimSupport::No => "no",
            TrimSupport::Unknown => "unknown",
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum DeviceKind {
    /// A whole physical disk (`\\.\PhysicalDrive0`, `/dev/sda`, `/dev/disk0`).
    PhysicalDisk,
    /// A single volume/partition.
    Volume,
    /// A `.dd` / `.raw` / `.img` file treated transparently as a device.
    ImageFile,
}

impl fmt::Display for DeviceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            DeviceKind::PhysicalDisk => "disk",
            DeviceKind::Volume => "volume",
            DeviceKind::ImageFile => "image",
        })
    }
}

/// Stable identity used to decide whether two paths name the same device.
///
/// This backs the safety invariant in SPEC.md section 4.1/4.2: an output sink
/// must refuse any destination that resolves to a device currently registered
/// as a scan source. Comparing canonical paths alone is not enough, because the
/// same disk is reachable as `\\.\PhysicalDrive1`, a volume GUID, and a drive
/// letter, so physical devices are keyed on serial plus size where available.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum DeviceId {
    /// Keyed on hardware identity; survives being reached by a different path.
    Hardware { serial: String, total_bytes: u64 },
    /// Fallback when no serial is exposed: the OS-canonical device path.
    Path(String),
    /// An image file, keyed on its canonical filesystem path.
    File(String),
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeviceId::Hardware {
                serial,
                total_bytes,
            } => {
                write!(f, "hw:{serial}:{total_bytes}")
            }
            DeviceId::Path(p) => write!(f, "path:{p}"),
            DeviceId::File(p) => write!(f, "file:{p}"),
        }
    }
}

/// Everything we can learn about a device without reading its contents.
///
/// Enumeration populates this without ever issuing a data read, which is why
/// `rc-cli devices` works without elevation even though reading does not.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct DeviceInfo {
    pub id: DeviceId,
    pub path: PathBuf,
    pub kind: DeviceKind,
    pub model: Option<String>,
    pub serial: Option<String>,
    pub sector_size: SectorSize,
    /// Physical sector size when it differs from the logical one (4Kn / 512e).
    pub physical_sector_size: Option<SectorSize>,
    pub total_sectors: u64,
    pub rotational: Option<bool>,
    pub trim: TrimSupport,
    pub removable: Option<bool>,
    /// Whether a read handle could actually be obtained. False here is not an
    /// error during enumeration; it drives the CLI's honest status column.
    pub readable: bool,
    /// Human-readable reason when `readable` is false, e.g. "requires
    /// Administrator" or "BitLocker-locked volume".
    pub access_note: Option<String>,
}

impl DeviceInfo {
    pub fn total_bytes(&self) -> u64 {
        self.total_sectors * self.sector_size.get() as u64
    }
}
