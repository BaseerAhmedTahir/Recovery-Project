//! Formats whose header states, or implies, exactly where the file ends:
//! RTF, TIFF, EVTX, registry hives and ELF.
//!
//! Grouped because they share the property that matters for carving. None has
//! a footer, none needs decoding, and each carries enough in a fixed-position
//! field to compute a length - a brace depth, a strip table, a chunk count, a
//! bin size, a section table. Before these, all five located a file and left
//! its end unknown.

use super::{be16, be32, le16, le32, Outcome};

// ---------------------------------------------------------------------------
// RTF
// ---------------------------------------------------------------------------

/// RTF: a document is one brace group, so the file ends at the brace that
/// closes the first one.
///
/// The subtlety is that `\{` and `\}` are escaped literals and must not count,
/// and `\\` is an escaped backslash that must not escape the character after
/// it. Counting braces naively runs off the end of most real documents.
pub fn validate_rtf(d: &[u8]) -> Outcome {
    if d.len() < 6 {
        return Outcome::reject("shorter than an RTF header").truncated();
    }
    if !d.starts_with(b"{\\rtf") {
        return Outcome::reject("does not begin with {\\rtf");
    }
    // The version digit follows.
    if !d[5].is_ascii_digit() {
        return Outcome::reject("no version digit after {\\rtf");
    }

    let mut depth = 0i64;
    let mut i = 0usize;
    let mut groups = 0u64;
    while i < d.len() {
        match d[i] {
            b'\\' => {
                // Skip the escaped character, whatever it is.
                i += 2;
                continue;
            }
            b'{' => {
                depth += 1;
                groups += 1;
            }
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Outcome::valid((i + 1) as u64)
                        .with("version", (d[5] - b'0') as u64)
                        .with("groups", groups);
                }
                if depth < 0 {
                    return Outcome::reject("a closing brace with nothing open");
                }
            }
            _ => {}
        }
        i += 1;
    }
    Outcome::partial(
        d.len() as u64,
        format!("the outermost group never closes; {depth} still open"),
    )
    .truncated()
}

// ---------------------------------------------------------------------------
// TIFF
// ---------------------------------------------------------------------------

