//! ddrescue-style copy strategy (SPEC.md section 5.2).
//!
//! A failing drive has a limited number of reads left in it, and every retry of
//! a bad sector costs seconds and can accelerate the failure. So the passes are
//! ordered to grab everything easy first and only then work at the damaged
//! edges:
//!
//! 1. **Copy** - large sequential blocks. On the first error the whole block is
//!    marked `*` (non-trimmed) and the pass skips ahead. No retries at all.
//! 2. **Trim** - walk inward from each end of a failed block at sector
//!    granularity to find where the good data actually stops. Narrows the
//!    damage without re-reading the middle.
//! 3. **Scrape** - read the remaining unknown region sector by sector, in both
//!    directions, so a bad sector in the middle does not hide the good sectors
//!    behind it.
//! 4. **Retry** - optional extra passes over confirmed-bad sectors, for drives
//!    that sometimes succeed on the nth attempt.
//!
//! Every pass updates the map, and the map is flushed periodically, so killing
//! the process and resuming loses at most the current block.

use crate::error::Result;
use crate::hash::StreamHasher;
use crate::map::{BlockMap, BlockStatus};
use crate::sink::OutputSink;
use crate::sparse;
use rc_device::{AlignedBuf, Lba, ReadOnlyDevice};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct RescueOptions {
    /// Size of a bulk read in the copy pass.
    pub block_bytes: usize,
    /// Extra passes over confirmed-bad sectors. 0 disables retrying.
    pub retries: u32,
    /// Skip writing runs of zeros, leaving holes in the output.
    pub sparse: bool,
    /// Byte used to fill unreadable regions in the output.
    pub fill_byte: u8,
    /// How often to flush the map file.
    pub checkpoint_interval: Duration,
    /// Abort the copy pass after this many failed blocks. `None` means never.
    pub max_error_blocks: Option<u64>,
}

