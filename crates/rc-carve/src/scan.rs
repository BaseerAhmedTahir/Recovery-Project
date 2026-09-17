//! The scanner: find candidate files in raw sectors.
//!
//! # What "precision" means here, and why it is not what the validators measure
//!
//! The validator suite is graded with known file boundaries handed to it. This
//! is the opposite situation. A scan of a real device meets:
//!
//! * every three-byte coincidence in gigabytes of unstructured sectors,
//! * compressed streams whose bytes happen to read as `ftyp` boxes or `BM`,
//! * EXIF thumbnails, which are whole valid JPEGs inside other JPEGs,
//! * JPEGs and PNGs inside docx files on the same volume,
//! * and slack space holding fragments of files deleted long before these.
//!
//! So the scanner is graded on its own denominator: candidates generated per
//! GB scanned, how many survive validation, and how many were suppressed as
//! contained inside a larger accepted file. [`ScanStats`] reports each of
//! those as a separate number, because a single "precision" figure hides which
//! stage did the work.
//!
//! # Structure
//!
//! One reader thread does large sequential reads; N workers prefilter and
//! match headers; a validation pass then re-reads each surviving candidate.
//! SPEC.md section 5.5 asks for that shape, and it is the right one - but
//! not for the reason it looks like.
//!
//! **Whether the scan is I/O-bound depends on the storage, and two earlier
//! versions of this comment were wrong about it.** The first claimed the scan
//! was I/O-bound "on any real medium" because one thread prefilters at roughly
//! 1 GB/s. The second, written from a single benchmark run, said the disk cost
//! about a quarter of the throughput. Measured properly - median of five runs,
//! against a raw-read baseline, in `tests/throughput.rs` - on this machine's
//! NVMe drive:
//!
//! | tier | MiB/s |
//! |---|---|
//! | raw unbuffered read, no scanning | 1256 |
//! | full scan, unbuffered | 336 |
//! | full scan, from the page cache | 349 |
//! | prefilter + header match, one thread | 189 |
//!
//! So on fast storage the scan is **CPU-bound**: it consumes 27% of what the
//! disk delivers, and reading from the disk costs almost nothing over reading
//! from RAM. Against a USB stick or an SD card at tens of MB/s the engine is
//! far ahead and the scan is I/O-bound instead. The 1 GB/s figure was the
//! prefilter alone on random bytes; header matching on a real image is what
//! brings it down.
//!
//! Two consequences worth stating plainly:
//!
//! * **The workers scale poorly.** Eight threads scan at about 1.8 times the
//!   one-thread rate. Something is serialising them - the shared block channel
//!   is the first suspect - and on fast storage that is now the limit. Not a
//!   Milestone 3 requirement, and recorded rather than hidden.
//! * **The reader is not the problem.** At a queue depth of one it still pulls
//!   1.26 GB/s from the drive, nearly four times what the workers use. An
//!   earlier version of the benchmark printed a note blaming the queue depth,
//!   unconditionally and without measuring it; the raw-read tier is what showed
//!   that note was wrong.
//!
//! Any MB/s figure this produces describes the device, or on a cached fixture
//! the page cache; [`ScanStats::cache_warning`] says which, and `rc carve
//! --unbuffered` removes the ambiguity.
//!
//! # Bounded memory
//!
//! Milestone 3 requires peak RSS under 2 GB regardless of device size. Block
//! buffers are recycled through a channel rather than allocated per block, and
//! the number in flight is fixed. Validation buffers are per-worker and
//! reused. Nothing here grows with the size of the device; the candidate list
//! does grow with the number of hits, which is what `rc-index` exists to move
//! onto disk.

use crate::prefilter::ScanIndex;
use crate::signature::{Category, Signature, SignatureDb};
use crate::validate::{self, Status};
use rc_device::ReadOnlyDevice;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How much data to hand a validator for one candidate.
///
/// Most validators must walk to the end of the file to establish its length -
/// JPEG to its EOI, PNG to its IEND, ZIP to its end-of-central-directory - so
/// this bounds how large a file can be established exactly. A candidate whose
/// format allows more than this is validated against a truncated view and will
/// usually come back `Partial`; [`ScanStats::window_capped`] counts them so the
/// limit is visible in the numbers rather than silently changing the verdict.
pub const DEFAULT_VALIDATION_WINDOW: usize = 16 << 20;