/// TIFF: the file ends past the furthest thing any directory entry points at.
///
/// The naive version of this - track StripOffsets and StripByteCounts, take
/// the maximum - is wrong on almost every real TIFF, and the independent corpus
/// is what showed it. ImageMagick's output came back 70 bytes short, because a
/// directory entry whose value needs more than four bytes stores it *outside*
/// the entry, at an offset that is usually past the image data: a set of
/// per-channel bit depths, a pair of RATIONAL resolutions, a software string.
/// Those bytes are part of the file and a carve that stops before them produces
/// a TIFF that decoders reject.
///
/// So every entry contributes, whether it is a strip pointer or not, and
/// multi-strip images have their offset and count arrays followed rather than
/// skipped. My own hand-written sample was single-strip with everything inline,
/// which is exactly why it could not have caught this.
///
/// Both byte orders are real and both are in the signature database, so the
/// endianness comes from the file rather than from an assumption.
pub fn validate_tiff(d: &[u8]) -> Outcome {
    if d.len() < 8 {
        return Outcome::reject("shorter than a TIFF header").truncated();
    }
    let big = match &d[..2] {
        b"II" => false,
        b"MM" => true,
        _ => return Outcome::reject("byte-order mark is neither II nor MM"),
    };
    let u16at = |at: usize| -> Option<u64> {
        let b = d.get(at..at + 2)?;
        Some(if big {
            u16::from_be_bytes([b[0], b[1]]) as u64
        } else {
            u16::from_le_bytes([b[0], b[1]]) as u64
        })
    };
    let u32at = |at: usize| -> Option<u64> {
        let b = d.get(at..at + 4)?;
        Some(if big {
            u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as u64
        } else {
            u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as u64
        })
    };

    if u16at(2) != Some(42) {
        return Outcome::reject("the magic number after the byte order is not 42");
    }
    let mut ifd = u32at(4).unwrap_or(0) as usize;
    if ifd < 8 {
        return Outcome::reject("the first IFD offset points into the header");
    }

    let mut end = 8u64;
    let mut ifds = 0u64;
    let mut entries_total = 0u64;
    let mut width = 0u64;
    let mut height = 0u64;
    let mut strip_offsets: Vec<u64> = Vec::new();
    let mut strip_counts: Vec<u64> = Vec::new();

    // Bounded so a corrupt next-IFD pointer that loops cannot spin here.
    for _ in 0..64 {
        if ifd == 0 {
            break;
        }
        let Some(count) = u16at(ifd) else {
            return truncated_tiff(end, ifds, "an IFD is past the available data");
        };
        if count == 0 || count > 512 {
            return structural_tiff(end, ifds, "an IFD declares an implausible entry count");
        }
        let ifd_end = ifd + 2 + 12 * count as usize + 4;
        if ifd_end > d.len() {
            return truncated_tiff(end, ifds, "an IFD runs past the available data");
        }
        end = end.max(ifd_end as u64);

        for i in 0..count as usize {
            let at = ifd + 2 + 12 * i;
            let tag = u16at(at).unwrap_or(0);
            let typ = u16at(at + 2).unwrap_or(0);
            let n = u32at(at + 4).unwrap_or(0);
            entries_total += 1;

            let unit = type_size(typ);
            let bytes = unit.saturating_mul(n);
            // Four bytes or fewer live in the entry; anything larger is stored
            // elsewhere and the value field is an offset to it.
            let inline = bytes <= 4;
            if !inline {
                let off = u32at(at + 8).unwrap_or(0);
                end = end.max(off.saturating_add(bytes));
            }

            match tag {
                256 => width = read_value(d, at, typ, 0, big).unwrap_or(0),
                257 => height = read_value(d, at, typ, 0, big).unwrap_or(0),
                273 => {
                    for k in 0..n.min(4096) {
                        if let Some(v) = read_value(d, at, typ, k, big) {
                            strip_offsets.push(v);
                        }
                    }
                }
                279 => {
                    for k in 0..n.min(4096) {
                        if let Some(v) = read_value(d, at, typ, k, big) {
                            strip_counts.push(v);
                        }
                    }
                }
                _ => {}
            }
        }
        ifds += 1;
        ifd = u32at(ifd_end - 4).unwrap_or(0) as usize;
    }

    if width == 0 || height == 0 {
        return Outcome::reject("no usable ImageWidth and ImageLength tags");
    }
    if strip_offsets.is_empty() {
        return Outcome::partial(
            end,
            "the directories are intact but no strip offsets were found, so the image \
             data's extent is unknown; length covers the directories only",
        )
        .with("width", width)
        .with("height", height)
        .with("ifds", ifds);
    }
    if strip_offsets.len() != strip_counts.len() {
        return Outcome::partial(
            end,
            format!(
                "{} strip offsets against {} byte counts; the image data's extent \
                 cannot be resolved",
                strip_offsets.len(),
                strip_counts.len()
            ),
        )
        .with("width", width)
        .with("height", height);
    }
    for (o, c) in strip_offsets.iter().zip(&strip_counts) {
        end = end.max(o.saturating_add(*c));
    }

    let out = |o: Outcome| {
        o.with("byte_order", if big { "MM" } else { "II" })
            .with("width", width)
            .with("height", height)
            .with("ifds", ifds)
            .with("entries", entries_total)
            .with("strips", strip_offsets.len())
    };
    if end > d.len() as u64 {
        return out(Outcome::partial(
            end,
            format!(
                "the directories reach {end} but only {} bytes are available",
                d.len()
            ),
        )
        .established()
        .truncated());
    }
    out(Outcome::valid(end))
}

/// Bytes per element of a TIFF field type. Unknown types return 0, which makes
/// them contribute nothing rather than a wild offset.
fn type_size(typ: u64) -> u64 {
    match typ {
        1 | 2 | 6 | 7 => 1, // BYTE, ASCII, SBYTE, UNDEFINED
        3 | 8 => 2,         // SHORT, SSHORT
        4 | 9 | 11 => 4,    // LONG, SLONG, FLOAT
        5 | 10 | 12 => 8,   // RATIONAL, SRATIONAL, DOUBLE
        _ => 0,
    }
}

