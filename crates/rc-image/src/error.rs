//! Errors for imaging and output.

use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, ImageError>;

#[derive(Debug, thiserror::Error)]
pub enum ImageError {
    /// The safety refusal from SPEC.md sections 4.1 and 4.2. This is not a
    /// warning and is never bypassed by a flag.
    #[error(
        "refusing to write to {destination}: {detail}\n\
         The device being scanned is {scanning}. Choose a destination on a \
         different physical device."
    )]
    WouldWriteToSource {
        destination: PathBuf,
        /// Deliberately not named `source`: thiserror treats a field with that
        /// name as the underlying error, and this is a path.
        scanning: PathBuf,
        detail: String,
    },

    #[error(
        "cannot determine which physical device backs {destination} ({reason}), \
         and strict mode is enabled"
    )]
    UnresolvableDestination {
        destination: PathBuf,
        reason: String,
    },

    #[error("{0} already exists (pass --overwrite to replace it)")]
    DestinationExists(PathBuf),

    #[error("creating {path} failed: {source}")]
    Create {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("writing to {path} at offset {offset} failed: {source}")]
    Write {
        path: PathBuf,
        offset: u64,
        #[source]
        source: std::io::Error,
    },

    #[error("map file {path}: {detail}")]
    Map { path: PathBuf, detail: String },

    /// The saved map does not describe the device we were just handed.
    #[error(
        "cannot resume: the map file describes a {map_bytes}-byte device but \
         {path} is {actual_bytes} bytes"
    )]
    ResumeMismatch {
        path: PathBuf,
        map_bytes: u64,
        actual_bytes: u64,
    },

    #[error("the clone was cancelled")]
    Cancelled,

    #[error(transparent)]
    Device(#[from] rc_device::DeviceError),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}
