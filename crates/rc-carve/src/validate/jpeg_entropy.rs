//! Huffman decoding of a sequential JPEG scan: SPEC.md section 5.6 item 5,
//! "decode until failure".
//!
//! # Why a marker walk is not enough
//!
//! The JPEG validator's entropy scan checks what markers can tell it: that
//! restart markers cycle RST0..RST7 in order, and that no more bytes pass
//! between two of them than the declared interval could possibly hold. Both
//! catch data that has been *reordered* or *lost*.
//!
//! Neither catches data that has been *inserted*, and insertion is exactly what
//! a fragmented file looks like. When a file is split across two fragments,
//! whatever occupies the gap sits in the middle of its entropy stream, and the
//! file's own data continues on the far side - so the next restart marker
//! carries precisely the phase the sequence expects. The gap is invisible.
//! Measured on the fragmented fixtures: a JPEG with a restart interval of 50
//! MCUs came back Valid at 198254 bytes for a 99950-byte file, having swallowed
//! 32 KiB of filler twice over, and a JPEG with no restart interval at all came
//! back Valid at 267933 bytes for 136861.
//!
//! The byte bound cannot close that. It is derived from the format's worst case
//! (512 bytes per 8x8 block) because a validator may not reject a legal file,
//! while real data runs 10 to 30 bytes per block. That leaves a factor of
//! twenty of slack for filler to hide in, and at an interval of 50 MCUs the
//! bound is 153600 bytes: six clusters of anything at all.
//!
//! # What decoding adds
//!
//! Entropy-coded data is self-describing in a way byte scanning cannot see.
//! Every block is a run of Huffman codes from a table the file carries, and the
//! number of blocks between two restart markers is fixed by the frame's
//! dimensions and the restart interval. So:
//!
//! - a code that is not in the table is not this file's data;
//! - a zero run reaching past the 64th coefficient is not a block;
//! - a restart marker arriving after the wrong number of MCUs means bytes were
//!   added or removed since the last one - which is what a gap is;
//! - the scan ending after the wrong number of MCUs means the same.
//!
//! That last pair is the point. An inserted gap changes the MCU count between
//! two markers, and nothing else in this file can be checked against it.
//!
//! It also gives a reassembler something it had no other way to get: progress
//! *within* a restart interval. Every decoded MCU ends at a byte offset, so a
//! file with no restart markers at all still reports how far its entropy data
//! decodes cleanly.
//!
//! # What it does not do
//!
//! No dequantisation, no inverse DCT, no upsampling: coefficients are decoded
//! and range-checked, then dropped. This is not a picture decoder and cannot
//! say whether an image looks right - only whether the entropy coding is this
//! file's.
//!
//! Sequential Huffman frames only (SOF0 baseline, SOF1 extended). Progressive
//! scans code a spectral band of each block across several passes with their
//! own end-of-band run coding, lossless frames code differences rather than
//! blocks, and arithmetic coding is a different entropy coder entirely. Each of
//! those returns [`ScanResult::NotApplicable`] and leaves the byte-level checks
//! in charge, which is an honest gap rather than a wrong answer.

/// One component of the frame header.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Component {
    pub id: u8,
    pub h: u8,
    pub v: u8,
}

/// What SOF declared.
#[derive(Clone, Debug)]
pub(crate) struct Frame {
    pub precision: u8,
    pub width: u16,
    pub height: u16,
    pub components: Vec<Component>,
    /// False for progressive, lossless and arithmetic frames.
    pub sequential_huffman: bool,
}

/// One component of a scan header, with the tables it selects.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ScanComponent {
    pub id: u8,
    pub dc_table: u8,
    pub ac_table: u8,
}

/// The Huffman tables in force, by class and id.
#[derive(Clone, Default)]
pub(crate) struct Tables {
    pub dc: [Option<Huffman>; 4],
    pub ac: [Option<Huffman>; 4],
}

/// A canonical Huffman table in the form T.81 B.2.4.2 describes: for each code
/// length, the smallest and largest code and where its values start.
#[derive(Clone)]
pub(crate) struct Huffman {
    mincode: [i32; 17],
    maxcode: [i32; 17],
    valptr: [usize; 17],
    values: Vec<u8>,
    /// The next eight bits, straight to (code length, value) for every code
    /// eight bits or shorter. Longer codes have a zero length here and take the
    /// bit-at-a-time path.
    ///
    /// Reassembly re-decodes a file's whole prefix for every candidate cluster
    /// it weighs, so this is the difference between measuring a fixture in
    /// seconds and in minutes. Most codes in a JPEG are short: the standard
    /// tables put every DC code and the common AC codes inside eight bits.
    short: [(u8, u8); 256],
}

