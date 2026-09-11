//! GIF validator: walk the block chain to the trailer.
//!
//! GIF is one of the few formats here whose end is unambiguous - a single
//! `0x3B` trailer byte - but you cannot find it by searching, because `0x3B`
//! occurs constantly inside LZW-compressed image data. The only way to the
//! trailer is to walk the block structure: every block declares its own length,
//! and image and extension payloads arrive as chains of sub-blocks each
//! prefixed by a one-byte count and terminated by a zero.
//!
//! Getting that walk right is the whole validator. A carver that searched for
//! the first `0x3B` would truncate most real GIFs mid-image.

use super::{le16, Outcome};

const TRAILER: u8 = 0x3B;
const IMAGE_SEPARATOR: u8 = 0x2C;
const EXTENSION: u8 = 0x21;

pub fn validate(d: &[u8]) -> Outcome {
    if d.len() < 13 {
        return Outcome::reject("shorter than a GIF header and screen descriptor");
    }
    if &d[..3] != b"GIF" || !matches!(&d[3..6], b"87a" | b"89a") {
        return Outcome::reject("not a GIF87a or GIF89a header");
    }
    let version = String::from_utf8_lossy(&d[3..6]).to_string();

    let width = le16(d, 6).unwrap_or(0);
    let height = le16(d, 8).unwrap_or(0);
    if width == 0 || height == 0 {
        return Outcome::reject("the logical screen descriptor declares a zero dimension");
    }
    let flags = d[10];

    // A global colour table, if present, is 3 * 2^(N+1) bytes.
    let mut at = 13usize;
    if flags & 0x80 != 0 {
        at += 3 * (1usize << ((flags & 0x07) + 1));
        if at > d.len() {
            return Outcome::reject("the global colour table runs past the data");
        }
    }

    let mut images = 0u64;
    let mut extensions = 0u64;

    loop {
        let Some(&marker) = d.get(at) else {
            return truncated(at, images, "ran out of data before the trailer");
        };
        at += 1;
        match marker {
            TRAILER => {
                if images == 0 {
                    return Outcome::reject("reached the trailer with no image block");
                }
                return Outcome::valid(at as u64)
                    .with("version", version)
                    .with("width", width)
                    .with("height", height)
                    .with("images", images)
                    .with("extensions", extensions);
            }
            IMAGE_SEPARATOR => {
                // Image descriptor: 9 bytes, then an optional local colour
                // table, then the LZW minimum code size, then sub-blocks.
                let Some(desc) = d.get(at..at + 9) else {
                    return truncated(at, images, "truncated image descriptor");
                };
                let local = desc[8];
                at += 9;
                if local & 0x80 != 0 {
                    at += 3 * (1usize << ((local & 0x07) + 1));
                }
                // LZW minimum code size.
                match d.get(at) {
                    None => return truncated(at, images, "truncated before the LZW code size"),
                    // The spec allows 2 through 12; anything else is not an
                    // image and is a good sign the walk has left the structure.
                    Some(&n) if !(2..=12).contains(&n) => {
                        return structural(at, images, "an implausible LZW minimum code size")
                    }
                    Some(_) => at += 1,
                }
                // Count the image here rather than after its data. A complete
                // image descriptor plus a plausible LZW code size is already
                // enough to know this is a GIF, and a file whose *first* image
                // is cut short should come back truncated rather than rejected -
                // that is exactly the file a carve of a damaged disk produces.
                images += 1;
                match skip_sub_blocks(d, at) {
                    Some(next) => at = next,
                    None => return truncated(at, images, "image data runs past the end"),
                }
            }
            EXTENSION => {
                // Extension: a one-byte label, then sub-blocks.
                if at >= d.len() {
                    return truncated(at, images, "truncated extension label");
                }
                at += 1;
                match skip_sub_blocks(d, at) {
                    Some(next) => at = next,
                    None => return truncated(at, images, "extension data runs past the end"),
                }
                extensions += 1;
            }
            _ => {
                return structural(
                    at - 1,
                    images,
                    "a block marker that is not an image, an extension or the trailer",
                )
            }
        }
    }
}

/// Step over a chain of sub-blocks, returning the offset just past the
/// terminating zero-length block.
fn skip_sub_blocks(d: &[u8], mut at: usize) -> Option<usize> {
    loop {
        let n = *d.get(at)? as usize;
        at += 1;
        if n == 0 {
            return Some(at);
        }
        at = at.checked_add(n)?;
        if at > d.len() {
            return None;
        }
    }
}