/// Bytes read per sequential I/O.
pub const DEFAULT_BLOCK: usize = 4 << 20;

#[derive(Clone, Debug)]
pub struct ScanOptions {
    pub block_bytes: usize,
    /// Worker threads. `None` uses the available parallelism, capped at 8 -
    /// past that the reader is the limit and extra threads only add memory.
    pub threads: Option<usize>,
    /// Run validators. With this off the scan reports raw header matches,
    /// which is the honest baseline to compare validated output against.
    pub validate: bool,
    pub validation_window: usize,
    /// Restrict the scan to these signature ids. Fewer signatures usually
    /// means a narrower prefilter and a much faster scan.
    pub only: Option<Vec<String>>,
    /// Drop candidates wholly inside a *structurally complete* candidate - the
    /// EXIF-thumbnail case, and the images-inside-a-docx case.
    ///
    /// Only a `Valid` candidate may suppress. A `Partial` one has no
    /// established length, and letting it suppress destroys recall; see the
    /// note on [`suppress_contained`].
    pub suppress_contained: bool,
    /// Stop after this many candidates. A safety bound, not a feature: hitting
    /// it is reported in [`ScanStats::truncated`].
    pub max_candidates: usize,
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions {
            block_bytes: DEFAULT_BLOCK,
            threads: None,
            validate: true,
            validation_window: DEFAULT_VALIDATION_WINDOW,
            only: None,
            suppress_contained: true,
            max_candidates: 5_000_000,
        }
    }
}

/// One thing the scanner thinks might be a file.
#[derive(Clone, Debug)]
pub struct Candidate {
    /// Byte offset from the start of the scanned region.
    pub offset: u64,
    /// Established length. Zero when the candidate was never validated.
    pub length: u64,
    pub signature_id: String,
    /// The extension to use, after any refinement a validator made - `docx`
    /// rather than `zip`.
    pub ext: String,
    pub category: Category,
    pub status: Status,
    pub detail: String,
    pub evidence: Vec<(&'static str, String)>,
    /// Whether `length` is the file's real size or a floor. Only an
    /// established length is allowed to suppress overlapping candidates.
    pub length_established: bool,
    /// How many bytes of header this signature had to match, counting the
    /// offset it sits at.
    ///
    /// Used to pick a winner when two signatures describe the same bytes. An
    /// M4A file matches both `ftyp` at offset 4 (four bytes) and `ftypM4A ` at
    /// offset 4 (eight bytes); the longer match is the more specific claim and
    /// is the one worth reporting, for the same reason a .docx should not come
    /// back as a .zip.
    pub header_span: usize,
}

impl Candidate {
    pub fn end(&self) -> u64 {
        self.offset.saturating_add(self.length)
    }
}

/// Every number needed to judge a scan, kept separate on purpose.
#[derive(Clone, Debug, Default)]
pub struct ScanStats {
    pub bytes_scanned: u64,
    pub elapsed: Duration,
    /// Positions the prefilter flagged as worth a full comparison.
    pub prefilter_hits: u64,
    /// Prefilter hits that actually matched a signature header. This is the
    /// scanner's raw output and the denominator that matters.
    pub header_matches: u64,
    /// Header matches a validator accepted.
    pub validated: u64,
    /// Header matches a validator rejected.
    pub rejected: u64,
    /// Accepted candidates dropped as contained inside a larger one.
    pub suppressed_contained: u64,
    /// Candidates whose format permits a file larger than the validation
    /// window, so their verdict was reached on a truncated view.
    pub window_capped: u64,
    /// Candidates whose reported length was discarded because it was just the
    /// size of the validation window rather than anything the file established.
    pub window_artifact_lengths: u64,
    /// `max_candidates` was reached and the scan stopped early.
    pub truncated: bool,
    pub strategy: &'static str,
    pub threads: usize,
    pub signatures: usize,
}

impl ScanStats {
    pub fn mib_per_sec(&self) -> f64 {
        let secs = self.elapsed.as_secs_f64();
        if secs <= 0.0 {
            return 0.0;
        }
        (self.bytes_scanned as f64 / (1024.0 * 1024.0)) / secs
    }

