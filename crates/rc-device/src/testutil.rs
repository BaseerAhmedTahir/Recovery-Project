//! Fault-injecting device wrapper, for testing recovery behaviour.
//!
//! Gated behind the `test-util` feature and never compiled into a release
//! binary. It lives inside `rc-device` because [`crate::ReadOnlyDevice`] is
//! sealed: no other crate can implement it, which is exactly the property that
//! keeps a write path from being introduced from outside. A test double
//! therefore has to be provided here rather than written in the consuming
//! crate's tests.
//!
//! This is still a read-only device. It only ever makes reads *fail*; it cannot
//! modify anything.

use crate::error::{DeviceError, Result};
use crate::geometry::{DeviceInfo, Lba};
use crate::readonly::{sealed::Sealed, ReadOnlyDevice};
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};

/// Wraps another device and fails reads that touch configured byte ranges.
///
/// Used to exercise the ddrescue trim and scrape passes without a physically
/// failing drive.
pub struct FaultyDevice {
    inner: Box<dyn ReadOnlyDevice>,
    bad_ranges: Vec<Range<u64>>,
    /// Reads that overlap a bad range but succeed after this many attempts,
    /// modelling a marginal sector that sometimes reads.
    flaky_after: Option<u64>,
    attempts: AtomicU64,
    reads: AtomicU64,
}

impl FaultyDevice {
    /// Fail every read overlapping any of `bad_ranges` (byte offsets).
    pub fn new(inner: Box<dyn ReadOnlyDevice>, bad_ranges: Vec<Range<u64>>) -> Self {
        FaultyDevice {
            inner,
            bad_ranges,
            flaky_after: None,
            attempts: AtomicU64::new(0),
            reads: AtomicU64::new(0),
        }
    }

    /// Succeed on bad ranges once they have been attempted `n` times.
    pub fn flaky_after(mut self, n: u64) -> Self {
        self.flaky_after = Some(n);
        self
    }

    /// Total reads issued, including failed ones.
    pub fn read_count(&self) -> u64 {
        self.reads.load(Ordering::Relaxed)
    }

    /// Reads that landed on a configured bad range.
    pub fn fault_attempts(&self) -> u64 {
        self.attempts.load(Ordering::Relaxed)
    }

    pub fn bad_bytes(&self) -> u64 {
        self.bad_ranges.iter().map(|r| r.end - r.start).sum()
    }

    fn overlaps_bad(&self, start: u64, len: u64) -> bool {
        let end = start + len;
        self.bad_ranges
            .iter()
            .any(|r| start < r.end && r.start < end)
    }
}

impl Sealed for FaultyDevice {}

impl ReadOnlyDevice for FaultyDevice {
    fn info(&self) -> &DeviceInfo {
        self.inner.info()
    }

    fn read_at(&self, lba: Lba, buf: &mut [u8]) -> Result<usize> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let ss = self.info().sector_size;
        let start = lba.byte_offset(ss);

        if self.overlaps_bad(start, buf.len() as u64) {
            let n = self.attempts.fetch_add(1, Ordering::Relaxed) + 1;
            let recovers = self.flaky_after.is_some_and(|t| n >= t);
            if !recovers {
                return Err(DeviceError::Read {
                    path: self.info().path.clone(),
                    lba: lba.0,
                    source: std::io::Error::other("injected read fault"),
                });
            }
        }
        self.inner.read_at(lba, buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A single bad range. Written as an explicit `Range` rather than
    /// `vec![a..b]`, which clippy flags as ambiguous between "a Vec holding one
    /// Range" (what we want) and "a Vec of every value in the range".
    fn one_range(start: u64, end: u64) -> Vec<Range<u64>> {
        vec![Range { start, end }]
    }

    fn scratch(name: &str, sectors: u64) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("rc-device-testutil");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        for lba in 0..sectors {
            let mut s = [0u8; 512];
            s[..8].copy_from_slice(&lba.to_le_bytes());
            f.write_all(&s).unwrap();
        }
        path
    }

    #[test]
    fn fails_reads_inside_bad_ranges_and_passes_others() {
        let p = scratch("faulty.img", 64);
        let inner = crate::open(&p, None).unwrap();
        let dev = FaultyDevice::new(inner, one_range(4096, 8192));

        let mut buf = vec![0u8; 512];
        assert!(
            dev.read_at(Lba(0), &mut buf).is_ok(),
            "outside the bad range"
        );
        assert!(
            dev.read_at(Lba(8), &mut buf).is_err(),
            "inside the bad range"
        );
        assert!(dev.read_at(Lba(15), &mut buf).is_err(), "still inside");
        assert!(dev.read_at(Lba(16), &mut buf).is_ok(), "past the bad range");
        assert_eq!(dev.fault_attempts(), 2);
    }

    #[test]
    fn flaky_ranges_recover_after_enough_attempts() {
        let p = scratch("flaky.img", 64);
        let inner = crate::open(&p, None).unwrap();
        let dev = FaultyDevice::new(inner, one_range(4096, 8192)).flaky_after(3);

        let mut buf = vec![0u8; 512];
        assert!(dev.read_at(Lba(8), &mut buf).is_err(), "attempt 1");
        assert!(dev.read_at(Lba(8), &mut buf).is_err(), "attempt 2");
        assert!(dev.read_at(Lba(8), &mut buf).is_ok(), "attempt 3 recovers");
    }

    #[test]
    fn a_read_straddling_a_bad_range_fails() {
        let p = scratch("straddle.img", 64);
        let inner = crate::open(&p, None).unwrap();
        let dev = FaultyDevice::new(inner, one_range(4096, 4608));
        // 4 sectors from LBA 6 covers bytes 3072..5120, overlapping the range.
        let mut buf = vec![0u8; 2048];
        assert!(dev.read_at(Lba(6), &mut buf).is_err());
    }
}
