//! `rc-bifrag` - fragment reassembly (SPEC.md section 5.6).
//!
//! A carved file whose clusters are not contiguous on disk comes back from
//! the scanner as a truncated or spliced candidate. This crate tries to put it
//! back together.
//!
//! # The method, and why it is not Garfinkel's
//!
//! SPEC.md orders "bifragment gap carving" first: for a header and a footer,
//! search split points so the two halves validate. That handles exactly two
//! fragments. The first fragmented fixture's files turned out to be in 3, 3, 16
//! and 26 fragments - measured by locating every cluster in the image, not
//! assumed - so a bifragment carver would have recovered none of them.
//!
//! What this does instead generalises it to any number of fragments, under one
//! assumption: **a file's fragments are in ascending order on disk.** That is
//! what FAT's forward allocation produces, and the fixtures' recorded layouts
//! confirm it for every file. It is not universal - a file extended long after
//! it was written, or one on a heavily churned NTFS volume, can have a
//! fragment *before* its first one - and that case is not handled.
//!
//! Given that assumption a file is an increasing sequence of clusters, grown
//! one decision at a time: which cluster comes next? The physically next one
//! first, then each one further on, up to a window. Every candidate is judged
//! by the format's validator, and a wrong choice is undone by backtracking.
//!
//! # How a candidate is judged
//!
//! By the validator, through [`Outcome::ran_out`] and [`Outcome::length`] -
//! whether it stopped because the bytes ended or because it found something
//! that cannot belong, and how far into the bytes it can vouch for them.
//!
//! - **Contradicted** - the validator found something wrong once the candidate
//!   was appended. The path so far was not contradicted, so the candidate is
//!   what broke it. Rejected, *whatever the length did*. The first version of
//!   this crate asked only whether the length grew, and an H.264 sample whose
//!   tail ran 81 bytes into a cluster of filler made it grow.
//! - **Advances** - no contradiction, and a structural landmark (a restart
//!   marker, the end of a sample, a chunk checksum) now lies inside the
//!   candidate. Accepted.
//! - **Neutral** - no contradiction, no landmark. The physically next cluster
//!   is accepted on contiguity alone, tentatively: most of a file is
//!   contiguous. A cluster further on has no such claim, so a few clusters
//!   past it are appended to look for a landmark; without one it is rejected.
//!   That lookahead is what lets a JPEG with one restart marker per MCU row -
//!   several clusters apart - be reassembled at all.
//!
//! Every accepted cluster is a decision the search can return to. When no
//! candidate within the window continues the path, it backs up to the most
//! recent decision and tries the next candidate there.
//!
//! # The window deepens
//!
//! The search runs with a window of 16 clusters first, then 256, then the
//! configured maximum. A wide window costs validator calls at every fragment
//! boundary, and it is where a format that discriminates poorly finds most of
//! its false continuations: a JPEG restart marker has one chance in eight of
//! being in phase by accident, so 4096 candidates hold hundreds of them.
//! Fragments are usually close together, so the narrow search usually
//! suffices and the wide one is a last resort.
//!
//! # It is only as good as the validator
//!
//! Everything here reduces to the validator's power to *reject* foreign data,
//! and that differs sharply by format. H.264 NAL framing rejects another
//! file's cluster almost always; a JPEG restart marker's phase rejects it
//! seven times in eight. The reassembly results are reported per format and
//! per fixture for exactly that reason: see the tests.
//!
//! # What gets recorded
//!
//! SPEC.md asks that every reassembled file record how it was assembled, so
//! scoring can use it. [`Reassembly`] carries the pieces in order, the
//! validator calls and backtracks it took to find them, the window it needed,
//! and the validator's verdict on the result. A file stitched from sixteen
//! pieces after four hundred guesses is not the same evidence as one read
//! straight off the disk, and the score must be able to tell.

use rc_carve::validate::{self, Outcome, Status};
use rc_device::ReadOnlyDevice;

