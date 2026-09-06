//! `rc-carve` - signature carving for RECOVERY-CORE (SPEC.md section 5.5).
//!
//! Carving finds files by their content when the filesystem metadata that
//! described them is gone. That makes it powerful and makes it noisy, and the
//! noise is the hard part:
//!
//! * `FF D8 FF` occurs by chance roughly once every 16 MB of random data, and
//!   *deliberately* inside every JPEG carrying an EXIF thumbnail.
//! * Every OOXML document and every JAR is a ZIP, so a naive carver reports
//!   `.zip` for all of them.
//! * A format with no footer is bounded by nothing at all unless a maximum
//!   size is imposed, so one spurious header can swallow a whole volume.
//!
//! A carver that emits eighty thousand candidates and happens to include every
//! file you wanted has perfect recall and is useless. So this engine is built
//! to be graded on **precision as well as recall**, validators do real
//! structural decoding rather than header matching, and every signature carries
//! a mandatory size ceiling.

pub(crate) mod crc32;
pub mod error;
pub mod prefilter;
pub mod signature;
pub mod validate;

pub use error::{CarveError, Result};
pub use prefilter::{Prefilter, ScanIndex};
pub use signature::{Category, Signature, SignatureDb};
pub use validate::{Outcome, Status};