fn truncated(at: usize, images: u64, why: &str) -> Outcome {
    if images > 0 {
        Outcome::partial(at as u64, format!("truncated: {why}")).with("truncated", true)
    } else {
        Outcome::reject(format!("{why}, before any image block"))
    }
}

fn structural(at: usize, images: u64, why: &str) -> Outcome {
    if images > 0 {
        Outcome::partial(at as u64, format!("the block chain broke: {why}"))
    } else {
        Outcome::reject(format!("the block chain broke before any image: {why}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate::Status;

    fn gif(version: &[u8; 3], payload_len: usize, extensions: usize) -> Vec<u8> {
        let mut v = b"GIF".to_vec();
        v.extend_from_slice(version);
        v.extend_from_slice(&64u16.to_le_bytes());
        v.extend_from_slice(&48u16.to_le_bytes());
        v.extend_from_slice(&[0xF1, 0x00, 0x00]);
        for rgb in [(0u8, 0u8, 0u8), (255, 0, 0), (0, 255, 0), (0, 0, 255)] {
            v.extend_from_slice(&[rgb.0, rgb.1, rgb.2]);
        }
        for _ in 0..extensions {
            v.extend_from_slice(&[0x21, 0xF9, 4, 0, 0, 0, 0, 0x00]);
        }
        v.push(0x2C);
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&64u16.to_le_bytes());
        v.extend_from_slice(&48u16.to_le_bytes());
        v.push(0x00);
        v.push(2); // LZW minimum code size
        let payload: Vec<u8> = (0..payload_len).map(|i| (i % 251) as u8).collect();
        for chunk in payload.chunks(255) {
            v.push(chunk.len() as u8);
            v.extend_from_slice(chunk);
        }
        v.push(0x00);
        v.push(0x3B);
        v
    }

    #[test]
    fn accepts_a_well_formed_gif() {
        let g = gif(b"89a", 4000, 1);
        let out = validate(&g);
        assert_eq!(out.status, Status::Valid, "{}", out.detail);
        assert_eq!(out.length, g.len() as u64);
        assert_eq!(out.evidence_of("version"), Some("89a"));
        assert_eq!(out.evidence_of("images"), Some("1"));
        assert_eq!(out.evidence_of("extensions"), Some("1"));
    }

    #[test]
    fn accepts_gif87a_too() {
        let g = gif(b"87a", 1000, 0);
        assert_eq!(validate(&g).status, Status::Valid);
    }

    /// The reason the walk exists: 0x3B occurs constantly inside LZW data, so a
    /// carver that searched for the first one would truncate the file.
    #[test]
    fn a_trailer_byte_inside_the_image_data_does_not_end_the_file() {
        let g = gif(b"89a", 3000, 0);
        // The payload is 0..251 repeating, so it certainly contains 0x3B.
        let first = g.iter().position(|b| *b == 0x3B).expect("a 0x3B somewhere");
        assert!(
            first < g.len() - 1,
            "the test needs a 0x3B before the trailer to be meaningful"
        );
        let out = validate(&g);
        assert_eq!(out.status, Status::Valid, "{}", out.detail);
        assert_eq!(out.length, g.len() as u64, "stopped at an interior 0x3B");
    }

    #[test]
    fn trailing_bytes_do_not_extend_the_length() {
        let mut g = gif(b"89a", 2000, 0);
        let real = g.len();
        g.extend_from_slice(&[0xAB; 4096]);
        assert_eq!(validate(&g).length, real as u64);
    }

    #[test]
    fn a_truncated_gif_is_partial() {
        let g = gif(b"89a", 6000, 0);
        let out = validate(&g[..g.len() - 2000]);
        assert_eq!(out.status, Status::Partial, "{}", out.detail);
    }

    #[test]
    fn rejects_a_header_with_no_image_block() {
        let mut v = b"GIF89a".to_vec();
        v.extend_from_slice(&64u16.to_le_bytes());
        v.extend_from_slice(&48u16.to_le_bytes());
        v.extend_from_slice(&[0x00, 0x00, 0x00]);
        v.push(0x3B);
        assert_eq!(validate(&v).status, Status::Rejected);
    }

    #[test]
    fn rejects_a_zero_dimension() {
        let mut g = gif(b"89a", 100, 0);
        g[6..8].copy_from_slice(&0u16.to_le_bytes());
        assert_eq!(validate(&g).status, Status::Rejected);
    }
}
