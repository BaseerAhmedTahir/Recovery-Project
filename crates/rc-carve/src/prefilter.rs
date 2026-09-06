//! Finding the bytes worth looking at, quickly.
//!
//! The scanner's inner loop runs over every byte of the device, so its cost is
//! the throughput ceiling for the whole carve. Comparing 44 signatures against
//! every offset is hopeless; the standard answer, and what SPEC.md section
//! 5.5 asks for, is to search for each signature's first byte with a SIMD
//! `memchr` and only do the full comparison at a hit.
//!
//! That works beautifully for a narrow search and badly for a wide one, and
//! the difference is worth being explicit about because it decides the design.
//! `memchr` takes at most three needles per pass, so N distinct first bytes
//! means ceil(N/3) passes over the same block. Enough passes and one scalar
//! pass over a 256-entry lookup table wins instead.
//!
//! **Where that crossover falls was measured, and both guesses were wrong.**
//! The first version put it at three needles, reasoning that SIMD passes add
//! up quickly. Re-reasoning from a single measurement put it at nine. The
//! sweep says five: over 4 MiB in release, the table costs a flat ~3.5 ms
//! whatever the needle count, while the SIMD path runs ~1.0 ms for one needle
//! and crosses the table between five and six.
//!
//! ```text
//!    1 needles  simd 1.036ms  table 3.822ms
//!    3 needles  simd 1.851ms  table 3.572ms
//!    5 needles  simd 3.133ms  table 3.437ms   <- last win for SIMD
//!    6 needles  simd 3.645ms  table 3.539ms
//!   16 needles  simd 10.229ms table 3.506ms
//! ```
//!
//! `crossover_is_where_the_threshold_says_it_is` re-measures on whatever
//! machine runs it and fails if the constant stops matching reality. That test
//! is what caught the second wrong guess.
//!
//! (The first measurement was taken in a debug build and said the table was
//! 13x slower, which is also wrong: `Cargo.toml` sets `opt-level = 2` for
//! dependencies only, so it was comparing an optimised `memchr` against an
//! unoptimised table loop. Performance claims have to be measured in the
//! profile they will run in.)
//!
//! Three strategies, chosen by how many distinct bytes can begin a match:
//!
//! * **`memchr` / `memchr2` / `memchr3`** for one to three. This is the case
//!   that matters in practice - "find my deleted JPEGs" is one signature, one
//!   needle - and it runs at memory bandwidth.
//! * **Several `memchr3` passes** up to `MEMCHR_MAX_NEEDLES`.
//! * **A 256-entry table** beyond that, one scalar pass. The full 44-signature
//!   database lands here.
//!
//! Hit order is unspecified: the multi-pass strategy yields hits grouped by
//! needle rather than by position, and sorting them would cost more than the
//! passes save. Nothing downstream depends on the order - the scanner resolves
//! and de-duplicates candidates by offset regardless.

use crate::signature::{Signature, SignatureDb};
use memchr::{memchr, memchr2, memchr3};

/// Above this many distinct leading bytes, one table pass beats ceil(N/3)
/// memchr passes. Measured, not guessed - see the module docs and
/// `crossover_is_where_the_threshold_says_it_is`.
pub const MEMCHR_MAX_NEEDLES: usize = 5;

/// How to locate positions worth a full signature comparison.
#[derive(Clone, Debug)]
pub enum Prefilter {
    /// Nothing can be prefiltered - every offset must be considered. Only
    /// happens if every signature has a masked first byte.
    Everything,
    One(u8),
    Two(u8, u8),
    Three(u8, u8, u8),
    /// Four to `MEMCHR_MAX_NEEDLES` distinct bytes, scanned as several
    /// `memchr3` passes. Hits come out grouped by needle, not in order.
    Multi(Vec<u8>),
    /// `true` at index b means byte b can begin a match.
    Table(Box<[bool; 256]>),
}

impl Prefilter {
    /// Build the prefilter for a set of leading bytes.
    pub fn for_bytes(bytes: &[u8]) -> Prefilter {
        let mut distinct: Vec<u8> = bytes.to_vec();
        distinct.sort_unstable();
        distinct.dedup();
        match distinct.len() {
            0 => Prefilter::Everything,
            1 => Prefilter::One(distinct[0]),
            2 => Prefilter::Two(distinct[0], distinct[1]),
            3 => Prefilter::Three(distinct[0], distinct[1], distinct[2]),
            n if n <= MEMCHR_MAX_NEEDLES => Prefilter::Multi(distinct),
            _ => {
                let mut t = Box::new([false; 256]);
                for b in distinct {
                    t[b as usize] = true;
                }
                Prefilter::Table(t)
            }
        }
    }