#[derive(Debug, thiserror::Error)]
pub enum BifragError {
    #[error("no validator named {0:?}; reassembly needs one to judge each splice")]
    NoValidator(String),
    #[error("reading the device: {0}")]
    Device(#[from] rc_device::DeviceError),
}

pub type Result<T> = std::result::Result<T, BifragError>;

#[derive(Clone, Debug)]
pub struct Options {
    /// Allocation unit of the filesystem the file lived on. Fragments start and
    /// end on these boundaries.
    pub cluster: u64,
    /// The widest search for the next fragment, in clusters past the end of
    /// the last one. The search deepens toward this rather than starting here.
    ///
    /// The gap between two fragments is whatever other files occupied, so it
    /// has no natural bound. This is a cost bound, not a model of disks.
    pub max_gap_clusters: u64,
    /// Longest file to assemble, in clusters. A safety bound.
    pub max_clusters: u64,
    /// Validator calls allowed for one attempt at one window. The search
    /// backtracks, and without a budget a format that discriminates poorly
    /// could search for a very long time.
    ///
    /// Each call re-validates the whole file so far, so the work is the budget
    /// times the file size - the cost that decides how long this can run. A
    /// validator that could resume from its last landmark would remove it; see
    /// docs/LIMITATIONS.md.
    pub budget: u64,
    /// Clusters a path may grow without the verified length moving before the
    /// branch is abandoned.
    ///
    /// Some files offer nothing to verify against until their last bytes - an
    /// MP4 whose moov box follows its mdat is the common case, since a recorder
    /// cannot write the index until it stops. Reassembling those by this method
    /// is guesswork, and this is what stops it from being expensive guesswork.
    pub max_unverified_clusters: u64,
    /// Window used when re-trying a cluster that was taken from the middle of a
    /// run the validator was vouching for.
    ///
    /// Such a cluster is rarely the mistake, so it does not deserve the full
    /// window on the way back up - which would multiply every backtrack by
    /// thousands of candidates.
    pub continuation_window: u64,
    /// Bytes validated per attempt. The honest cost measure: every call
    /// re-validates the whole file so far, and formats differ by two orders of
    /// magnitude in what one call costs - a 916 KiB PNG re-checksums every
    /// chunk, an MP4 walks a few hundred sample sizes.
    pub budget_bytes: u64,
    /// Clusters validated in one call when extending a run that is verifying.
    pub run_clusters: u64,
    /// Clusters past a non-adjacent candidate to append when the candidate
    /// alone neither verifies nor contradicts.
    pub lookahead_clusters: u64,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            cluster: 4096,
            // 16 MiB at 4 KiB clusters.
            max_gap_clusters: 4096,
            max_clusters: 1 << 16,
            budget: 200_000,
            // 4 GiB of validation: about four seconds for the slowest format
            // measured here.
            budget_bytes: 4 << 30,
            // 256 KiB with nothing verified.
            max_unverified_clusters: 64,
            continuation_window: 16,
            // 1 MiB per call: long enough that a contiguous file costs a
            // handful of calls, short enough that one bad call is cheap.
            run_clusters: 256,
            // 32 KiB. A JPEG restarting once per MCU row puts markers 10-15
            // KiB apart at camera resolutions; this covers that twice over.
            lookahead_clusters: 8,
        }
    }
}

/// The windows the search deepens through: 16, then 16 times wider, up to
/// `max`.
fn windows(max: u64) -> Vec<u64> {
    let mut out = Vec::new();
    let mut w = 16u64;
    while w < max {
        out.push(w);
        w = w.saturating_mul(16);
    }
    out.push(max.max(1));
    out
}

/// One contiguous run of the reassembled file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Piece {
    pub offset: u64,
    pub length: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    /// The file lay in one run on the disk. No reassembly was needed.
    Contiguous,
    /// Stitched from this many in-order fragments.
    Sequential { fragments: usize },
}