    /// Candidates generated per GB scanned - the scanner's own denominator.
    pub fn header_matches_per_gb(&self) -> f64 {
        let gb = self.bytes_scanned as f64 / (1024.0 * 1024.0 * 1024.0);
        if gb <= 0.0 {
            return 0.0;
        }
        self.header_matches as f64 / gb
    }

    /// Share of header matches that survived validation. This is a property of
    /// the *validators*, not of the scan, and is reported alongside the raw
    /// count rather than instead of it.
    pub fn validation_survival(&self) -> f64 {
        if self.header_matches == 0 {
            return 0.0;
        }
        self.validated as f64 / self.header_matches as f64
    }

    /// A throughput figure from a source small enough to sit in RAM measures
    /// the page cache. Say so rather than printing it as a device speed.
    pub fn cache_warning(&self) -> Option<&'static str> {
        if self.bytes_scanned < (2 << 30) {
            Some(
                "the scanned region is small enough to be served from the page cache; \
                 this MB/s figure is not a device throughput",
            )
        } else {
            None
        }
    }
}

pub struct ScanResult {
    pub candidates: Vec<Candidate>,
    pub stats: ScanStats,
}

// ---------------------------------------------------------------------------
// block plumbing
// ---------------------------------------------------------------------------

/// A block of the device plus the overlap needed to catch a header that
/// straddles the boundary into the next one.
struct Block {
    /// Absolute offset of `buf[0]`.
    start: u64,
    /// Bytes belonging to this block. Anything past this in `buf` is overlap
    /// that the *next* block owns.
    owned: usize,
    buf: Vec<u8>,
    /// Valid bytes in `buf`, i.e. `owned` plus however much overlap was read.
    filled: usize,
}

/// Read the device into recycled buffers.
///
/// Buffers cycle between the reader and the workers through `recycle`, so the
/// number allocated is fixed no matter how large the device is.
#[allow(clippy::too_many_arguments)]
fn reader_thread(
    device: Arc<dyn ReadOnlyDevice>,
    start: u64,
    end: u64,
    read_end: u64,
    block_bytes: usize,
    overlap: usize,
    out: SyncSender<Block>,
    recycle: Receiver<Vec<u8>>,
) {
    let mut pos = start;
    while pos < end {
        let mut buf = match recycle.recv() {
            Ok(b) => b,
            Err(_) => return, // every worker is gone
        };
        buf.resize(block_bytes + overlap, 0);

        // Blocks are owned up to `end`, but the overlap that lets a header
        // straddling a block boundary be seen reads on to `read_end`. For a
        // segment of a larger scan those differ, and reading overlap only to the
        // segment's end would miss a header in its last few bytes - making the
        // result depend on where the scan was split.
        let owned = block_bytes.min((end - pos) as usize);
        let want = (block_bytes + overlap).min((read_end - pos) as usize);

        let filled = match device.read_bytes_at(pos, &mut buf[..want]) {
            Ok(n) => n,
            // A bad sector is not fatal to a carve: skip the block and keep
            // going. rc-image's rescue pass is where read errors get retried;
            // here the point is not to abandon the rest of the device.
            Err(_) => {
                if out
                    .send(Block {
                        start: pos,
                        owned: 0,
                        buf,
                        filled: 0,
                    })
                    .is_err()
                {
                    return;
                }
                pos += owned as u64;
                continue;
            }
        };

        if out
            .send(Block {
                start: pos,
                owned: owned.min(filled),
                buf,
                filled,
            })
            .is_err()
        {
            return;
        }
        pos += owned as u64;
    }
}

/// A header match, before validation.
struct Hit {
    offset: u64,
    sig: usize,
}

// ---------------------------------------------------------------------------
// the scan
// ---------------------------------------------------------------------------

