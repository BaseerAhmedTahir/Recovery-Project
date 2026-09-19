//! Telling the caller what a long scan is doing, and letting it stop.
//!
//! A filesystem scan on a fixture takes a moment; on a real 728 GB volume the
//! $MFT alone holds millions of records, and a scan that reports nothing until
//! it finishes looks broken - which is exactly how it looked to the first
//! person who ran it. Scanners report as they go and check [`ScanCtx::stopped`]
//! often enough that Stop is immediate.

use std::sync::atomic::{AtomicBool, Ordering};

/// Where a scan has got to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsProgress {
    /// Said to a person: "reading the list of files on this drive".
    pub phase: String,
    pub done: u64,
    pub total: u64,
    /// Deleted entries found so far, when the scanner can count them yet.
    pub found: u64,
}

/// Progress sink and stop flag handed to a scan.
#[derive(Default)]
pub struct ScanCtx<'a> {
    stop: Option<&'a AtomicBool>,
    progress: Option<&'a mut dyn FnMut(FsProgress)>,
}

impl<'a> ScanCtx<'a> {
    /// A scan that reports nothing and is never stopped.
    pub fn quiet() -> ScanCtx<'a> {
        ScanCtx::default()
    }

    pub fn new(stop: &'a AtomicBool, progress: &'a mut dyn FnMut(FsProgress)) -> ScanCtx<'a> {
        ScanCtx {
            stop: Some(stop),
            progress: Some(progress),
        }
    }

    pub fn stopped(&self) -> bool {
        self.stop.is_some_and(|s| s.load(Ordering::Relaxed))
    }

    pub fn report(&mut self, phase: &str, done: u64, total: u64, found: u64) {
        if let Some(cb) = self.progress.as_mut() {
            cb(FsProgress {
                phase: phase.to_string(),
                done,
                total,
                found,
            });
        }
    }
}