impl Huffman {
    /// Build from a DHT payload: sixteen code-length counts, then the values.
    ///
    /// Rejects a table whose codes cannot fit their lengths (the Kraft
    /// inequality): such a table is undecodable, so a file carrying one is not
    /// a file being read correctly.
    pub(crate) fn new(counts: &[u8; 16], values: &[u8]) -> Option<Huffman> {
        let total: usize = counts.iter().map(|&c| c as usize).sum();
        if total == 0 || total != values.len() || total > 256 {
            return None;
        }
        let mut mincode = [0i32; 17];
        let mut maxcode = [-1i32; 17];
        let mut valptr = [0usize; 17];
        let mut code = 0i32;
        let mut k = 0usize;
        for len in 1..=16usize {
            let n = counts[len - 1] as usize;
            valptr[len] = k;
            mincode[len] = code;
            if n > 0 {
                code += n as i32;
                maxcode[len] = code - 1;
                k += n;
            }
            if code > (1 << len) {
                return None; // over-subscribed: more codes than the length holds
            }
            code <<= 1;
        }
        // Fill the short-code lookup: every eight-bit prefix that begins with
        // this code maps to it.
        let mut short = [(0u8, 0u8); 256];
        let mut code = 0u32;
        let mut k = 0usize;
        for (len, &n) in counts.iter().enumerate().map(|(i, c)| (i + 1, c)) {
            for _ in 0..n {
                if len <= 8 {
                    let shift = 8 - len;
                    let base = (code << shift) as usize;
                    for e in short.iter_mut().skip(base).take(1usize << shift) {
                        *e = (len as u8, values[k]);
                    }
                }
                code += 1;
                k += 1;
            }
            code <<= 1;
        }
        Some(Huffman {
            mincode,
            maxcode,
            valptr,
            values: values.to_vec(),
            short,
        })
    }
}

/// What lies at the reader's position once it is byte-aligned.
#[derive(Debug, PartialEq, Eq)]
enum Next {
    /// A marker: entropy data has ended.
    Marker(u8),
    /// More entropy data.
    Data,
    /// The buffer ended.
    End,
}

/// Bits out of entropy-coded data, with FF 00 unstuffed.
///
/// Bits are held in the low `count` bits of `bits` and taken from the top of
/// those, so whole bytes can be buffered ahead and eight bits peeked at once.
/// Filling stops at a marker without consuming it, which is what lets the
/// decoder tell padding from data at the end of a restart interval.
struct BitReader<'d> {
    d: &'d [u8],
    /// Next byte to read.
    at: usize,
    bits: u32,
    count: u8,
    /// Where the marker that ended the data begins, once one is reached.
    marker_at: Option<usize>,
    /// One past the last byte consumed as entropy data.
    consumed_to: usize,
}