/// The `k`th value of a directory entry, following the value field out of line
/// when the entry is too large to hold it.
///
/// Only the integer types are read: the callers want dimensions and strip
/// pointers, and a RATIONAL resolution is not something to compare offsets on.
fn read_value(d: &[u8], entry_at: usize, typ: u64, k: u64, big: bool) -> Option<u64> {
    let unit = type_size(typ);
    if !matches!(typ, 3 | 4) {
        return None;
    }
    let count = {
        let b = d.get(entry_at + 4..entry_at + 8)?;
        if big {
            u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as u64
        } else {
            u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as u64
        }
    };
    if k >= count {
        return None;
    }
    let total = unit.checked_mul(count)?;
    let base = if total <= 4 {
        entry_at + 8
    } else {
        let b = d.get(entry_at + 8..entry_at + 12)?;
        if big {
            u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize
        } else {
            u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize
        }
    };
    let at = base.checked_add((k * unit) as usize)?;
    match typ {
        3 => {
            let b = d.get(at..at + 2)?;
            Some(if big {
                u16::from_be_bytes([b[0], b[1]]) as u64
            } else {
                u16::from_le_bytes([b[0], b[1]]) as u64
            })
        }
        4 => {
            let b = d.get(at..at + 4)?;
            Some(if big {
                u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as u64
            } else {
                u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as u64
            })
        }
        _ => None,
    }
}

fn truncated_tiff(end: u64, ifds: u64, why: &str) -> Outcome {
    if ifds > 0 {
        Outcome::partial(end, format!("truncated: {why}")).truncated()
    } else {
        Outcome::reject(format!("{why}, before any directory was read")).truncated()
    }
}

fn structural_tiff(end: u64, ifds: u64, why: &str) -> Outcome {
    if ifds > 0 {
        Outcome::partial(end, format!("the directory chain broke: {why}"))
    } else {
        Outcome::reject(format!("the directory chain broke immediately: {why}"))
    }
}

// ---------------------------------------------------------------------------
// EVTX
// ---------------------------------------------------------------------------

/// Windows event log: a 4096-byte header followed by fixed 64 KiB chunks, and
/// the header says how many.
pub fn validate_evtx(d: &[u8]) -> Outcome {
    const HEADER: u64 = 4096;
    const CHUNK: u64 = 65536;

    if d.len() < 128 {
        return Outcome::reject("shorter than an EVTX file header").truncated();
    }
    if &d[..8] != b"ElfFile\0" {
        return Outcome::reject("signature mismatch");
    }
    // Signature 8, then three 8-byte counters, so the fixed fields start at 32.
    let header_size = le32(d, 32).unwrap_or(0);
    let minor = le16(d, 36).unwrap_or(0);
    let major = le16(d, 38).unwrap_or(0);
    let header_block = le16(d, 40).unwrap_or(0) as u64;
    let chunks = le16(d, 42).unwrap_or(0) as u64;

    if header_size != 128 {
        return Outcome::reject(format!("the header size field is {header_size}, not 128"));
    }
    if major != 3 {
        return Outcome::reject(format!("major version {major} is not 3"));
    }
    if header_block != HEADER {
        return Outcome::reject(format!(
            "the header block size is {header_block}, not {HEADER}"
        ));
    }
    if chunks == 0 {
        return Outcome::reject("the header declares no chunks");
    }

    let total = HEADER + chunks * CHUNK;
    let out = |o: Outcome| {
        o.with("version", format!("{major}.{minor}"))
            .with("chunks", chunks)
    };
    if total > d.len() as u64 {
        return out(Outcome::partial(
            total,
            format!(
                "the header declares {chunks} chunk(s) = {total} bytes, but only {} \
                     are available",
                d.len()
            ),
        )
        .established()
        .truncated());
    }
    out(Outcome::valid(total))
}

// ---------------------------------------------------------------------------
// registry hive
// ---------------------------------------------------------------------------

/// Registry hive: a 4096-byte base block, then `hive bins size` bytes of bins.
pub fn validate_reg_hive(d: &[u8]) -> Outcome {
    const BASE: u64 = 4096;

    if d.len() < 48 {
        return Outcome::reject("shorter than a hive base block").truncated();
    }
    if &d[..4] != b"regf" {
        return Outcome::reject("signature mismatch");
    }
    let primary = le32(d, 4).unwrap_or(0);
    let secondary = le32(d, 8).unwrap_or(0);
    let major = le32(d, 20).unwrap_or(0);
    let file_type = le32(d, 28).unwrap_or(0);
    let file_format = le32(d, 32).unwrap_or(0);
    let bins = le32(d, 40).unwrap_or(0) as u64;

    if major != 1 {
        return Outcome::reject(format!("major version {major} is not 1"));
    }
    if file_format != 1 {
        return Outcome::reject("the file format field is not 1 (direct memory load)");
    }
    if file_type > 1 {
        return Outcome::reject(format!(
            "file type {file_type} is not a primary or log hive"
        ));
    }
    if bins == 0 || bins % BASE != 0 {
        return Outcome::reject(format!(
            "the hive bins size is {bins}, which is not a non-zero multiple of {BASE}"
        ));
    }

    let total = BASE + bins;
    let out = |o: Outcome| {
        o.with("hive_bins_bytes", bins)
            .with("sequences_match", primary == secondary)
    };
    if total > d.len() as u64 {
        return out(Outcome::partial(
            total,
            format!(
                "the base block declares {total} bytes but only {} are available",
                d.len()
            ),
        )
        .established()
        .truncated());
    }
    out(Outcome::valid(total))
}

