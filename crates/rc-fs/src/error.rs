//! Errors for filesystem parsing.
//!
//! Parsers hand back specific reasons rather than a generic "corrupt", because
//! the CLI has to explain why a file could not be recovered. "The MFT record
//! was torn by an interrupted write" and "the runlist points past the end of
//! the volume" call for different responses from the operator.

pub type Result<T> = std::result::Result<T, FsError>;

#[derive(Debug, thiserror::Error)]
pub enum FsError {
    #[error("not a recognised filesystem: {detail}")]
    Unrecognised { detail: String },

    #[error("bad boot sector: {detail}")]
    BadBootSector { detail: String },

    /// The NTFS update sequence array did not validate. See ntfs::fixup.
    #[error("update sequence array check failed: {detail}")]
    Fixup { detail: String },

    #[error("attribute parse error: {detail}")]
    Attribute { detail: String },

    #[error("runlist decode error: {detail}")]
    Runlist { detail: String },

    #[error("directory entry error: {detail}")]
    DirectoryEntry { detail: String },

    /// Recognised, but recovery from it is not implemented. `detail` says what
    /// was read and what to do instead.
    #[error("{fs}: {detail}")]
    NotImplemented { fs: &'static str, detail: String },

    #[error(transparent)]
    Device(#[from] rc_device::DeviceError),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}