impl<'d> BitReader<'d> {
    fn new(d: &'d [u8], at: usize) -> BitReader<'d> {
        BitReader {
            d,
            at,
            bits: 0,
            count: 0,
            marker_at: None,
            consumed_to: at,
        }
    }

    /// Buffer bytes of entropy data until at least `want` bits are held, or a
    /// marker or the end of the buffer stops it.
    ///
    /// A loop, not recursion: a run of fill bytes can be as long as the buffer,
    /// and this parses data off a damaged disk.
    fn fill(&mut self, want: u8) -> bool {
        while self.count < want {
            if self.marker_at.is_some() {
                return false;
            }
            let Some(&b) = self.d.get(self.at) else {
                return false;
            };
            let byte = if b != 0xFF {
                self.at += 1;
                b
            } else {
                match self.d.get(self.at + 1) {
                    // A stuffed FF: one literal byte from two bytes of stream.
                    Some(0x00) => {
                        self.at += 2;
                        0xFF
                    }
                    // Fill bytes before a marker are legal padding.
                    Some(0xFF) => {
                        self.at += 1;
                        continue;
                    }
                    Some(_) => {
                        self.marker_at = Some(self.at);
                        return false;
                    }
                    None => return false,
                }
            };
            self.consumed_to = self.at;
            self.bits = (self.bits << 8) | byte as u32;
            self.count += 8;
        }
        true
    }

    /// One bit, or `None` at a marker or the end of the buffer.
    fn bit(&mut self) -> Option<u32> {
        if !self.fill(1) {
            return None;
        }
        self.count -= 1;
        Some((self.bits >> self.count) & 1)
    }

    fn receive(&mut self, n: u8) -> Option<u32> {
        if n == 0 {
            return Some(0);
        }
        if n <= 24 && self.fill(n) {
            self.count -= n;
            return Some((self.bits >> self.count) & ((1u32 << n) - 1));
        }
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Some(v)
    }

    fn decode(&mut self, t: &Huffman) -> Option<u8> {
        // The common case: the code is eight bits or shorter and the buffer
        // holds them.
        if self.fill(8) {
            let peek = ((self.bits >> (self.count - 8)) & 0xFF) as usize;
            let (len, value) = t.short[peek];
            if len > 0 {
                self.count -= len;
                return Some(value);
            }
        }
        let mut code = self.bit()? as i32;
        for len in 1..=16usize {
            if t.maxcode[len] >= 0 && code <= t.maxcode[len] && code >= t.mincode[len] {
                let idx = t.valptr[len] + (code - t.mincode[len]) as usize;
                return t.values.get(idx).copied();
            }
            code = (code << 1) | self.bit()? as i32;
        }
        None
    }

    /// The bits left unread, which a restart marker or the end of a scan must
    /// find set: T.81 2.10 pads a scan to the byte with 1s.
    ///
    /// Eight or more bits left is a whole byte of something else, not padding,
    /// and says so.
    fn padding_is_ones(&self) -> bool {
        if self.count == 0 {
            return true;
        }
        if self.count >= 8 {
            return false;
        }
        let mask = (1u32 << self.count) - 1;
        (self.bits & mask) == mask
    }

    /// Discard the padding bits and report what follows. Bytes already
    /// buffered were consumed from the stream, so this only looks from `at`.
    fn align(&mut self) -> Next {
        self.count = 0;
        self.bits = 0;
        if let Some(m) = self.marker_at {
            return match self.d.get(m + 1) {
                Some(&ty) => Next::Marker(ty),
                None => Next::End,
            };
        }
        let mut i = self.at;
        while let Some(&b) = self.d.get(i) {
            if b != 0xFF {
                return Next::Data;
            }
            match self.d.get(i + 1) {
                Some(0x00) => return Next::Data,
                Some(0xFF) => i += 1,
                Some(&ty) => {
                    self.marker_at = Some(i);
                    return Next::Marker(ty);
                }
                None => return Next::End,
            }
        }
        Next::End
    }

    /// Step past the marker the reader is sitting on.
    fn take_marker(&mut self) {
        if let Some(m) = self.marker_at {
            self.at = m + 2;
            self.marker_at = None;
            self.bits = 0;
            self.count = 0;
            self.consumed_to = self.at;
        }
    }

    fn at_marker(&self) -> bool {
        self.marker_at.is_some()
    }

    /// A whole byte still unread where a scan or interval should have ended.
    /// Padding is less than a byte by definition, so this is data that does not
    /// belong - the signature of bytes inserted since the last marker.
    fn holds_a_whole_byte(&self) -> bool {
        self.count >= 8
    }
}

/// Extend a magnitude-category value to its signed coefficient (T.81 F.2.2.1).
fn extend(v: u32, s: u8) -> i32 {
    if s == 0 {
        return 0;
    }
    let half = 1i32 << (s - 1);
    let v = v as i32;
    if v < half {
        v - (1 << s) + 1
    } else {
        v
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ScanResult {
    /// The scan decoded to exactly the MCUs the frame calls for and stopped at
    /// a marker. `end` is that marker's first byte.
    Complete {
        end: usize,
        mcus: u64,
        restarts: u64,
    },
    /// The data ended mid-scan. `verified_to` is the end of the last MCU that
    /// decoded cleanly - a real floor, not the buffer size.
    Truncated { verified_to: usize, mcus: u64 },
    /// Something in the entropy data cannot belong to this file.
    Broken {
        verified_to: usize,
        mcus: u64,
        why: String,
    },
    /// Not a mode this decodes.
    NotApplicable,
}

/// Decode one scan's entropy-coded data, which begins at `start`.
pub(crate) fn decode_scan(
    d: &[u8],
    start: usize,
    frame: &Frame,
    scan: &[ScanComponent],
    tables: &Tables,
    restart_interval: u64,
) -> ScanResult {
    if !frame.sequential_huffman || scan.is_empty() || frame.components.is_empty() {
        return ScanResult::NotApplicable;
    }
    let hmax = frame
        .components
        .iter()
        .map(|c| c.h)
        .max()
        .unwrap_or(1)
        .max(1) as u64;
    let vmax = frame
        .components
        .iter()
        .map(|c| c.v)
        .max()
        .unwrap_or(1)
        .max(1) as u64;
    let w = frame.width as u64;
    let h = frame.height as u64;

    // Each scan component, with its sampling and its two tables.
    let mut plan: Vec<(Component, &Huffman, &Huffman)> = Vec::new();
    for sc in scan {
        let Some(comp) = frame.components.iter().find(|c| c.id == sc.id) else {
            return ScanResult::NotApplicable;
        };
        let (Some(dc), Some(ac)) = (
            tables.dc.get(sc.dc_table as usize).and_then(|t| t.as_ref()),
            tables.ac.get(sc.ac_table as usize).and_then(|t| t.as_ref()),
        ) else {
            // A scan whose tables were never defined is not decodable, and
            // saying so beats guessing.
            return ScanResult::NotApplicable;
        };
        plan.push((*comp, dc, ac));
    }

    // A single-component scan codes one block per MCU over that component's own
    // block grid; an interleaved scan codes h*v blocks each over the frame's
    // MCU grid (T.81 A.1.1, A.2.2).
    let (total_mcus, blocks): (u64, Vec<(usize, u64)>) = if plan.len() == 1 {
        let c = plan[0].0;
        let bw = (w * c.h as u64).div_ceil(hmax).div_ceil(8);
        let bh = (h * c.v as u64).div_ceil(vmax).div_ceil(8);
        (bw * bh, vec![(0, 1)])
    } else {
        let mcus = w.div_ceil(8 * hmax) * h.div_ceil(8 * vmax);
        let per = plan
            .iter()
            .enumerate()
            .map(|(i, (c, _, _))| (i, c.h as u64 * c.v as u64))
            .collect();
        (mcus, per)
    };
    if total_mcus == 0 {
        return ScanResult::NotApplicable;
    }

    // Coefficient bounds from the sample precision. The DC coefficient of an
    // 8-bit block is at most 8 * 127 before quantisation, and quantisation only
    // shrinks it, so a predictor outside this is not this image's DC.
    let dc_limit = 1024i32 << frame.precision.saturating_sub(8).min(8);
    let max_dc_category = if frame.precision > 8 { 15 } else { 11 };
    let max_ac_category = if frame.precision > 8 { 14 } else { 10 };

    let mut r = BitReader::new(d, start);
    let mut preds = vec![0i32; plan.len()];
    let mut mcus = 0u64;
    let mut restarts = 0u64;
    let mut expect_rst = 0u8;
    let mut since_restart = 0u64;
    // End of the last MCU that decoded cleanly.
    let mut verified_to = start;

    while mcus < total_mcus {
        // A restart marker is due: the interval is full.
        if restart_interval > 0 && since_restart == restart_interval {
            if r.holds_a_whole_byte() {
                return broken(
                    verified_to,
                    mcus,
                    format!(
                        "{restart_interval} MCUs decoded and the entropy data runs on where a \
                         restart marker is due; bytes have been inserted since the last marker"
                    ),
                );
            }
            if !r.padding_is_ones() {
                return broken(
                    verified_to,
                    mcus,
                    "the bits before a restart marker are not the 1s that pad a scan".to_string(),
                );
            }
            match r.align() {
                Next::Marker(m) => {
                    if !(0xD0..=0xD7).contains(&m) {
                        return broken(
                            verified_to,
                            mcus,
                            format!(
                                "marker FF{m:02X} where restart marker RST{expect_rst} was due"
                            ),
                        );
                    }
                    if m - 0xD0 != expect_rst {
                        return broken(
                            verified_to,
                            mcus,
                            format!(
                                "restart marker RST{} where RST{expect_rst} was due",
                                m - 0xD0
                            ),
                        );
                    }
                    r.take_marker();
                    restarts += 1;
                    expect_rst = (expect_rst + 1) % 8;
                    since_restart = 0;
                    preds.iter_mut().for_each(|p| *p = 0);
                    verified_to = r.at;
                }
                Next::Data => {
                    return broken(
                        verified_to,
                        mcus,
                        format!(
                            "{restart_interval} MCUs decoded and the entropy data runs on where a \
                             restart marker is due; there is more in it than this file's - bytes \
                             have been inserted since the last marker"
                        ),
                    )
                }
                Next::End => return ScanResult::Truncated { verified_to, mcus },
            }
        }

        // One MCU: every block of every component in the scan.
        for &(ci, n) in &blocks {
            for _ in 0..n {
                let (_, dc_table, ac_table) = plan[ci];
                let Some(s) = r.decode(dc_table) else {
                    return end_of_data(&r, verified_to, mcus, "a DC code is not in the table");
                };
                if s > max_dc_category {
                    return broken(
                        verified_to,
                        mcus,
                        format!("a DC magnitude category of {s} cannot occur at this precision"),
                    );
                }
                let Some(bits) = r.receive(s) else {
                    return end_of_data(&r, verified_to, mcus, "the DC magnitude bits ran out");
                };
                preds[ci] += extend(bits, s);
                if preds[ci].abs() > dc_limit {
                    return broken(
                        verified_to,
                        mcus,
                        format!(
                            "the DC predictor reached {}, outside the +/-{dc_limit} an image of \
                             this precision can hold; these are not this file's coefficients",
                            preds[ci]
                        ),
                    );
                }
                // AC coefficients, run-length coded to the end of the block.
                let mut k = 1usize;
                while k < 64 {
                    let Some(rs) = r.decode(ac_table) else {
                        return end_of_data(
                            &r,
                            verified_to,
                            mcus,
                            "an AC code is not in the table",
                        );
                    };
                    let run = (rs >> 4) as usize;
                    let size = rs & 0x0F;
                    if size == 0 {
                        if run == 15 {
                            k += 16; // ZRL: sixteen zero coefficients
                            continue;
                        }
                        break; // end of block
                    }
                    if size > max_ac_category {
                        return broken(
                            verified_to,
                            mcus,
                            format!(
                                "an AC magnitude category of {size} cannot occur at this precision"
                            ),
                        );
                    }
                    k += run;
                    if k > 63 {
                        return broken(
                            verified_to,
                            mcus,
                            "a zero run reaches past the 64th coefficient of a block".to_string(),
                        );
                    }
                    if r.receive(size).is_none() {
                        return end_of_data(&r, verified_to, mcus, "the AC magnitude bits ran out");
                    }
                    k += 1;
                }
                if k > 64 {
                    return broken(
                        verified_to,
                        mcus,
                        "a zero run reaches past the 64th coefficient of a block".to_string(),
                    );
                }
            }
        }
        mcus += 1;
        since_restart += 1;
        // The MCU is whole, so every byte it consumed is justified.
        verified_to = r.consumed_to;
    }

    // Every MCU accounted for: the scan must end here, at a marker.
    if r.holds_a_whole_byte() {
        return broken(
            verified_to,
            mcus,
            format!(
                "all {total_mcus} MCUs of the frame decoded and the entropy data runs on; there \
                 is more in it than this file's"
            ),
        );
    }
    if !r.padding_is_ones() {
        return broken(
            verified_to,
            mcus,
            "the bits after the last MCU are not the 1s that pad a scan".to_string(),
        );
    }
    match r.align() {
        Next::Marker(_) => {}
        Next::Data => {
            return broken(
                verified_to,
                mcus,
                format!(
                    "all {total_mcus} MCUs of the frame decoded and the entropy data runs on; \
                     there is more in it than this file's"
                ),
            )
        }
        Next::End => return ScanResult::Truncated { verified_to, mcus },
    }
    ScanResult::Complete {
        end: r.marker_at.unwrap_or(r.consumed_to),
        mcus,
        restarts,
    }
}

/// A decode that stopped for want of bits is a truncation; one that stopped on
/// a marker where data was due is a splice.
fn end_of_data(r: &BitReader, verified_to: usize, mcus: u64, why: &str) -> ScanResult {
    if r.at_marker() {
        broken(
            verified_to,
            mcus,
            format!("{why}: a marker arrives mid-MCU, so the entropy data stops short"),
        )
    } else {
        ScanResult::Truncated { verified_to, mcus }
    }
}

fn broken(verified_to: usize, mcus: u64, why: String) -> ScanResult {
    ScanResult::Broken {
        verified_to,
        mcus,
        why,
    }
}