    /// Is this byte worth a full comparison?
    pub fn matches(&self, b: u8) -> bool {
        match self {
            Prefilter::Everything => true,
            Prefilter::One(a) => b == *a,
            Prefilter::Two(a, c) => b == *a || b == *c,
            Prefilter::Three(a, c, d) => b == *a || b == *c || b == *d,
            Prefilter::Multi(v) => v.contains(&b),
            Prefilter::Table(t) => t[b as usize],
        }
    }

    /// Whether this uses the SIMD path. Reported by the scanner so a
    /// throughput number says which strategy produced it.
    pub fn is_simd(&self) -> bool {
        !matches!(self, Prefilter::Table(_) | Prefilter::Everything)
    }

    /// Distinct bytes this prefilter searches for. `None` when it searches for
    /// everything.
    pub fn needles(&self) -> Option<Vec<u8>> {
        match self {
            Prefilter::Everything => None,
            Prefilter::One(a) => Some(vec![*a]),
            Prefilter::Two(a, b) => Some(vec![*a, *b]),
            Prefilter::Three(a, b, c) => Some(vec![*a, *b, *c]),
            Prefilter::Multi(v) => Some(v.clone()),
            Prefilter::Table(t) => Some(
                (0..=255u8).filter(|b| t[*b as usize]).collect(),
            ),
        }
    }

    pub fn strategy(&self) -> &'static str {
        match self {
            Prefilter::Everything => "every-offset",
            Prefilter::One(_) => "memchr",
            Prefilter::Two(..) => "memchr2",
            Prefilter::Three(..) => "memchr3",
            Prefilter::Multi(_) => "memchr3-multipass",
            Prefilter::Table(_) => "byte-table",
        }
    }

    /// Call `f` with every offset in `hay` whose byte can begin a match.
    ///
    /// A callback rather than an iterator: the SIMD variants want to drive
    /// their own loop, and the borrow of `self` inside a returned iterator
    /// would have to be threaded through the scanner for no gain.
    pub fn for_each_hit(&self, hay: &[u8], mut f: impl FnMut(usize)) {
        match self {
            Prefilter::Everything => {
                for i in 0..hay.len() {
                    f(i);
                }
            }
            Prefilter::One(a) => {
                let mut base = 0;
                while let Some(p) = memchr(*a, &hay[base..]) {
                    f(base + p);
                    base += p + 1;
                }
            }
            Prefilter::Two(a, b) => {
                let mut base = 0;
                while let Some(p) = memchr2(*a, *b, &hay[base..]) {
                    f(base + p);
                    base += p + 1;
                }
            }
            Prefilter::Three(a, b, c) => {
                let mut base = 0;
                while let Some(p) = memchr3(*a, *b, *c, &hay[base..]) {
                    f(base + p);
                    base += p + 1;
                }
            }
            Prefilter::Multi(v) => {
                // ceil(n/3) passes. Hits arrive grouped by chunk rather than
                // in ascending order; see the module docs.
                for chunk in v.chunks(3) {
                    let (a, b, c) = (
                        chunk[0],
                        *chunk.get(1).unwrap_or(&chunk[0]),
                        *chunk.get(2).unwrap_or(&chunk[0]),
                    );
                    let mut base = 0;
                    while let Some(p) = memchr3(a, b, c, &hay[base..]) {
                        f(base + p);
                        base += p + 1;
                    }
                }
            }
            Prefilter::Table(t) => {
                for (i, b) in hay.iter().enumerate() {
                    if t[*b as usize] {
                        f(i);
                    }
                }
            }
        }
    }
}

