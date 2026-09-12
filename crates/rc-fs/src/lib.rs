//! `rc-fs` - filesystem parsers focused on deleted entries (SPEC.md 5.4).
//!
//! Every parser produces the same [`Entry`] vocabulary, so the carving,
//! scoring and CLI layers never branch on filesystem type. Allocated files are
//! enumerated too, but only so the scoring engine knows which clusters are
//! occupied; the CLI hides them by default.

pub mod detect;
pub mod entry;
pub mod error;
pub mod exfat;
pub mod fat;
pub mod ntfs;

pub use detect::{detect, scan_volume, FsType};
pub use entry::{
    DataLocation, Entry, EntryKind, EntryState, Extent, Geometry, PathConfidence, ScanResult,
    Timestamps,
};
pub use error::{FsError, Result};