/// What one segment of a scan hands to the next.
///
/// A scan split into segments - so it can checkpoint, and resume after being
/// killed - must produce exactly what one uninterrupted scan would. Two things
/// cross a boundary: the furthest end of a complete file already kept (so a
/// thumbnail inside a photo that started in an earlier segment is still
/// suppressed), and the candidate budget.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Carry {
    pub cover_end: u64,
    pub candidates: u64,
}

/// Scan `device` between `start` and `end` for anything the database knows.
pub fn scan(
    device: Arc<dyn ReadOnlyDevice>,
    db: &SignatureDb,
    start: u64,
    end: u64,
    opts: &ScanOptions,
) -> crate::Result<ScanResult> {
    let mut carry = Carry::default();
    scan_segment(device, db, start, end, end, opts, &mut carry)
}

/// Scan one segment `[start, end)` of a scan whose range ends at `range_end`.
///
/// Headers are claimed only if they begin inside the segment. Everything else
/// (overlap reads, validation, containment) behaves as it would for the whole
/// range, so consecutive segments with `carry` threaded through give the same
/// candidates as one call to [`scan`].
pub fn scan_segment(
    device: Arc<dyn ReadOnlyDevice>,
    db: &SignatureDb,
    start: u64,
    end: u64,
    range_end: u64,
    opts: &ScanOptions,
    carry: &mut Carry,
) -> crate::Result<ScanResult> {
    let range_end = range_end.max(end);
    let sigs: Vec<&Signature> = match &opts.only {
        None => db.signatures.iter().collect(),
        Some(ids) => db
            .signatures
            .iter()
            .filter(|s| ids.iter().any(|i| i == &s.id))
            .collect(),
    };
    if sigs.is_empty() {
        return Err(crate::CarveError::SignatureDb {
            detail: "no signatures selected for the scan".into(),
        });
    }

    let max_span = sigs
        .iter()
        .map(|s| s.header_offset + s.header.len())
        .max()
        .unwrap_or(0);
    let index = ScanIndex::from_signatures(sigs.iter().copied(), max_span);
    // One byte less than the longest header: a header starting at the last
    // owned byte must still be fully visible.
    let overlap = max_span.saturating_sub(1);

    let threads = opts
        .threads
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
        })
        .clamp(1, 8);

    let mut stats = ScanStats {
        strategy: index.strategy(),
        threads,
        signatures: sigs.len(),
        ..Default::default()
    };

    let began = Instant::now();

    // --- phase 1: stream the device and match headers ---------------------
    //
    // Buffers in flight are bounded by the channel depth plus what the workers
    // hold, so memory does not grow with device size.
    let depth = threads + 2;
    let (block_tx, block_rx) = sync_channel::<Block>(threads);
    let (recycle_tx, recycle_rx) = sync_channel::<Vec<u8>>(depth);
    for _ in 0..depth {
        recycle_tx
            .send(Vec::with_capacity(opts.block_bytes + overlap))
            .expect("priming the buffer pool");
    }

    let block_rx = std::sync::Mutex::new(block_rx);
    let mut hits: Vec<Hit> = Vec::new();
    let mut prefilter_hits = 0u64;
    let mut bytes = 0u64;

    let sigs = &sigs;
    std::thread::scope(|scope| {
        let dev = Arc::clone(&device);
        scope.spawn(move || {
            reader_thread(
                dev,
                start,
                end,
                range_end,
                opts.block_bytes,
                overlap,
                block_tx,
                recycle_rx,
            );
        });

        let mut workers = Vec::new();
        for _ in 0..threads {
            let block_rx = &block_rx;
            let recycle_tx = recycle_tx.clone();
            let index = &index;
            workers.push(scope.spawn(move || {
                let mut local: Vec<Hit> = Vec::new();
                let mut local_prefilter = 0u64;
                let mut local_bytes = 0u64;
                loop {
                    let block = {
                        let rx = block_rx.lock().expect("block channel");
                        match rx.recv() {
                            Ok(b) => b,
                            Err(_) => break,
                        }
                    };
                    local_bytes += block.owned as u64;

                    if block.filled > 0 {
                        let hay = &block.buf[..block.filled];
                        index.prefilter.for_each_hit(hay, |at| {
                            local_prefilter += 1;
                            for (_sig, rel_start, sig_idx) in index.candidates_at(hay, at) {
                                // Only claim candidates that begin in the part
                                // of the block this block owns. One starting in
                                // the overlap belongs to the next block, and
                                // claiming it here would report it twice.
                                if rel_start >= block.owned {
                                    continue;
                                }
                                local.push(Hit {
                                    offset: block.start + rel_start as u64,
                                    sig: sig_idx,
                                });
                            }
                        });
                    }

                    // Hand the buffer back. A closed channel means the reader
                    // finished, which is not an error.
                    let _ = recycle_tx.send(block.buf);
                }
                (local, local_prefilter, local_bytes)
            }));
        }
        drop(recycle_tx);

        for w in workers {
            let (local, pf, by) = w.join().expect("worker panicked");
            hits.extend(local);
            prefilter_hits += pf;
            bytes += by;
        }
    });

    stats.prefilter_hits = prefilter_hits;
    stats.bytes_scanned = bytes;
    stats.header_matches = hits.len() as u64;

    // Workers finish blocks out of order, so sort into device order. Ties
    // broken by signature so the output is deterministic run to run.
    hits.sort_unstable_by_key(|h| (h.offset, h.sig));

    let budget = (opts.max_candidates as u64).saturating_sub(carry.candidates) as usize;
    if hits.len() > budget {
        hits.truncate(budget);
        stats.truncated = true;
    }

    // --- phase 2: validate ------------------------------------------------
    let mut candidates = Vec::with_capacity(hits.len());
    let mut buf = vec![0u8; opts.validation_window];

    for h in &hits {
        let sig = sigs[h.sig];
        if !opts.validate {
            candidates.push(Candidate {
                offset: h.offset,
                length: 0,
                signature_id: sig.id.clone(),
                ext: sig.ext.clone(),
                category: sig.category,
                status: Status::Partial,
                detail: "not validated".into(),
                evidence: Vec::new(),
                length_established: false,
                header_span: sig.header_offset + sig.header.len(),
            });
            continue;
        }

        let Some(validator) = sig.validator.as_deref() else {
            // Header-match-only formats: 30 of the 44 by design. Emitted, but
            // with no length and a status that says why.
            candidates.push(Candidate {
                offset: h.offset,
                length: 0,
                signature_id: sig.id.clone(),
                ext: sig.ext.clone(),
                category: sig.category,
                status: Status::Partial,
                detail: "header match only; this format has no validator".into(),
                evidence: Vec::new(),
                length_established: false,
                header_span: sig.header_offset + sig.header.len(),
            });
            continue;
        };

        // Bounded by the whole range, not the segment: a file may run on past
        // the segment it starts in.
        let remaining = range_end.saturating_sub(h.offset);
        let reachable = sig.max_size.min(remaining);
        let want = reachable.min(opts.validation_window as u64) as usize;
        let capped = reachable > opts.validation_window as u64;
        if capped {
            stats.window_capped += 1;
        }
        let got = device
            .read_bytes_at(h.offset, &mut buf[..want])
            .unwrap_or(0);
        let Some(mut out) = validate::validate(validator, &buf[..got]) else {
            continue;
        };

        if out.status == Status::Rejected {
            stats.rejected += 1;
            continue;
        }

        // A length equal to the window is not a length, it is the window.
        //
        // A validator handed a truncated view can only say "everything you gave
        // me looks like part of this file", and if the view was cut by the
        // window rather than by the device then that number describes this
        // scanner's configuration and nothing about the file. Reporting it
        // would be bad enough on its own; it also used to let one spurious
        // header claim 16 MiB and suppress every real file inside that span.
        // Only the scanner knows the view was cut, so only the scanner can
        // catch this.
        if capped && !out.length_established && out.length >= got as u64 {
            stats.window_artifact_lengths += 1;
            out.length = 0;
            out.detail = format!(
                "{} (length not established: the {} MiB validation window was reached, \
                 so no end could be found)",
                out.detail,
                opts.validation_window >> 20
            );
        }

        stats.validated += 1;
        candidates.push(Candidate {
            offset: h.offset,
            length: out.length,
            signature_id: sig.id.clone(),
            ext: out
                .refined_ext
                .map(|e| e.to_string())
                .unwrap_or_else(|| sig.ext.clone()),
            category: sig.category,
            status: out.status,
            detail: out.detail,
            evidence: out.evidence,
            length_established: out.length_established,
            header_span: sig.header_offset + sig.header.len(),
        });
    }

    if opts.suppress_contained {
        stats.suppressed_contained = suppress_contained(&mut candidates, &mut carry.cover_end);
    }
    carry.candidates += hits.len() as u64;

    stats.elapsed = began.elapsed();
    Ok(ScanResult { candidates, stats })
}