#[derive(Clone, Debug)]
pub struct Reassembly {
    /// The file's runs, in order.
    pub pieces: Vec<Piece>,
    pub method: Method,
    /// The validator's verdict on the assembled bytes. `Valid` means complete
    /// and internally consistent; anything else means reassembly stopped short
    /// and `pieces` is the furthest-verified path it found.
    pub status: Status,
    /// Bytes the validator vouches for.
    pub length: u64,
    /// Validator calls spent. The cost of the result, and part of its evidence.
    pub validator_calls: u64,
    /// Times the search abandoned a decision and tried another. A file found
    /// without backtracking is stronger evidence than one found after many.
    pub backtracks: u64,
    /// Bytes handed to the validator, summed over every call: the work this
    /// result cost.
    pub validated_bytes: u64,
    /// Candidate clusters refused on entropy before any validator saw them
    /// (SPEC.md 5.6.3). Only ever uniform filler in a high-entropy file, so
    /// it is a cost saving and never a verdict.
    pub prefiltered: u64,
    /// The search window, in clusters, of the attempt that produced this.
    pub window: u64,
    pub detail: String,
}

impl Reassembly {
    pub fn fragments(&self) -> usize {
        self.pieces.len()
    }
}

/// Reassemble the file whose header is at `header`.
///
/// `validator` is a validator id from `rc-carve`; without one there is nothing
/// to judge a splice by, so it is required rather than optional.
pub fn reassemble(
    dev: &dyn ReadOnlyDevice,
    validator: &str,
    header: u64,
    opts: &Options,
) -> Result<Reassembly> {
    if !validate::has_validator(validator) {
        return Err(BifragError::NoValidator(validator.to_string()));
    }
    let mut s = Search {
        dev,
        validator,
        opts,
        dev_end: dev.total_bytes(),
        calls: 0,
        bytes: 0,
        backtracks: 0,
        prefiltered: 0,
        best: None,
        attempt_start: 0,
        bytes_at_attempt_start: 0,
    };

    let mut last_window = 0;
    for window in windows(opts.max_gap_clusters) {
        last_window = window;
        match s.attempt(header, window)? {
            End::Done { chosen, length } => {
                return Ok(s.finish(&chosen, length, Status::Valid, window, String::new()));
            }
            End::Exhausted => continue,
            End::Stopped(why) => {
                return Ok(s.best_or_nothing(header, last_window, why));
            }
        }
    }
    let why = format!(
        "no path through windows up to {} clusters led to a complete file",
        opts.max_gap_clusters
    );
    Ok(s.best_or_nothing(header, last_window, why))
}

/// Read the reassembled file's bytes.
pub fn assemble(dev: &dyn ReadOnlyDevice, r: &Reassembly) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(r.length as usize);
    for p in &r.pieces {
        let mut buf = vec![0u8; p.length as usize];
        let n = dev.read_bytes_at(p.offset, &mut buf)?;
        out.extend_from_slice(&buf[..n]);
    }
    out.truncate(r.length as usize);
    Ok(out)
}

// ---------------------------------------------------------------------------

/// How one attempt at one window ended.
enum End {
    /// Complete: the validator says Valid.
    Done { chosen: Vec<u64>, length: u64 },
    /// Every path within this window was tried and none completed.
    Exhausted,
    /// Out of budget or past the size bound; wider windows would not help.
    Stopped(String),
}

/// The verdict on one candidate.
enum Verdict {
    Complete(u64),
    Accepted,
    Rejected,
}

/// A decision point: which cluster follows the first `base` of the path.
struct Frame {
    base: usize,
    /// The next candidate to try: 0 is the physically next cluster, `k` is `k`
    /// clusters further on.
    next_k: u64,
    /// How far this decision may look. A decision reached going forward gets
    /// the attempt's window; one re-opened in the middle of a verified run gets
    /// the narrower continuation window.
    window: u64,
}