/// A signature database arranged for scanning.
///
/// Maps each leading byte to the signatures it could start, so a prefilter hit
/// turns into a short list rather than a walk of all 44.
pub struct ScanIndex<'a> {
    pub prefilter: Prefilter,
    /// Indexed by leading byte value. Each entry lists `(signature, offset of
    /// the leading byte within the file, position in the source slice)`.
    ///
    /// The slice position is carried so the scanner's hot loop can record which
    /// signature matched with a `usize` instead of resolving a pointer against
    /// all 44 on every hit.
    by_byte: Vec<Vec<(&'a Signature, usize, usize)>>,
    /// Signatures whose first byte is masked and so cannot be prefiltered.
    /// These must be compared at every prefilter hit regardless of the byte.
    unfiltered: Vec<(&'a Signature, usize)>,
    max_span: usize,
}

impl<'a> ScanIndex<'a> {
    pub fn new(db: &'a SignatureDb) -> ScanIndex<'a> {
        Self::from_signatures(db.signatures.iter(), db.max_header_span())
    }

    pub fn from_signatures(
        sigs: impl Iterator<Item = &'a Signature>,
        max_span: usize,
    ) -> ScanIndex<'a> {
        let mut by_byte: Vec<Vec<(&Signature, usize, usize)>> = vec![Vec::new(); 256];
        let mut unfiltered = Vec::new();
        let mut bytes = Vec::new();

        for (i, s) in sigs.enumerate() {
            match s.prefilter_byte() {
                Some((b, off)) => {
                    by_byte[b as usize].push((s, off, i));
                    bytes.push(b);
                }
                // A masked leading byte cannot be searched for exactly.
                None => unfiltered.push((s, i)),
            }
        }

        // If anything is unfilterable we must look at every offset, because
        // there is no byte to search for that would find it.
        let prefilter = if unfiltered.is_empty() {
            Prefilter::for_bytes(&bytes)
        } else {
            Prefilter::Everything
        };

        ScanIndex {
            prefilter,
            by_byte,
            unfiltered,
            max_span,
        }
    }

    /// Lookahead the scanner must carry across a block boundary so a header
    /// straddling it is still found.
    pub fn max_header_span(&self) -> usize {
        self.max_span
    }

    pub fn strategy(&self) -> &'static str {
        self.prefilter.strategy()
    }

    /// Signatures that could begin at `hit`, given the byte there.
    ///
    /// `hit` is the position of the *leading byte*, which for a container
    /// format is not the start of the file - MP4's `ftyp` sits at offset 4,
    /// after the box size - so this yields the candidate's start offset too,
    /// along with the signature's position in the slice this index was built
    /// from.
    pub fn candidates_at<'b>(
        &'b self,
        hay: &'b [u8],
        hit: usize,
    ) -> impl Iterator<Item = (&'a Signature, usize, usize)> + 'b {
        let b = hay.get(hit).copied().unwrap_or(0);
        self.by_byte[b as usize]
            .iter()
            .copied()
            .chain(
                self.unfiltered
                    .iter()
                    .map(|(s, i)| (*s, s.header_offset, *i)),
            )
            .filter_map(move |(s, off, i)| {
                // The candidate begins `off` bytes before its leading byte.
                let start = hit.checked_sub(off)?;
                s.matches(hay.get(start..)?).then_some((s, start, i))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SignatureDb;

    /// Hit order is unspecified (the multi-pass strategy groups by needle), so
    /// every comparison here sorts first.
    fn hits(p: &Prefilter, hay: &[u8]) -> Vec<usize> {
        let mut v = Vec::new();
        p.for_each_hit(hay, |i| v.push(i));
        v.sort_unstable();
        v
    }

    fn noise(n: usize) -> Vec<u8> {
        let mut hay = Vec::with_capacity(n);
        let mut state = 0x2545_F491u32;
        for _ in 0..n {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            hay.push(state as u8);
        }
        hay
    }

    /// Every strategy must find exactly the same positions. They are chosen
    /// for speed, so a difference in *results* between them would be a bug
    /// that only shows up once a signature set crosses the threshold.
    #[test]
    fn all_strategies_agree_on_which_offsets_hit() {
        let hay = noise(8192);

        // Spans every strategy: memchr, memchr2, memchr3, the multi-pass tier,
        // and the table beyond MEMCHR_MAX_NEEDLES.
        for n in 1..=(MEMCHR_MAX_NEEDLES + 4) {
            let needles: Vec<u8> = (0..n).map(|i| (i as u8).wrapping_mul(31)).collect();
            let p = Prefilter::for_bytes(&needles);
            let got = hits(&p, &hay);
            let want: Vec<usize> = hay
                .iter()
                .enumerate()
                .filter(|(_, b)| needles.contains(b))
                .map(|(i, _)| i)
                .collect();
            assert_eq!(got, want, "strategy {} disagreed for {n} needles", p.strategy());
        }
    }

    #[test]
    fn strategy_is_chosen_by_needle_count() {
        assert_eq!(Prefilter::for_bytes(&[1]).strategy(), "memchr");
        assert_eq!(Prefilter::for_bytes(&[1, 2]).strategy(), "memchr2");
        assert_eq!(Prefilter::for_bytes(&[1, 2, 3]).strategy(), "memchr3");
        assert_eq!(
            Prefilter::for_bytes(&[1, 2, 3, 4]).strategy(),
            "memchr3-multipass"
        );

        let at_limit: Vec<u8> = (1..=MEMCHR_MAX_NEEDLES as u8).collect();
        assert_eq!(Prefilter::for_bytes(&at_limit).strategy(), "memchr3-multipass");
        let past_limit: Vec<u8> = (1..=MEMCHR_MAX_NEEDLES as u8 + 1).collect();
        assert_eq!(Prefilter::for_bytes(&past_limit).strategy(), "byte-table");

        // Duplicates collapse: a set of one distinct byte is still memchr.
        assert_eq!(Prefilter::for_bytes(&[7, 7, 7, 7]).strategy(), "memchr");
        assert_eq!(Prefilter::for_bytes(&[]).strategy(), "every-offset");
    }

    /// The multi-pass tier must not report a position twice, even though it
    /// makes several passes over the same buffer. Overlapping needle chunks
    /// would double-count, and a duplicated hit becomes a duplicated candidate.
    #[test]
    fn the_multipass_tier_reports_each_position_once() {
        let hay = noise(4096);
        for n in 4..=MEMCHR_MAX_NEEDLES {
            let needles: Vec<u8> = (0..n as u8).map(|i| i.wrapping_mul(17)).collect();
            let got = hits(&Prefilter::for_bytes(&needles), &hay);
            let mut dedup = got.clone();
            dedup.dedup();
            assert_eq!(got, dedup, "{n} needles produced a duplicate hit");
        }
    }

    #[test]
    fn matches_agrees_with_for_each_hit() {
        let needles = [0x00u8, 0x42, 0xFF, 0x89, 0x25];
        let p = Prefilter::for_bytes(&needles);
        for b in 0..=255u8 {
            assert_eq!(p.matches(b), needles.contains(&b), "byte {b:#04X}");
        }
    }

    /// A container format's leading byte is not at the start of the file, so a
    /// hit has to be translated back. MP4's `ftyp` is at offset 4.
    #[test]
    fn a_hit_resolves_to_the_candidate_start_not_the_hit_offset() {
        let db = SignatureDb::builtin().expect("builtin db");
        let idx = ScanIndex::new(&db);

        // 4-byte size, then ftyp, then a brand.
        let mut buf = 0x20u32.to_be_bytes().to_vec();
        buf.extend_from_slice(b"ftypisom");
        buf.extend_from_slice(&[0u8; 16]);

        let found: Vec<_> = idx
            .candidates_at(&buf, 4)
            .map(|(s, start, _)| (s.id.clone(), start))
            .collect();
        assert!(
            found.iter().any(|(id, start)| id == "mp4" && *start == 0),
            "mp4 should resolve to offset 0 from a hit at 4, got {found:?}"
        );
    }

    /// A leading byte at an offset that would put the candidate before the
    /// start of the buffer must not underflow.
    #[test]
    fn a_hit_too_close_to_the_start_is_dropped_not_wrapped() {
        let db = SignatureDb::builtin().expect("builtin db");
        let idx = ScanIndex::new(&db);
        let buf = b"ftypisom".to_vec();
        // A hit at 0 for a signature whose header_offset is 4 would need the
        // candidate to start at -4.
        let n = idx.candidates_at(&buf, 0).count();
        assert_eq!(n, 0, "should have found nothing rather than panicking");
    }

    #[test]
    fn the_builtin_database_uses_the_table_strategy() {
        let db = SignatureDb::builtin().expect("builtin db");
        let idx = ScanIndex::new(&db);
        // 44 signatures span far more than three distinct leading bytes, so a
        // full-database scan is expected to take the table path. If this ever
        // flips, the throughput characteristics change and the benchmark
        // numbers stop being comparable.
        assert_eq!(idx.strategy(), "byte-table");
        assert!(idx.max_header_span() >= 8);
    }

    /// Scanning for a single format should take the SIMD path - this is the
    /// case SPEC.md section 5.5 is describing, and the one users hit when
    /// they ask for one file type.
    #[test]
    fn a_single_format_scan_takes_the_simd_path() {
        let db = SignatureDb::builtin().expect("builtin db");
        let jpeg: Vec<_> = db.signatures.iter().filter(|s| s.id == "jpeg").collect();
        let idx = ScanIndex::from_signatures(jpeg.into_iter(), db.max_header_span());
        assert_eq!(idx.strategy(), "memchr");
        assert!(idx.prefilter.is_simd());
    }

    /// `MEMCHR_MAX_NEEDLES` is a performance claim, so measure it.
    ///
    /// Sweeps needle counts across the threshold, timing the multi-pass SIMD
    /// path against one table pass over the same buffer, and checks that the
    /// strategy this module would pick is the one that actually wins.
    ///
    /// **Run this in release.** A debug build applies `opt-level = 2` to
    /// dependencies only, which pits an optimised `memchr` against an
    /// unoptimised table loop and overstates the SIMD advantage roughly
    /// fourfold - that is how the threshold came to be wrong in the first
    /// place. Under a debug build this reports and does not assert.
    #[test]
    fn crossover_is_where_the_threshold_says_it_is() {
        use std::time::Instant;

        let hay = noise(4 << 20);
        let best_of = |p: &Prefilter| {
            let mut best = std::time::Duration::MAX;
            let mut hits = 0usize;
            for _ in 0..5 {
                let mut n = 0usize;
                let t = Instant::now();
                p.for_each_hit(&hay, |_| n += 1);
                best = best.min(t.elapsed());
                hits = n;
            }
            (best, hits)
        };

        let _ = best_of(&Prefilter::for_bytes(&[1])); // warm up

        eprintln!("\nprefilter over 4 MiB (best of 5), by distinct needle count:");
        let mut disagreements = Vec::new();
        for n in 1..=16usize {
            let needles: Vec<u8> = (0..n as u8).map(|i| i.wrapping_mul(37)).collect();

            // Force each strategy rather than letting for_bytes choose, so the
            // two are compared at the same needle count.
            let simd = if n <= MEMCHR_MAX_NEEDLES {
                Prefilter::for_bytes(&needles)
            } else {
                Prefilter::Multi(needles.clone())
            };
            let table = {
                let mut t = Box::new([false; 256]);
                for b in &needles {
                    t[*b as usize] = true;
                }
                Prefilter::Table(t)
            };

            let (ts, hs) = best_of(&simd);
            let (tt, ht) = best_of(&table);
            assert_eq!(hs, ht, "strategies disagreed on hit count at {n} needles");

            let simd_wins = ts < tt;
            let we_pick_simd = n <= MEMCHR_MAX_NEEDLES;
            eprintln!(
                "  {n:>2} needles  simd {ts:>10.3?}  table {tt:>10.3?}  \
                 winner {:<5}  we pick {}",
                if simd_wins { "simd" } else { "table" },
                if we_pick_simd { "simd" } else { "table" },
            );
            // Only flag a disagreement outside a 25% band; at the crossover the
            // two are by definition close, and a loaded runner should not fail
            // the build over a tie.
            let close = ts.min(tt) * 5 >= ts.max(tt) * 4;
            if simd_wins != we_pick_simd && !close {
                disagreements.push(format!(
                    "{n} needles: measured winner is {}, we pick {}",
                    if simd_wins { "simd" } else { "table" },
                    if we_pick_simd { "simd" } else { "table" },
                ));
            }
        }

        if cfg!(debug_assertions) {
            eprintln!(
                "  (debug build: reporting only. Dependencies are optimised and this \
                 crate is not, so the comparison is not meaningful.)"
            );
            return;
        }
        assert!(
            disagreements.is_empty(),
            "MEMCHR_MAX_NEEDLES = {MEMCHR_MAX_NEEDLES} does not match measurement:\n  {}",
            disagreements.join("\n  ")
        );
    }
}