// ---------------------------------------------------------------------------
// PSD
// ---------------------------------------------------------------------------

/// Photoshop: three length-prefixed sections, then image data whose size the
/// geometry gives when it is stored raw.
pub fn validate_psd(d: &[u8]) -> Outcome {
    if d.len() < 34 {
        return Outcome::reject("shorter than a PSD header").truncated();
    }
    if &d[..4] != b"8BPS" {
        return Outcome::reject("signature mismatch");
    }
    let version = be16(d, 4).unwrap_or(0);
    if version != 1 && version != 2 {
        return Outcome::reject(format!("version {version} is neither PSD (1) nor PSB (2)"));
    }
    // Six reserved bytes that every writer leaves zero.
    if d[6..12].iter().any(|b| *b != 0) {
        return Outcome::reject("the reserved bytes in the header are non-zero");
    }
    let channels = be16(d, 12).unwrap_or(0) as u64;
    let height = be32(d, 14).unwrap_or(0) as u64;
    let width = be32(d, 18).unwrap_or(0) as u64;
    let depth = be16(d, 22).unwrap_or(0) as u64;
    let mode = be16(d, 24).unwrap_or(0);

    if !(1..=56).contains(&channels) {
        return Outcome::reject(format!("{channels} channels is outside the 1..56 range"));
    }
    if width == 0 || height == 0 {
        return Outcome::reject("a dimension is zero");
    }
    if !matches!(depth, 1 | 8 | 16 | 32) {
        return Outcome::reject(format!("{depth} bits per channel is not a defined depth"));
    }
    if mode > 15 {
        return Outcome::reject(format!("colour mode {mode} is not defined"));
    }

    // Three length-prefixed sections: colour mode data, image resources, and
    // the layer and mask information.
    let mut at = 26usize;
    for what in ["colour mode data", "image resources", "layer and mask info"] {
        let Some(n) = be32(d, at) else {
            return Outcome::partial(
                at as u64,
                format!("truncated before the {what} section length"),
            )
            .truncated();
        };
        let Some(next) = at.checked_add(4).and_then(|v| v.checked_add(n as usize)) else {
            return Outcome::reject(format!("the {what} section length overflows"));
        };
        if next > d.len() {
            return Outcome::partial(
                at as u64,
                format!("the {what} section runs past the available data"),
            )
            .truncated();
        }
        at = next;
    }

    let Some(compression) = be16(d, at) else {
        return Outcome::partial(at as u64, "truncated before the compression method").truncated();
    };
    at += 2;

    let evidence = |o: Outcome| {
        o.with("width", width)
            .with("height", height)
            .with("channels", channels)
            .with("depth", depth)
            .with("compression", compression)
    };

    // Raw storage gives an exact size. RLE and the zip variants do not without
    // walking the row table, so say the length is not established rather than
    // guess one.
    if compression != 0 {
        return evidence(Outcome::partial(
            at as u64,
            format!(
                "the header is intact but the image data uses compression method \
                 {compression}, whose length needs a row-table walk; this length covers \
                 the headers only"
            ),
        ));
    }

    let pixels = width
        .saturating_mul(height)
        .saturating_mul(channels)
        .saturating_mul(depth.div_ceil(8));
    let total = at as u64 + pixels;
    if total > d.len() as u64 {
        return evidence(
            Outcome::partial(
                total,
                format!(
                    "the geometry needs {total} bytes but only {} are available",
                    d.len()
                ),
            )
            .established()
            .truncated(),
        );
    }
    evidence(Outcome::valid(total))
}

// ---------------------------------------------------------------------------
// ELF
// ---------------------------------------------------------------------------