/// The path being grown: clusters, and the bytes they spell.
#[derive(Default)]
struct Path {
    chosen: Vec<u64>,
    /// `buf.len()` after each chosen cluster, so a backtrack can truncate.
    ends: Vec<usize>,
    /// How far the validator vouched when each cluster was accepted.
    verified: Vec<u64>,
    /// Clusters taken since the verified length last moved, per position.
    stale: Vec<u64>,
    /// Lowest per-cluster entropy in the path so far, per position.
    min_entropy: Vec<f32>,
    buf: Vec<u8>,
}

/// Shannon entropy of a cluster, in bits per byte.
///
/// Compressed media runs 7.8 to 8.0; a run of one repeated byte is 0. Used only
/// to refuse a candidate that could not belong to the file at all, never to
/// accept one.
fn entropy(bytes: &[u8]) -> f32 {
    if bytes.is_empty() {
        return 0.0;
    }
    let mut counts = [0u32; 256];
    for &b in bytes {
        counts[b as usize] += 1;
    }
    let n = bytes.len() as f32;
    -counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f32 / n;
            p * p.log2()
        })
        .sum::<f32>()
}

impl Path {
    fn len(&self) -> usize {
        self.chosen.len()
    }
    fn last(&self) -> u64 {
        *self.chosen.last().expect("a path always holds its header")
    }
    fn push(&mut self, at: u64, bytes: &[u8], verified: u64) {
        self.chosen.push(at);
        self.buf.extend_from_slice(bytes);
        self.ends.push(self.buf.len());
        let previous = self.verified.last().copied().unwrap_or(0);
        let v = previous.max(verified);
        let stale = if v > previous {
            0
        } else {
            self.stale.last().copied().unwrap_or(0) + 1
        };
        self.verified.push(v);
        self.stale.push(stale);
        let e = entropy(bytes);
        let lowest = self
            .min_entropy
            .last()
            .copied()
            .map_or(e, |m| if e < m { e } else { m });
        self.min_entropy.push(lowest);
    }
    fn truncate(&mut self, n: usize) {
        self.chosen.truncate(n);
        self.ends.truncate(n);
        self.verified.truncate(n);
        self.stale.truncate(n);
        self.min_entropy.truncate(n);
        self.buf.truncate(self.ends.last().copied().unwrap_or(0));
    }
    fn stale(&self) -> u64 {
        self.stale.last().copied().unwrap_or(0)
    }
    /// Every cluster of the file so far is high-entropy, so a uniform candidate
    /// cannot be part of it.
    fn all_high_entropy(&self) -> bool {
        self.min_entropy.last().copied().unwrap_or(0.0) > 7.0
    }
}

struct Search<'a> {
    dev: &'a dyn ReadOnlyDevice,
    validator: &'a str,
    opts: &'a Options,
    dev_end: u64,
    calls: u64,
    /// Bytes handed to the validator, summed over calls.
    bytes: u64,
    backtracks: u64,
    prefiltered: u64,
    /// The furthest-verified path seen: reported if nothing completes.
    best: Option<(Vec<u64>, u64, Status, String)>,
    /// `calls` when the current attempt began; the budget is per attempt.
    attempt_start: u64,
    /// `bytes` when the current attempt began.
    bytes_at_attempt_start: u64,
}