/// Drop candidates that lie wholly inside a structurally complete candidate.
///
/// This is the EXIF-thumbnail case and the images-inside-a-docx case: both
/// produce candidates that are genuinely valid files, which is exactly why no
/// validator can reject them. Only containment can, and only after lengths are
/// known. Returns how many were dropped, because it is a large number on real
/// data and hiding it inside the accepted count would overstate precision.
///
/// **Only a `Valid` candidate may suppress.** This is not a refinement, it is
/// the difference between the feature working and destroying the carve. On the
/// quick-formatted fixture the first version let any candidate suppress, and 84
/// spurious `PK\x03\x04` matches - every one `Partial`, every one claiming the
/// full validation window because that was all it had been given - ate 131 real
/// images and PDFs. Recall went from 228 of 228 to 26. A `Partial` length is a
/// floor at best and an artifact at worst; only a structurally complete file
/// has an extent worth trusting against other people's data.
fn suppress_contained(candidates: &mut Vec<Candidate>, cover_end: &mut u64) -> u64 {
    // Longest first at each offset, so an outer file is seen before what it
    // contains, and where two candidates cover exactly the same bytes the one
    // that had to match more header wins.
    //
    // That last tiebreak is not cosmetic. An M4A matches `ftyp` at offset 4 and
    // `ftypM4A ` at offset 4; both produce a candidate at the same offset with
    // the same length, one suppresses the other as contained, and without a
    // rule it is whichever the database happened to list first. Before this,
    // every .m4a carved as .mp4.
    candidates.sort_by(|a, b| {
        a.offset
            .cmp(&b.offset)
            .then(b.length.cmp(&a.length))
            .then(b.header_span.cmp(&a.header_span))
    });

    let mut keep: Vec<Candidate> = Vec::with_capacity(candidates.len());
    let mut dropped = 0u64;
    // The furthest end among *complete* files kept so far, carried in from any
    // earlier segment. Because the list is in offset order, anything ending at
    // or before this lies inside one.
    for c in candidates.drain(..) {
        if c.length > 0 && c.end() <= *cover_end {
            dropped += 1;
            continue;
        }
        // Only a length the file itself established may judge other
        // candidates. A floor is not an extent.
        if c.status == Status::Valid || c.length_established {
            *cover_end = (*cover_end).max(c.end());
        }
        keep.push(c);
    }
    *candidates = keep;
    dropped
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_device::testutil;

    fn db() -> SignatureDb {
        SignatureDb::builtin().expect("builtin db")
    }

    /// Build an image with known content at known offsets.
    fn image(parts: &[(u64, Vec<u8>)], total: usize) -> tempfile_lite::Temp {
        let mut data = vec![0u8; total];
        for (at, bytes) in parts {
            let at = *at as usize;
            data[at..at + bytes.len()].copy_from_slice(bytes);
        }
        tempfile_lite::Temp::with_bytes(&data)
    }

    fn png(w: u32, h: u32) -> Vec<u8> {
        use crate::crc32::crc32_parts;
        let chunk = |ty: &[u8; 4], d: &[u8]| {
            let mut v = (d.len() as u32).to_be_bytes().to_vec();
            v.extend_from_slice(ty);
            v.extend_from_slice(d);
            v.extend_from_slice(&crc32_parts(&[ty, d]).to_be_bytes());
            v
        };
        let mut ihdr = w.to_be_bytes().to_vec();
        ihdr.extend_from_slice(&h.to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
        let mut v = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        v.extend(chunk(b"IHDR", &ihdr));
        v.extend(chunk(
            b"IDAT",
            &[0x78, 0x9C, 0x63, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01],
        ));
        v.extend(chunk(b"IEND", &[]));
        v
    }

    fn run(path: &std::path::Path, opts: ScanOptions) -> ScanResult {
        let dev = rc_device::open(path, None).expect("open");
        let end = dev.total_sectors() * dev.sector_size().get() as u64;
        scan(Arc::from(dev), &db(), 0, end, &opts).expect("scan")
    }

    #[test]
    fn finds_a_file_at_a_known_offset_with_its_real_length() {
        let p = png(8, 8);
        let want_len = p.len() as u64;
        let img = image(&[(100_000, p)], 1 << 20);
        let r = run(img.path(), ScanOptions::default());

        let found: Vec<_> = r
            .candidates
            .iter()
            .filter(|c| c.signature_id == "png")
            .collect();
        assert_eq!(found.len(), 1, "expected one png, got {found:?}");
        assert_eq!(found[0].offset, 100_000);
        assert_eq!(found[0].length, want_len);
        assert_eq!(found[0].status, Status::Valid);
    }

    /// A header landing exactly on a block boundary is the classic scanner
    /// bug: found twice, or not at all.
    #[test]
    fn a_file_straddling_a_block_boundary_is_found_exactly_once() {
        let block = 64 * 1024;
        let p = png(4, 4);
        let want_len = p.len() as u64;
        // Put the signature two bytes before the boundary, so the 8-byte PNG
        // magic spans it.
        let at = block as u64 - 2;
        let img = image(&[(at, p)], 1 << 20);

        let opts = ScanOptions {
            block_bytes: block,
            threads: Some(4),
            ..Default::default()
        };
        let r = run(img.path(), opts);
        let found: Vec<_> = r
            .candidates
            .iter()
            .filter(|c| c.signature_id == "png")
            .collect();
        assert_eq!(found.len(), 1, "expected exactly one png, got {found:?}");
        assert_eq!(found[0].offset, at);
        assert_eq!(found[0].length, want_len);
    }

    #[test]
    fn every_block_size_finds_the_same_files() {
        let mut parts = Vec::new();
        for i in 0..8u64 {
            parts.push((10_000 + i * 37_000, png(4 + i as u32, 4)));
        }
        let img = image(&parts, 1 << 20);

        let mut baseline: Option<Vec<(u64, u64)>> = None;
        for block in [8 * 1024usize, 64 * 1024, 256 * 1024, 1 << 20] {
            let r = run(
                img.path(),
                ScanOptions {
                    block_bytes: block,
                    threads: Some(3),
                    ..Default::default()
                },
            );
            let mut got: Vec<(u64, u64)> = r
                .candidates
                .iter()
                .filter(|c| c.signature_id == "png")
                .map(|c| (c.offset, c.length))
                .collect();
            got.sort_unstable();
            match &baseline {
                None => baseline = Some(got),
                Some(b) => assert_eq!(&got, b, "block size {block} disagreed"),
            }
        }
        assert_eq!(baseline.as_ref().map(|b| b.len()), Some(8));
    }

    /// The scan must not modify the source. Same invariant as Milestone 1,
    /// asserted again here because this is new code that touches the device.
    #[test]
    fn scanning_does_not_modify_the_source() {
        use sha2::{Digest, Sha256};
        let img = image(&[(4096, png(16, 16))], 512 * 1024);
        let before = Sha256::digest(std::fs::read(img.path()).unwrap());
        let _ = run(img.path(), ScanOptions::default());
        let after = Sha256::digest(std::fs::read(img.path()).unwrap());
        assert_eq!(before, after, "the scan modified the image");
    }

    #[test]
    fn stats_report_the_scanner_denominator_separately() {
        let img = image(&[(1000, png(8, 8))], 512 * 1024);
        let r = run(img.path(), ScanOptions::default());
        let s = &r.stats;
        assert!(s.prefilter_hits >= s.header_matches);
        assert_eq!(
            s.header_matches,
            s.validated + s.rejected + not_validated(&r)
        );
        assert!(s.bytes_scanned >= 512 * 1024);
        assert!(
            s.cache_warning().is_some(),
            "a 512 KiB image is cache-resident"
        );
    }

    fn not_validated(r: &ScanResult) -> u64 {
        r.candidates
            .iter()
            .filter(|c| c.detail.contains("no validator"))
            .count() as u64
            + r.stats.suppressed_contained
    }

    /// Restricting the scan should narrow the prefilter, which is where the
    /// speed of a targeted carve comes from.
    #[test]
    fn restricting_to_one_format_takes_the_simd_path() {
        let img = image(&[(2048, png(8, 8))], 256 * 1024);
        let r = run(
            img.path(),
            ScanOptions {
                only: Some(vec!["png".into()]),
                ..Default::default()
            },
        );
        assert_eq!(r.stats.strategy, "memchr");
        assert_eq!(r.stats.signatures, 1);
        assert_eq!(r.candidates.len(), 1);
    }

    /// Two signatures describing the same bytes: the more specific one is the
    /// one reported.
    #[test]
    fn the_more_specific_signature_wins_at_the_same_offset() {
        // An MP4 whose brand is M4A matches `ftyp` (4 bytes at offset 4) and
        // `ftypM4A ` (8 bytes at offset 4).
        let mut f = 24u32.to_be_bytes().to_vec();
        f.extend_from_slice(b"ftypM4A ");
        f.extend_from_slice(&512u32.to_be_bytes());
        f.extend_from_slice(b"M4A isom");
        let mut v = f.clone();
        v.extend_from_slice(&40u32.to_be_bytes());
        v.extend_from_slice(b"moov");
        v.extend_from_slice(&[0x11; 32]);
        v.extend_from_slice(&200u32.to_be_bytes());
        v.extend_from_slice(b"mdat");
        v.extend_from_slice(&[0x22; 192]);

        let img = image(&[(8192, v.clone())], 64 * 1024);
        let r = run(img.path(), ScanOptions::default());

        let at: Vec<&Candidate> = r.candidates.iter().filter(|c| c.offset == 8192).collect();
        assert_eq!(
            at.len(),
            1,
            "expected one candidate for one file, got {:?}",
            at.iter().map(|c| &c.signature_id).collect::<Vec<_>>()
        );
        assert_eq!(
            at[0].signature_id, "m4a",
            "the eight-byte ftypM4A match should beat the four-byte ftyp one"
        );
        assert_eq!(at[0].length, v.len() as u64);
    }

    #[test]
    fn a_read_error_skips_the_block_rather_than_ending_the_scan() {
        // A device that fails one region but not the rest: the scan should
        // still find what lies beyond it.
        let p = png(8, 8);
        let img = image(&[(300_000, p)], 512 * 1024);
        let inner = rc_device::open(img.path(), None).expect("open");
        // Not vec![0..65_536]: clippy reads a single Range in a vec literal as
        // a likely typo for a range of vecs.
        let bad = vec![std::ops::Range {
            start: 0u64,
            end: 65_536u64,
        }];
        let faulty = testutil::FaultyDevice::new(inner, bad);
        let end = 512 * 1024;
        let r = scan(
            Arc::new(faulty),
            &db(),
            0,
            end,
            &ScanOptions {
                block_bytes: 64 * 1024,
                ..Default::default()
            },
        )
        .expect("scan should survive a read error");
        assert!(
            r.candidates.iter().any(|c| c.offset == 300_000),
            "the file past the bad region was not found"
        );
    }
}

/// A minimal temp-file helper so the tests do not add a dev-dependency.
#[cfg(test)]
mod tempfile_lite {
    use std::path::{Path, PathBuf};

    pub struct Temp(PathBuf);

    impl Temp {
        pub fn with_bytes(data: &[u8]) -> Temp {
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let p =
                std::env::temp_dir().join(format!("rc-carve-scan-{}-{n}.img", std::process::id()));
            std::fs::write(&p, data).expect("write scratch image");
            Temp(p)
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}