/// ELF: the file ends past the furthest of its program headers, section
/// headers, and the segments and sections they point at.
pub fn validate_elf(d: &[u8]) -> Outcome {
    if d.len() < 24 {
        return Outcome::reject("shorter than an ELF identification header").truncated();
    }
    if &d[..4] != b"\x7FELF" {
        return Outcome::reject("signature mismatch");
    }
    let class = d[4]; // 1 = 32-bit, 2 = 64-bit
    let data = d[5]; // 1 = little-endian, 2 = big-endian
    let version = d[6];
    if !matches!(class, 1 | 2) {
        return Outcome::reject(format!("EI_CLASS is {class}, neither 32- nor 64-bit"));
    }
    if !matches!(data, 1 | 2) {
        return Outcome::reject(format!("EI_DATA is {data}, neither endianness"));
    }
    if version != 1 {
        return Outcome::reject(format!("EI_VERSION is {version}, not 1"));
    }
    let big = data == 2;
    let wide = class == 2;

    let u16at = |at: usize| -> Option<u64> {
        let b = d.get(at..at + 2)?;
        Some(if big {
            u16::from_be_bytes([b[0], b[1]]) as u64
        } else {
            u16::from_le_bytes([b[0], b[1]]) as u64
        })
    };
    let u32at = |at: usize| -> Option<u64> {
        let b = d.get(at..at + 4)?;
        Some(if big {
            u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as u64
        } else {
            u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as u64
        })
    };
    let wordat = |at: usize| -> Option<u64> {
        if wide {
            let b = d.get(at..at + 8)?;
            let a: [u8; 8] = b.try_into().ok()?;
            Some(if big {
                u64::from_be_bytes(a)
            } else {
                u64::from_le_bytes(a)
            })
        } else {
            u32at(at)
        }
    };

    let e_type = u16at(16).unwrap_or(0);
    if !(1..=4).contains(&e_type) && !(0xFE00..=0xFFFF).contains(&e_type) {
        return Outcome::reject(format!("e_type {e_type} is not a defined object type"));
    }

    // Header layout diverges after e_version depending on the class.
    let (phoff_at, shoff_at) = if wide { (32, 40) } else { (28, 32) };
    let hdr_tail = if wide { 52 } else { 40 };
    let e_phoff = wordat(phoff_at).unwrap_or(0);
    let e_shoff = wordat(shoff_at).unwrap_or(0);
    let e_phentsize = u16at(hdr_tail + 2).unwrap_or(0);
    let e_phnum = u16at(hdr_tail + 4).unwrap_or(0);
    let e_shentsize = u16at(hdr_tail + 6).unwrap_or(0);
    let e_shnum = u16at(hdr_tail + 8).unwrap_or(0);

    if e_phnum > 0 && e_phentsize < if wide { 56 } else { 32 } {
        return Outcome::reject("the program header entry size is too small for the class");
    }

    let mut end = if wide { 64u64 } else { 52 };
    if e_phnum > 0 {
        end = end.max(e_phoff + e_phnum * e_phentsize);
    }
    if e_shnum > 0 {
        end = end.max(e_shoff + e_shnum * e_shentsize);
    }

    // Segments carry a file size directly.
    for i in 0..e_phnum.min(256) {
        let at = (e_phoff + i * e_phentsize) as usize;
        let (off_at, filesz_at) = if wide {
            (at + 8, at + 32)
        } else {
            (at + 4, at + 16)
        };
        let (Some(off), Some(filesz)) = (wordat(off_at), wordat(filesz_at)) else {
            break;
        };
        end = end.max(off.saturating_add(filesz));
    }
    // Sections too, except SHT_NOBITS which occupies no file space.
    for i in 0..e_shnum.min(512) {
        let at = (e_shoff + i * e_shentsize) as usize;
        let (type_at, off_at, size_at) = if wide {
            (at + 4, at + 24, at + 32)
        } else {
            (at + 4, at + 16, at + 20)
        };
        let Some(sh_type) = u32at(type_at) else { break };
        if sh_type == 8 {
            continue; // SHT_NOBITS
        }
        let (Some(off), Some(size)) = (wordat(off_at), wordat(size_at)) else {
            break;
        };
        end = end.max(off.saturating_add(size));
    }

    let out = |o: Outcome| {
        o.with("class", if wide { 64 } else { 32 })
            .with("endian", if big { "big" } else { "little" })
            .with("type", e_type)
            .with("segments", e_phnum)
            .with("sections", e_shnum)
    };
    if end > d.len() as u64 {
        return out(Outcome::partial(
            end,
            format!(
                "the headers reach {end} but only {} bytes are available",
                d.len()
            ),
        )
        .established()
        .truncated());
    }
    out(Outcome::valid(end))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate::Status;

    // --- RTF ---------------------------------------------------------------

    #[test]
    fn rtf_ends_at_the_brace_that_closes_the_document() {
        let doc = b"{\\rtf1\\ansi{\\fonttbl{\\f0 Courier;}}Hello world.}".to_vec();
        let out = validate_rtf(&doc);
        assert_eq!(out.status, Status::Valid, "{}", out.detail);
        assert_eq!(out.length, doc.len() as u64);
    }

    /// The detail a naive brace counter gets wrong.
    #[test]
    fn rtf_ignores_escaped_braces() {
        let doc = b"{\\rtf1 a \\{ b \\} c }".to_vec();
        let out = validate_rtf(&doc);
        assert_eq!(out.status, Status::Valid, "{}", out.detail);
        assert_eq!(out.length, doc.len() as u64);
    }

    #[test]
    fn rtf_stops_at_its_own_end_not_the_buffer_end() {
        let mut doc = b"{\\rtf1 body }".to_vec();
        let real = doc.len();
        doc.extend_from_slice(b"{\\rtf1 a following document }");
        assert_eq!(validate_rtf(&doc).length, real as u64);
    }

    #[test]
    fn rtf_unclosed_is_partial() {
        let out = validate_rtf(b"{\\rtf1 this never closes");
        assert_eq!(out.status, Status::Partial, "{}", out.detail);
    }

    #[test]
    fn rtf_rejects_a_chance_header() {
        assert_eq!(validate_rtf(b"{\\rtfX nope}").status, Status::Rejected);
    }

    // --- TIFF --------------------------------------------------------------

    fn tiff(big: bool, w: u32, h: u32) -> Vec<u8> {
        let pack16 = |v: u16| {
            if big {
                v.to_be_bytes()
            } else {
                v.to_le_bytes()
            }
        };
        let pack32 = |v: u32| {
            if big {
                v.to_be_bytes()
            } else {
                v.to_le_bytes()
            }
        };
        let strip = (w * h) as usize;
        let entries: [(u16, u16, u32); 8] = [
            (256, 3, w),
            (257, 3, h),
            (258, 3, 8),
            (259, 3, 1),
            (262, 3, 1),
            (273, 4, 0),
            (278, 3, h),
            (279, 4, strip as u32),
        ];
        let ifd_at = 8usize;
        let ifd_size = 2 + 12 * entries.len() + 4;
        let data_at = (ifd_at + ifd_size) as u32;

        let mut v = Vec::new();
        v.extend_from_slice(if big { b"MM" } else { b"II" });
        v.extend_from_slice(&pack16(42));
        v.extend_from_slice(&pack32(ifd_at as u32));
        v.extend_from_slice(&pack16(entries.len() as u16));
        for (tag, typ, value) in entries {
            v.extend_from_slice(&pack16(tag));
            v.extend_from_slice(&pack16(typ));
            v.extend_from_slice(&pack32(1));
            let value = if tag == 273 { data_at } else { value };
            if typ == 3 {
                v.extend_from_slice(&pack16(value as u16));
                v.extend_from_slice(&[0, 0]);
            } else {
                v.extend_from_slice(&pack32(value));
            }
        }
        v.extend_from_slice(&pack32(0));
        v.resize(data_at as usize + strip, 0x7E);
        v
    }

    #[test]
    fn tiff_length_comes_from_the_strip_table_in_both_byte_orders() {
        for big in [false, true] {
            let t = tiff(big, 64, 64);
            let out = validate_tiff(&t);
            assert_eq!(out.status, Status::Valid, "{}", out.detail);
            assert_eq!(out.length, t.len() as u64, "byte order big={big}");
            assert_eq!(
                out.evidence_of("byte_order"),
                Some(if big { "MM" } else { "II" })
            );
        }
    }

    #[test]
    fn tiff_trailing_bytes_do_not_extend_the_length() {
        let mut t = tiff(false, 32, 32);
        let real = t.len();
        t.extend_from_slice(&[0; 4096]);
        assert_eq!(validate_tiff(&t).length, real as u64);
    }

    #[test]
    fn tiff_rejects_a_wrong_magic_number() {
        let mut t = tiff(false, 16, 16);
        t[2..4].copy_from_slice(&43u16.to_le_bytes());
        assert_eq!(validate_tiff(&t).status, Status::Rejected);
    }

    // --- EVTX --------------------------------------------------------------

    fn evtx(chunks: u16) -> Vec<u8> {
        let mut v = b"ElfFile\0".to_vec();
        // Three 8-byte counters, not four: first chunk, last chunk, next record
        // id. The fixed fields begin at offset 32.
        v.extend_from_slice(&0u64.to_le_bytes());
        v.extend_from_slice(&1u64.to_le_bytes());
        v.extend_from_slice(&1u64.to_le_bytes());
        v.extend_from_slice(&128u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&3u16.to_le_bytes());
        v.extend_from_slice(&4096u16.to_le_bytes());
        v.extend_from_slice(&chunks.to_le_bytes());
        v.resize(4096 + chunks as usize * 65536, 0);
        v
    }

    #[test]
    fn evtx_length_is_the_header_plus_its_chunks() {
        let e = evtx(1);
        let out = validate_evtx(&e);
        assert_eq!(out.status, Status::Valid, "{}", out.detail);
        assert_eq!(out.length, 4096 + 65536);
        assert_eq!(out.evidence_of("chunks"), Some("1"));
    }

    #[test]
    fn evtx_rejects_a_wrong_header_block_size() {
        let mut e = evtx(1);
        e[40..42].copy_from_slice(&512u16.to_le_bytes());
        assert_eq!(validate_evtx(&e).status, Status::Rejected);
    }

    // --- registry ----------------------------------------------------------

    fn hive(bins: u32) -> Vec<u8> {
        let mut v = b"regf".to_vec();
        v.extend_from_slice(&1u32.to_le_bytes());
        v.extend_from_slice(&1u32.to_le_bytes());
        v.extend_from_slice(&0u64.to_le_bytes());
        v.extend_from_slice(&1u32.to_le_bytes()); // major
        v.extend_from_slice(&3u32.to_le_bytes()); // minor
        v.extend_from_slice(&0u32.to_le_bytes()); // file type
        v.extend_from_slice(&1u32.to_le_bytes()); // file format
        v.extend_from_slice(&0x20u32.to_le_bytes()); // root cell
        v.extend_from_slice(&bins.to_le_bytes());
        v.resize(4096 + bins as usize, 0);
        v
    }

    #[test]
    fn hive_length_is_the_base_block_plus_its_bins() {
        let h = hive(4096);
        let out = validate_reg_hive(&h);
        assert_eq!(out.status, Status::Valid, "{}", out.detail);
        assert_eq!(out.length, 8192);
        assert_eq!(out.evidence_of("sequences_match"), Some("true"));
    }

    #[test]
    fn hive_rejects_a_bins_size_that_is_not_a_multiple_of_the_block() {
        let mut h = hive(4096);
        h[40..44].copy_from_slice(&5000u32.to_le_bytes());
        assert_eq!(validate_reg_hive(&h).status, Status::Rejected);
    }

    // --- PSD ---------------------------------------------------------------

    fn psd(w: u32, h: u32, channels: u16, compression: u16) -> Vec<u8> {
        let mut v = b"8BPS".to_vec();
        v.extend_from_slice(&1u16.to_be_bytes());
        v.extend_from_slice(&[0u8; 6]);
        v.extend_from_slice(&channels.to_be_bytes());
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&8u16.to_be_bytes()); // depth
        v.extend_from_slice(&3u16.to_be_bytes()); // RGB
        v.extend_from_slice(&0u32.to_be_bytes()); // colour mode data
        v.extend_from_slice(&0u32.to_be_bytes()); // image resources
        v.extend_from_slice(&0u32.to_be_bytes()); // layer and mask
        v.extend_from_slice(&compression.to_be_bytes());
        if compression == 0 {
            v.resize(v.len() + (w * h * channels as u32) as usize, 0x7F);
        }
        v
    }

    #[test]
    fn psd_length_comes_from_its_geometry_when_stored_raw() {
        let p = psd(64, 64, 3, 0);
        let out = validate_psd(&p);
        assert_eq!(out.status, Status::Valid, "{}", out.detail);
        assert_eq!(out.length, p.len() as u64);
        assert_eq!(out.evidence_of("channels"), Some("3"));
    }

    #[test]
    fn psd_trailing_bytes_do_not_extend_the_length() {
        let mut p = psd(32, 32, 3, 0);
        let real = p.len();
        p.extend_from_slice(&[0; 4096]);
        assert_eq!(validate_psd(&p).length, real as u64);
    }

    /// RLE storage has no length without a row-table walk, so the validator
    /// must not invent one.
    #[test]
    fn psd_rle_does_not_claim_an_established_length() {
        let p = psd(64, 64, 3, 1);
        let out = validate_psd(&p);
        assert_eq!(out.status, Status::Partial, "{}", out.detail);
        assert!(!out.length_established);
    }

    #[test]
    fn psd_rejects_implausible_headers() {
        let mut p = psd(64, 64, 3, 0);
        p[12..14].copy_from_slice(&99u16.to_be_bytes()); // 99 channels
        assert_eq!(validate_psd(&p).status, Status::Rejected);

        let mut p = psd(64, 64, 3, 0);
        p[6] = 1; // reserved byte
        assert_eq!(validate_psd(&p).status, Status::Rejected);
    }

    // --- ELF ---------------------------------------------------------------

    fn elf64(payload: usize) -> Vec<u8> {
        let mut v = b"\x7FELF".to_vec();
        v.extend_from_slice(&[2, 1, 1, 0]);
        v.extend_from_slice(&[0u8; 8]);
        v.extend_from_slice(&2u16.to_le_bytes()); // ET_EXEC
        v.extend_from_slice(&0x3Eu16.to_le_bytes());
        v.extend_from_slice(&1u32.to_le_bytes());
        v.extend_from_slice(&0x400000u64.to_le_bytes()); // e_entry
        v.extend_from_slice(&64u64.to_le_bytes()); // e_phoff
        v.extend_from_slice(&0u64.to_le_bytes()); // e_shoff
        v.extend_from_slice(&0u32.to_le_bytes()); // e_flags
        v.extend_from_slice(&64u16.to_le_bytes()); // e_ehsize
        v.extend_from_slice(&56u16.to_le_bytes()); // e_phentsize
        v.extend_from_slice(&1u16.to_le_bytes()); // e_phnum
        v.extend_from_slice(&0u16.to_le_bytes()); // e_shentsize
        v.extend_from_slice(&0u16.to_le_bytes()); // e_shnum
        v.extend_from_slice(&0u16.to_le_bytes()); // e_shstrndx
        assert_eq!(v.len(), 64);
        // One PT_LOAD covering the whole file.
        let total = 64 + 56 + payload;
        v.extend_from_slice(&1u32.to_le_bytes()); // p_type
        v.extend_from_slice(&5u32.to_le_bytes()); // p_flags
        v.extend_from_slice(&0u64.to_le_bytes()); // p_offset
        v.extend_from_slice(&0x400000u64.to_le_bytes());
        v.extend_from_slice(&0x400000u64.to_le_bytes());
        v.extend_from_slice(&(total as u64).to_le_bytes()); // p_filesz
        v.extend_from_slice(&(total as u64).to_le_bytes());
        v.extend_from_slice(&0x1000u64.to_le_bytes());
        v.resize(total, 0x90);
        v
    }

    #[test]
    fn elf_length_covers_its_segments() {
        let e = elf64(9000);
        let out = validate_elf(&e);
        assert_eq!(out.status, Status::Valid, "{}", out.detail);
        assert_eq!(out.length, e.len() as u64);
        assert_eq!(out.evidence_of("class"), Some("64"));
        assert_eq!(out.evidence_of("segments"), Some("1"));
    }

    #[test]
    fn elf_trailing_bytes_do_not_extend_the_length() {
        let mut e = elf64(2000);
        let real = e.len();
        e.extend_from_slice(&[0; 4096]);
        assert_eq!(validate_elf(&e).length, real as u64);
    }

    #[test]
    fn elf_rejects_a_bad_class_or_version() {
        let mut e = elf64(100);
        e[4] = 7;
        assert_eq!(validate_elf(&e).status, Status::Rejected);

        let mut e = elf64(100);
        e[6] = 2;
        assert_eq!(validate_elf(&e).status, Status::Rejected);
    }

    #[test]
    fn all_of_these_reject_pseudorandom_data_behind_their_signature() {
        let mut state = 0x2468_ACE0u32;
        let mut next = |n: usize| -> Vec<u8> {
            (0..n)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 17;
                    state ^= state << 5;
                    state as u8
                })
                .collect()
        };
        for _ in 0..400 {
            let mut v = b"ElfFile\0".to_vec();
            v.extend(next(256));
            assert_eq!(validate_evtx(&v).status, Status::Rejected);

            let mut v = b"regf".to_vec();
            v.extend(next(256));
            assert_eq!(validate_reg_hive(&v).status, Status::Rejected);

            let mut v = b"\x7FELF".to_vec();
            v.extend(next(256));
            assert_ne!(validate_elf(&v).status, Status::Valid);
        }
    }
}
