//! Errors for raw device access.
//!
//! `NeedsElevation` and `Encrypted` exist as distinct variants on purpose: the
//! CLI is required to tell the operator exactly why a device cannot be read
//! rather than reporting a generic I/O failure (SPEC.md section 1.1).

use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, DeviceError>;

#[derive(Debug, thiserror::Error)]
pub enum DeviceError {
    #[error("device not found: {0}")]
    NotFound(PathBuf),

    #[error("permission denied opening {path}: {detail}")]
    PermissionDenied { path: PathBuf, detail: String },

    /// Raised instead of a bare permission error when we can tell that the
    /// operation would succeed with administrative rights.
    #[error("{path} requires elevation ({hint})")]
    NeedsElevation { path: PathBuf, hint: &'static str },

    /// The target is a locked/encrypted container we cannot see through.
    /// Reported honestly rather than returning ciphertext as if it were data.
    #[error("{path} is an encrypted container that is not unlocked: {detail}")]
    Encrypted { path: PathBuf, detail: String },

    #[error("read failed at LBA {lba} on {path}: {source}")]
    Read {
        path: PathBuf,
        lba: u64,
        #[source]
        source: std::io::Error,
    },

    #[error("read past end of device: LBA {lba} + {sectors} sectors exceeds {total} sectors")]
    OutOfRange { lba: u64, sectors: u64, total: u64 },

    #[error("buffer of {len} bytes is not a whole multiple of the {sector_size}-byte sector size")]
    Misaligned { len: usize, sector_size: u32 },

    #[error("invalid sector size {0} (must be a power of two between 512 and 32768)")]
    InvalidSectorSize(u32),

    #[error("{path}: {detail}")]
    Unsupported { path: PathBuf, detail: String },

    #[error("enumerating devices failed: {0}")]
    Enumerate(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl DeviceError {
    /// True when the failure is a pure access-rights problem, which the CLI
    /// surfaces as actionable advice rather than an error dump.
    pub fn is_access_problem(&self) -> bool {
        matches!(
            self,
            DeviceError::PermissionDenied { .. }
                | DeviceError::NeedsElevation { .. }
                | DeviceError::Encrypted { .. }
        )
    }
}