impl Search<'_> {
    fn read(&self, at: u64, clusters: u64) -> Result<Vec<u8>> {
        let len = (self.opts.cluster * clusters).min(self.dev_end.saturating_sub(at));
        let mut buf = vec![0u8; len as usize];
        let n = self.dev.read_bytes_at(at, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    /// One validator call, or `None` when the budget is spent.
    fn check(&mut self, data: &[u8]) -> Option<Outcome> {
        if self.calls - self.attempt_start >= self.opts.budget
            || self.bytes - self.bytes_at_attempt_start >= self.opts.budget_bytes
        {
            return None;
        }
        self.calls += 1;
        self.bytes += data.len() as u64;
        Some(validate::validate(self.validator, data).expect("presence checked at entry"))
    }

    fn remember(&mut self, path: &Path, o: &Outcome) {
        // A contradiction is not progress, whatever length it reports.
        if !o.ran_out && o.status != Status::Valid {
            return;
        }
        // MSRV 1.80: Option::is_none_or is 1.82.
        let better = self
            .best
            .as_ref()
            .map_or(true, |(_, len, _, _)| o.length > *len);
        if better {
            self.best = Some((path.chosen.clone(), o.length, o.status, o.detail.clone()));
        }
    }

    fn attempt(&mut self, header: u64, window: u64) -> Result<End> {
        let c = self.opts.cluster;
        self.attempt_start = self.calls;
        self.bytes_at_attempt_start = self.bytes;
        let mut path = Path::default();

        let head = self.read(header, 1)?;
        let Some(o) = self.check(&head) else {
            return Ok(End::Stopped("validator budget spent".into()));
        };
        if o.status == Status::Valid && o.length <= head.len() as u64 {
            path.push(header, &head, o.length);
            return Ok(End::Done {
                chosen: path.chosen,
                length: o.length,
            });
        }
        if !o.ran_out {
            // Nothing to extend: the header cluster is already contradicted.
            self.remember(&path, &o);
            return Ok(End::Stopped(format!(
                "the header cluster itself does not validate: {}",
                o.detail
            )));
        }
        path.push(header, &head, o.length);
        self.remember(&path, &o);

        let mut stack: Vec<Frame> = Vec::new();
        // Contiguous first (SPEC.md 5.6.1): run forward from the header.
        if let Some(end) = self.gallop(&mut path, &mut stack)? {
            return Ok(end);
        }

        loop {
            if path.len() as u64 >= self.opts.max_clusters {
                return Ok(End::Stopped(format!(
                    "reached the {}-cluster size bound",
                    self.opts.max_clusters
                )));
            }
            // Extend the path, unless this branch has gone too far with nothing
            // verified - in which case fall through to the candidate loop, which
            // abandons it and tries the next candidate at the last decision.
            if path.stale() < self.opts.max_unverified_clusters {
                stack.push(Frame {
                    base: path.len(),
                    next_k: 0,
                    window,
                });
            } else if stack.is_empty() {
                return Ok(End::Stopped(format!(
                    "nothing verified in {} clusters from the header; this format                      offers no landmark to reassemble by",
                    path.stale()
                )));
            }

            // Try candidates for the top decision; back up when it runs out.
            loop {
                let Some(top) = stack.last_mut() else {
                    return Ok(End::Exhausted);
                };
                path.truncate(top.base);
                if top.next_k > top.window {
                    stack.pop();
                    self.backtracks += 1;
                    continue;
                }
                let k = top.next_k;
                top.next_k += 1;
                let stop = top.window + 1;
                let Some(cand) = path.last().checked_add(c * (k + 1)) else {
                    top.next_k = stop;
                    continue;
                };
                if cand >= self.dev_end {
                    top.next_k = stop;
                    continue;
                }
                let before = path.len();
                match self.consider(&mut path, cand, k)? {
                    None => return Ok(End::Stopped("validator budget spent".into())),
                    Some(Verdict::Rejected) => continue,
                    Some(Verdict::Complete(length)) => {
                        return Ok(End::Done {
                            chosen: path.chosen,
                            length,
                        });
                    }
                    Some(Verdict::Accepted) => {
                        // Clusters taken past the candidate by the lookahead
                        // were each a "physically next" decision.
                        let narrow = window.min(self.opts.continuation_window);
                        for base in before + 1..path.len() {
                            stack.push(Frame {
                                base,
                                next_k: 1,
                                window: narrow,
                            });
                        }
                        match self.gallop(&mut path, &mut stack)? {
                            Some(end) => return Ok(end),
                            None => break,
                        }
                    }
                }
            }
        }
    }

    /// Judge `cand` as the next cluster; on acceptance, append it (and any
    /// clusters the lookahead verified) to `path`. `None` when out of budget.
    fn consider(&mut self, path: &mut Path, cand: u64, k: u64) -> Result<Option<Verdict>> {
        let had = path.buf.len() as u64;
        let one = self.read(cand, 1)?;
        // SPEC.md 5.6.3: a sharp entropy transition marks a fragment
        // boundary. Used here only in its one unambiguous form - a file whose
        // every cluster is compressed media cannot continue into a cluster of
        // one repeated byte - so it saves validator calls over filler and
        // decides nothing. Against gaps holding other real media, where every
        // cluster is high-entropy, it never fires.
        if path.all_high_entropy() && entropy(&one) < 1.0 {
            self.prefiltered += 1;
            return Ok(Some(Verdict::Rejected));
        }
        let mut trial = path.buf.clone();
        trial.extend_from_slice(&one);
        let Some(o) = self.check(&trial) else {
            return Ok(None);
        };
        if o.status == Status::Valid && o.length <= trial.len() as u64 {
            path.push(cand, &one, o.length);
            return Ok(Some(Verdict::Complete(o.length)));
        }
        if !o.ran_out {
            // The path was consistent before this cluster and is not after it.
            return Ok(Some(Verdict::Rejected));
        }
        if o.length > had || k == 0 {
            // A landmark inside the candidate, or the physically next cluster
            // taken on contiguity alone.
            path.push(cand, &one, o.length);
            self.remember(path, &o);
            return Ok(Some(Verdict::Accepted));
        }

        // A non-adjacent cluster with no evidence either way. Look further.
        let run = self.read(cand, self.opts.lookahead_clusters)?;
        if run.len() <= one.len() {
            return Ok(Some(Verdict::Rejected));
        }
        let mut trial = path.buf.clone();
        trial.extend_from_slice(&run);
        let Some(o) = self.check(&trial) else {
            return Ok(None);
        };
        let valid = o.status == Status::Valid && o.length <= trial.len() as u64;
        if !valid && o.length <= had {
            return Ok(Some(Verdict::Rejected));
        }
        // Take every cluster of the run the verified length covers - all of
        // each, or, when complete, up to the one it ends in.
        let c = self.opts.cluster as usize;
        let mut off = 0usize;
        while off < run.len() {
            let end = (off + c).min(run.len());
            let covered = if valid {
                had + (off as u64) < o.length
            } else {
                had + end as u64 <= o.length
            };
            if !covered {
                break;
            }
            path.push(cand + off as u64, &run[off..end], o.length);
            off = end;
        }
        if valid {
            if path.buf.len() as u64 >= o.length {
                return Ok(Some(Verdict::Complete(o.length)));
            }
            return Ok(Some(Verdict::Rejected));
        }
        self.remember(path, &o);
        Ok(Some(Verdict::Accepted))
    }

    /// Extend the path straight on while the validator keeps vouching for
    /// whole clusters, `run_clusters` at a time. Each cluster taken is pushed
    /// as a decision the search can return to. `Some` ends the attempt.
    fn gallop(&mut self, path: &mut Path, stack: &mut Vec<Frame>) -> Result<Option<End>> {
        let c = self.opts.cluster;
        loop {
            let Some(start) = path.last().checked_add(c) else {
                return Ok(None);
            };
            if start >= self.dev_end {
                return Ok(None);
            }
            let run = self.read(start, self.opts.run_clusters)?;
            let had = path.buf.len() as u64;
            let mut trial = path.buf.clone();
            trial.extend_from_slice(&run);
            let Some(o) = self.check(&trial) else {
                return Ok(Some(End::Stopped("validator budget spent".into())));
            };
            let valid = o.status == Status::Valid && o.length <= trial.len() as u64;
            // A contradiction vouches for nothing. Its length can even be the
            // file's full size - a PNG whose chunks all parse but whose
            // checksums fail knows exactly how long it was meant to be - and
            // taking that as verified accepted 29 clusters of filler as a
            // whole file.
            if !valid && !o.ran_out {
                return Ok(None);
            }

            let mut taken = 0u64;
            let mut off = 0usize;
            while off < run.len() {
                let end = (off + c as usize).min(run.len());
                let covered = if valid {
                    had + (off as u64) < o.length
                } else {
                    // Only clusters wholly inside what the validator vouches
                    // for. The cluster the verified length ends in is left to
                    // be judged on its own: a sample's tail can lie in it
                    // whatever the cluster holds.
                    had + end as u64 <= o.length
                };
                if !covered {
                    break;
                }
                // Even a vouching validator does not get to walk into filler:
                // a file whose every cluster is compressed media does not
                // continue into 4 KiB of one repeated byte (SPEC.md 5.6.3).
                if path.all_high_entropy() && entropy(&run[off..end]) < 1.0 {
                    self.prefiltered += 1;
                    break;
                }
                stack.push(Frame {
                    base: path.len(),
                    next_k: 1,
                    window: self.opts.continuation_window,
                });
                path.push(start + off as u64, &run[off..end], o.length);
                taken += 1;
                off = end;
            }
            if valid {
                if path.buf.len() as u64 >= o.length {
                    return Ok(Some(End::Done {
                        chosen: path.chosen.clone(),
                        length: o.length,
                    }));
                }
                // The validator walked to the end of a file through a cluster
                // this refused - filler. Its verdict covers bytes that are not
                // part of the path, so it is not a result: stop here and let
                // the search look for the real continuation past the filler.
                // Returning it anyway reported a JPEG as Valid at 198254 bytes
                // with 32768 bytes of pieces behind it.
                return Ok(None);
            }
            if taken > 0 {
                self.remember(path, &o);
            }
            if taken < self.opts.run_clusters || !o.ran_out {
                return Ok(None);
            }
        }
    }

    fn best_or_nothing(&self, header: u64, window: u64, why: String) -> Reassembly {
        match &self.best {
            Some((chosen, length, status, detail)) => {
                let status = if *status == Status::Valid {
                    Status::Partial
                } else {
                    *status
                };
                self.finish(
                    chosen,
                    *length,
                    status,
                    window,
                    format!("{why}; best path verified {length} bytes ({detail})"),
                )
            }
            None => self.finish(&[header], 0, Status::Rejected, window, why),
        }
    }

    /// Collapse chosen clusters into runs and describe the result.
    fn finish(
        &self,
        chosen: &[u64],
        length: u64,
        status: Status,
        window: u64,
        detail: String,
    ) -> Reassembly {
        let cluster = self.opts.cluster;
        let mut pieces: Vec<Piece> = Vec::new();
        for &start in chosen {
            match pieces.last_mut() {
                Some(p) if p.offset + p.length == start => p.length += cluster,
                _ => pieces.push(Piece {
                    offset: start,
                    length: cluster,
                }),
            }
        }
        // The last run ends where the verified bytes end, not at a cluster edge.
        let held: u64 = pieces.iter().map(|p| p.length).sum();
        let length = length.min(held);
        let mut remaining = length;
        for p in pieces.iter_mut() {
            if remaining < p.length {
                p.length = remaining;
            }
            remaining = remaining.saturating_sub(p.length);
        }
        pieces.retain(|p| p.length > 0);

        let method = if pieces.len() <= 1 {
            Method::Contiguous
        } else {
            Method::Sequential {
                fragments: pieces.len(),
            }
        };
        Reassembly {
            pieces,
            method,
            status,
            length,
            validator_calls: self.calls,
            validated_bytes: self.bytes,
            backtracks: self.backtracks,
            prefiltered: self.prefiltered,
            window,
            detail,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_deepen_by_sixteen_up_to_the_maximum() {
        assert_eq!(windows(4096), vec![16, 256, 4096]);
        assert_eq!(windows(1000), vec![16, 256, 1000]);
        assert_eq!(windows(16), vec![16]);
        assert_eq!(windows(8), vec![8]);
    }
}
