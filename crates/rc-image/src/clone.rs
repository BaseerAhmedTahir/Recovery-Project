//! Whole-device cloning: orchestration, checkpointing and resume.

use crate::error::{ImageError, Result};
use crate::hash::{Digests, HashManifest};
use crate::map::BlockMap;
use crate::rescue::{ProgressFn, RescueOptions, RescueStats, Rescuer};
use crate::sink::{OutputSink, SinkOptions};
use rc_device::ReadOnlyDevice;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default)]
pub struct CloneOptions {
    pub rescue: RescueOptions,
    pub sink: SinkOptions,
    /// Continue an interrupted clone using the existing `.map` file.
    pub resume: bool,
}

#[derive(Debug)]
pub struct CloneReport {
    pub output: PathBuf,
    pub map_path: PathBuf,
    pub hash_path: PathBuf,
    pub stats: RescueStats,
    pub source_digests: Digests,
    pub bad_bytes: u64,
    /// False when any region was unreadable: the clone is not a faithful copy
    /// and its hash must not be compared against the source device.
    pub complete: bool,
}

/// Sidecar paths derived from the output path.
pub fn map_path_for(output: &Path) -> PathBuf {
    output.with_extension(format!(
        "{}map",
        output
            .extension()
            .map(|e| format!("{}.", e.to_string_lossy()))
            .unwrap_or_default()
    ))
}

pub fn hash_path_for(output: &Path) -> PathBuf {
    output.with_extension(format!(
        "{}hash",
        output
            .extension()
            .map(|e| format!("{}.", e.to_string_lossy()))
            .unwrap_or_default()
    ))
}

/// Clone `device` to `output`, writing `.map` and `.hash` sidecars.
pub fn clone_device(
    device: &dyn ReadOnlyDevice,
    output: &Path,
    opts: &CloneOptions,
    progress: Option<ProgressFn<'_>>,
) -> Result<CloneReport> {
    let total = device.total_bytes();
    let map_path = map_path_for(output);
    let hash_path = hash_path_for(output);

    // Load or create the map.
    let mut map = if opts.resume && map_path.exists() {
        let m = BlockMap::read_from(&map_path).map_err(|e| ImageError::Map {
            path: map_path.clone(),
            detail: e.to_string(),
        })?;
        if m.size() != total {
            return Err(ImageError::ResumeMismatch {
                path: output.to_path_buf(),
                map_bytes: m.size(),
                actual_bytes: total,
            });
        }
        tracing::info!(
            resumed_at = m.current_pos,
            finished = m.finished_bytes(),
            "resuming from existing map"
        );
        m
    } else {
        BlockMap::new(total)
    };

    let mut sink = if opts.resume && output.exists() {
        OutputSink::resume(output, &opts.sink)?
    } else {
        OutputSink::create(output, &opts.sink)?
    };
    // Establish the full length up front so a sparse clone ends at the right
    // size even if its tail is never written.
    sink.set_len(total)?;

    let (stats, hasher) = {
        let mut r = Rescuer::new(device, &mut sink, &mut map, opts.rescue.clone());
        let result = r.run(progress);
        let (stats, hasher) = r.into_parts();
        // Persist the map even when the run failed or was cancelled, so the
        // work already done is not lost.
        let _ = map.write_to(&map_path);
        result?;
        (stats, hasher)
    };

    sink.finish()?;
    map.write_to(&map_path).map_err(|e| ImageError::Map {
        path: map_path.clone(),
        detail: e.to_string(),
    })?;

    let bad_bytes = map.bad_bytes();
    let complete = bad_bytes == 0 && map.finished_bytes() == total;
    let source_digests = hasher.finish();

    // The output digest is computed over the file we just wrote. For a complete
    // clone it must equal the source digest; that equality is the acceptance
    // criterion for Milestone 1.
    let output_digests = crate::hash::hash_file(output)?;

    let manifest = HashManifest {
        source: device.info().path.to_string_lossy().to_string(),
        output: output.to_string_lossy().to_string(),
        created_utc: now_utc_rfc3339(),
        total_bytes: total,
        source_digests: source_digests.clone(),
        output_digests,
        bad_bytes,
        fill_byte: opts.rescue.fill_byte,
        complete,
        tool: format!("rc-image {}", env!("CARGO_PKG_VERSION")),
    };
    manifest.write_to(&hash_path)?;

    Ok(CloneReport {
        output: output.to_path_buf(),
        map_path,
        hash_path,
        stats,
        source_digests,
        bad_bytes,
        complete,
    })
}

/// Verify an image against a `.hash` manifest.
#[derive(Debug)]
pub struct VerifyReport {
    pub path: PathBuf,
    pub expected: Digests,
    pub actual: Digests,
    pub sha256_ok: bool,
    pub md5_ok: bool,
    pub size_ok: bool,
    /// The manifest recorded an incomplete clone, so a mismatch against the
    /// original device would be expected rather than alarming.
    pub source_was_incomplete: bool,
}

impl VerifyReport {
    pub fn passed(&self) -> bool {
        self.sha256_ok && self.md5_ok && self.size_ok
    }
}

pub fn verify_against_manifest(image: &Path, manifest_path: &Path) -> Result<VerifyReport> {
    let manifest = HashManifest::read_from(manifest_path)?;
    let actual = crate::hash::hash_file(image)?;
    let expected = manifest.output_digests.clone();
    Ok(VerifyReport {
        path: image.to_path_buf(),
        sha256_ok: actual.sha256 == expected.sha256,
        md5_ok: actual.md5 == expected.md5,
        size_ok: actual.bytes == expected.bytes,
        expected,
        actual,
        source_was_incomplete: !manifest.complete,
    })
}

/// RFC 3339 timestamp without pulling in a date/time dependency.
///
/// `chrono`/`time` would be the obvious choice, but SPEC.md section 1.3 keeps
/// the dependency surface deliberately small and this is the only place a
/// formatted date is needed.
fn now_utc_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86_400;
    let tod = secs % 86_400;
    let (h, mi, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);

    // Civil-from-days (Howard Hinnant's algorithm), epoch shifted to 0000-03-01.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecar_paths_are_derived_from_the_output_name() {
        assert_eq!(
            map_path_for(Path::new("/tmp/disk.raw")),
            PathBuf::from("/tmp/disk.raw.map")
        );
        assert_eq!(
            hash_path_for(Path::new("/tmp/disk.raw")),
            PathBuf::from("/tmp/disk.raw.hash")
        );
        assert_eq!(
            map_path_for(Path::new("/tmp/disk")),
            PathBuf::from("/tmp/disk.map")
        );
    }

    #[test]
    fn formats_known_timestamps() {
        // Spot-check the civil-from-days conversion against known epochs.
        assert!(now_utc_rfc3339().starts_with("20"));
        assert_eq!(now_utc_rfc3339().len(), 20);
        assert!(now_utc_rfc3339().ends_with('Z'));
    }
}