impl Default for RescueOptions {
    fn default() -> Self {
        RescueOptions {
            block_bytes: 1024 * 1024,
            retries: 0,
            sparse: true,
            fill_byte: 0,
            checkpoint_interval: Duration::from_secs(10),
            max_error_blocks: None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct RescueStats {
    pub bytes_copied: u64,
    pub bytes_bad: u64,
    pub read_errors: u64,
    pub blocks_skipped: u64,
    pub elapsed: Duration,
}

impl RescueStats {
    pub fn throughput_mib_s(&self) -> f64 {
        let secs = self.elapsed.as_secs_f64();
        if secs <= 0.0 {
            0.0
        } else {
            (self.bytes_copied as f64 / (1024.0 * 1024.0)) / secs
        }
    }
}

/// Callback for progress reporting. Returning `false` cancels the copy.
pub type ProgressFn<'a> = &'a mut dyn FnMut(&BlockMap, &RescueStats) -> bool;

pub struct Rescuer<'a> {
    device: &'a dyn ReadOnlyDevice,
    sink: &'a mut OutputSink,
    map: &'a mut BlockMap,
    opts: RescueOptions,
    stats: RescueStats,
    hasher: StreamHasher,
    last_checkpoint: Instant,
}

impl<'a> Rescuer<'a> {
    pub fn new(
        device: &'a dyn ReadOnlyDevice,
        sink: &'a mut OutputSink,
        map: &'a mut BlockMap,
        opts: RescueOptions,
    ) -> Self {
        Rescuer {
            device,
            sink,
            map,
            opts,
            stats: RescueStats::default(),
            hasher: StreamHasher::new(),
            last_checkpoint: Instant::now(),
        }
    }

    pub fn into_parts(self) -> (RescueStats, StreamHasher) {
        (self.stats, self.hasher)
    }

    /// Run every pass in order.
    pub fn run(&mut self, progress: Option<ProgressFn<'_>>) -> Result<()> {
        let start = Instant::now();

        // Collapse the Option once. `Option<&mut dyn FnMut>` cannot be
        // reborrowed across successive calls, but a plain `&mut dyn FnMut` can.
        let mut noop = |_: &BlockMap, _: &RescueStats| true;
        let cb: &mut dyn FnMut(&BlockMap, &RescueStats) -> bool = match progress {
            Some(p) => p,
            None => &mut noop,
        };

        self.copy_pass(&mut *cb)?;
        self.trim_pass(&mut *cb)?;
        self.scrape_pass(&mut *cb)?;
        for pass in 0..self.opts.retries {
            self.map.current_pass = 4 + pass;
            self.retry_pass(&mut *cb)?;
        }

        self.stats.elapsed = start.elapsed();
        self.stats.bytes_bad = self.map.bad_bytes();
        Ok(())
    }

    // --- pass 1: bulk sequential copy -------------------------------------

    fn copy_pass(
        &mut self,
        progress: &mut dyn FnMut(&BlockMap, &RescueStats) -> bool,
    ) -> Result<()> {
        self.map.current_pass = 1;
        self.map.current_status = BlockStatus::NonTried;

        let ss = self.device.sector_size();
        let block = ss
            .align_up(self.opts.block_bytes as u64)
            .max(ss.get() as u64);
        let mut buf = AlignedBuf::new(block as usize, ss.as_usize());
        let mut error_blocks = 0u64;

        let mut pos = 0u64;
        while let Some(b) = self.map.next_block_with(pos, BlockStatus::NonTried) {
            let len = b.size.min(block);
            self.map.current_pos = b.pos;

            match self.read_range(b.pos, len as usize, &mut buf) {
                Ok(got) => {
                    self.write_out(b.pos, &buf[..got])?;
                    self.map.mark(b.pos, got as u64, BlockStatus::Finished);
                    self.stats.bytes_copied += got as u64;
                    pos = b.pos + got.max(1) as u64;
                }
                Err(_) => {
                    // No retry here on purpose: the whole block is deferred to
                    // the trim pass so the copy pass keeps moving.
                    self.map.mark(b.pos, len, BlockStatus::NonTrimmed);
                    self.stats.read_errors += 1;
                    self.stats.blocks_skipped += 1;
                    error_blocks += 1;
                    pos = b.pos + len;
                    tracing::debug!(offset = b.pos, len, "copy pass: block failed, deferring");
                    if self
                        .opts
                        .max_error_blocks
                        .is_some_and(|m| error_blocks >= m)
                    {
                        tracing::warn!(error_blocks, "copy pass aborted: too many read errors");
                        break;
                    }
                }
            }

            if !self.tick(progress) {
                return Err(crate::error::ImageError::Cancelled);
            }
        }
        Ok(())
    }

    // --- pass 2: trim inward from both ends -------------------------------

    fn trim_pass(
        &mut self,
        progress: &mut dyn FnMut(&BlockMap, &RescueStats) -> bool,
    ) -> Result<()> {
        self.map.current_pass = 2;
        self.map.current_status = BlockStatus::NonTrimmed;

        let ss = self.device.sector_size();
        let sector = ss.get() as u64;
        let mut buf = AlignedBuf::new(ss.as_usize(), ss.as_usize());

        for b in self.map.blocks_with(BlockStatus::NonTrimmed) {
            // Forward from the start until a sector fails.
            let mut off = b.pos;
            while off < b.end() {
                self.map.current_pos = off;
                match self.read_range(off, sector as usize, &mut buf) {
                    Ok(got) if got > 0 => {
                        self.write_out(off, &buf[..got])?;
                        self.map.mark(off, got as u64, BlockStatus::Finished);
                        self.stats.bytes_copied += got as u64;
                        off += got as u64;
                    }
                    _ => {
                        self.stats.read_errors += 1;
                        break;
                    }
                }
                if !self.tick(progress) {
                    return Err(crate::error::ImageError::Cancelled);
                }
            }

            // Backward from the end until a sector fails.
            let mut end = b.end();
            while end > off {
                let at = end - sector;
                self.map.current_pos = at;
                match self.read_range(at, sector as usize, &mut buf) {
                    Ok(got) if got > 0 => {
                        self.write_out(at, &buf[..got])?;
                        self.map.mark(at, got as u64, BlockStatus::Finished);
                        self.stats.bytes_copied += got as u64;
                        end = at;
                    }
                    _ => {
                        self.stats.read_errors += 1;
                        break;
                    }
                }
                if !self.tick(progress) {
                    return Err(crate::error::ImageError::Cancelled);
                }
            }

            // Whatever is left in the middle still needs sector-by-sector work.
            if end > off {
                self.map.mark(off, end - off, BlockStatus::NonScraped);
            }
        }
        Ok(())
    }

    // --- pass 3: scrape sector by sector ----------------------------------

    fn scrape_pass(
        &mut self,
        progress: &mut dyn FnMut(&BlockMap, &RescueStats) -> bool,
    ) -> Result<()> {
        self.map.current_pass = 3;
        self.map.current_status = BlockStatus::NonScraped;

        let ss = self.device.sector_size();
        let sector = ss.get() as u64;
        let mut buf = AlignedBuf::new(ss.as_usize(), ss.as_usize());

        for b in self.map.blocks_with(BlockStatus::NonScraped) {
            let mut off = b.pos;
            while off < b.end() {
                self.map.current_pos = off;
                match self.read_range(off, sector as usize, &mut buf) {
                    Ok(got) if got > 0 => {
                        self.write_out(off, &buf[..got])?;
                        self.map.mark(off, got as u64, BlockStatus::Finished);
                        self.stats.bytes_copied += got as u64;
                    }
                    _ => {
                        // Confirmed bad. Fill the output so offsets downstream
                        // still line up with the source device.
                        self.fill_bad(off, sector as usize)?;
                        self.map.mark(off, sector, BlockStatus::Bad);
                        self.stats.read_errors += 1;
                    }
                }
                off += sector;
                if !self.tick(progress) {
                    return Err(crate::error::ImageError::Cancelled);
                }
            }
        }
        Ok(())
    }

    // --- pass 4+: retry confirmed-bad sectors ------------------------------

    fn retry_pass(
        &mut self,
        progress: &mut dyn FnMut(&BlockMap, &RescueStats) -> bool,
    ) -> Result<()> {
        self.map.current_status = BlockStatus::Bad;
        let ss = self.device.sector_size();
        let sector = ss.get() as u64;
        let mut buf = AlignedBuf::new(ss.as_usize(), ss.as_usize());

        for b in self.map.blocks_with(BlockStatus::Bad) {
            let mut off = b.pos;
            while off < b.end() {
                self.map.current_pos = off;
                if let Ok(got) = self.read_range(off, sector as usize, &mut buf) {
                    if got > 0 {
                        self.write_out(off, &buf[..got])?;
                        self.map.mark(off, got as u64, BlockStatus::Finished);
                        self.stats.bytes_copied += got as u64;
                        tracing::info!(offset = off, "retry recovered a previously bad sector");
                    }
                }
                off += sector;
                if !self.tick(progress) {
                    return Err(crate::error::ImageError::Cancelled);
                }
            }
        }
        Ok(())
    }

    // --- helpers ----------------------------------------------------------

    fn read_range(
        &self,
        offset: u64,
        len: usize,
        buf: &mut AlignedBuf,
    ) -> std::result::Result<usize, rc_device::DeviceError> {
        let ss = self.device.sector_size();
        let lba = Lba(offset / ss.get() as u64);
        let len = len.min(buf.len());
        self.device.read_at(lba, &mut buf[..len])
    }

    fn write_out(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        // Hashing tracks the source bytes as they are read, which is what makes
        // the manifest meaningful without a second full pass.
        self.hasher.update(data);
        if self.opts.sparse && sparse::is_all_zero(data) {
            return Ok(()); // leave a hole
        }
        self.sink.write_at(offset, data)
    }

    fn fill_bad(&mut self, offset: u64, len: usize) -> Result<()> {
        if self.opts.fill_byte == 0 && self.opts.sparse {
            return Ok(()); // a hole already reads as zero
        }
        let fill = vec![self.opts.fill_byte; len];
        self.hasher.update(&fill);
        self.sink.write_at(offset, &fill)
    }

    /// Periodic progress callback. Returns false when the caller cancels.
    fn tick(&mut self, progress: &mut dyn FnMut(&BlockMap, &RescueStats) -> bool) -> bool {
        if self.last_checkpoint.elapsed() < self.opts.checkpoint_interval {
            return true;
        }
        self.last_checkpoint = Instant::now();
        progress(self.map, &self.stats)
    }
}
