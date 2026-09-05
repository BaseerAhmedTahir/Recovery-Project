//! `rc-image` - forensic imaging for RECOVERY-CORE.
//!
//! Two responsibilities:
//!
//! 1. **Cloning.** A ddrescue-style bit-stream copy of a failing device to a
//!    `.raw` file, with a `.map` sidecar recording the status of every byte
//!    range and a `.hash` manifest of streaming SHA-256 + MD5 digests. Copies
//!    are resumable from the map (SPEC.md section 5.2).
//!
//! 2. **Being the only write path.** [`OutputSink`] is the single place in the
//!    project where a file is created, and it refuses any destination that
//!    resolves to a device currently registered as a scan source
//!    (SPEC.md sections 4.1 and 4.2). `rc-device` has no write path at all,
//!    so every byte the project emits passes through here.

pub mod clone;
pub mod error;
pub mod hash;
pub mod map;
pub mod rescue;
pub mod resolve;
pub mod sink;
pub mod sparse;

pub use clone::{
    clone_device, hash_path_for, map_path_for, verify_against_manifest, CloneOptions, CloneReport,
    VerifyReport,
};
pub use error::{ImageError, Result};
pub use hash::{hash_file, Digests, HashManifest, StreamHasher};
pub use map::{Block, BlockMap, BlockStatus};
pub use rescue::{RescueOptions, RescueStats, Rescuer};
pub use resolve::{resolve, Backing};
pub use sink::{check_destination, OutputSink, SinkOptions, UnknownBackingPolicy};
