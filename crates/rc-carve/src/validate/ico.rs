//! ICO/CUR validator: the directory has to describe images that exist.
//!
//! This validator exists because of a measurement rather than a hunch. On the
//! quick-formatted fixture, `ico` produced 3782 of 4060 candidates - 93% of the
//! carve - against 16 real SQLite files and 47 real JPEGs. The reason is
//! arithmetic: the signature is `00 00 01 00`, the fixture image is 98.6%
//! zeros, and a four-byte pattern that is three-quarters zeros matches
//! constantly in free space.
//!
//! An icon file is a six-byte header followed by a directory of 16-byte
//! entries, each naming a byte range holding either a PNG or a Windows
//! bitmap. Those ranges have to be inside the file, they have to not overlap
//! the directory, and the bytes they point at have to look like an image. A
//! run of zeros satisfies none of that: the image count is the first thing
//! read, and zero images is not an icon.

use super::{le16, le32, Outcome};

const HEADER: usize = 6;
const ENTRY: usize = 16;

/// An icon holding more images than this is not something anyone shipped;
/// Windows itself tops out well below it. A large count here is the signature
/// of noise rather than of an unusual file.
const MAX_IMAGES: u16 = 64;

pub fn validate(d: &[u8]) -> Outcome {
    if d.len() < HEADER {
        return Outcome::reject("shorter than an icon directory header");
    }
    if le16(d, 0) != Some(0) {
        return Outcome::reject("the reserved word is not zero");
    }
    let kind = le16(d, 2).unwrap_or(0);
    if !matches!(kind, 1 | 2) {
        return Outcome::reject(format!("image type {kind} is neither icon (1) nor cursor (2)"));
    }
    let count = le16(d, 4).unwrap_or(0);
    if count == 0 {
        // The overwhelmingly common false positive: `00 00 01 00` followed by
        // more zeros in free space.
        return Outcome::reject("the directory declares zero images");
    }
    if count > MAX_IMAGES {
        return Outcome::reject(format!("the directory declares {count} images"));
    }

    let dir_end = HEADER + ENTRY * count as usize;
    if dir_end > d.len() {
        return Outcome::reject("the directory extends past the available data");
    }

    let mut end = dir_end as u64;
    let mut png_images = 0u64;
    let mut dib_images = 0u64;

    for i in 0..count as usize {
        let at = HEADER + i * ENTRY;
        // Byte 3 of every entry is reserved and zero in every writer.
        if d[at + 3] != 0 {
            return Outcome::reject("an entry's reserved byte is not zero");
        }
        let planes = le16(d, at + 4).unwrap_or(0);
        let bitcount = le16(d, at + 6).unwrap_or(0);
        let size = le32(d, at + 8).unwrap_or(0) as u64;
        let offset = le32(d, at + 12).unwrap_or(0) as u64;

        // For a cursor these two fields are the hotspot coordinates instead,
        // so they are only constrained for icons.
        if kind == 1 && (planes > 1 || bitcount > 32) {
            return Outcome::reject(format!(
                "an entry declares {planes} planes at {bitcount} bits per pixel"
            ));
        }
        if size == 0 {
            return Outcome::reject("an entry declares a zero-byte image");
        }
        if offset < dir_end as u64 {
            return Outcome::reject("an entry's image data starts inside the directory");
        }
        let Some(entry_end) = offset.checked_add(size) else {
            return Outcome::reject("an entry's offset and size overflow");
        };
        if entry_end > d.len() as u64 {
            // Truncated rather than bogus, provided what we have so far held
            // together. The directory itself is established.
            return Outcome::partial(
                dir_end as u64,
                format!(
                    "the directory is intact but image {i} runs to {entry_end}, past the \
                     {} bytes available; length runs to the end of the directory",
                    d.len()
                ),
            )
            .with("images", count)
            .with("truncated", true);
        }

        // The payload is either an embedded PNG or a bitmap info header.
        let payload = &d[offset as usize..entry_end as usize];
        if payload.starts_with(&super::png::SIGNATURE) {
            png_images += 1;
        } else if le32(payload, 0) == Some(40) {
            dib_images += 1;
        } else {
            return Outcome::reject(format!(
                "image {i} is neither a PNG nor a 40-byte BITMAPINFOHEADER"
            ));
        }

        end = end.max(entry_end);
    }

    Outcome::valid(end)
        .with("images", count)
        .with("kind", if kind == 1 { "icon" } else { "cursor" })
        .with("png_images", png_images)
        .with("dib_images", dib_images)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate::Status;

    fn dib(w: u8, h: u8) -> Vec<u8> {
        let mut v = 40u32.to_le_bytes().to_vec();
        v.extend_from_slice(&(w as i32).to_le_bytes());
        v.extend_from_slice(&((h as i32) * 2).to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&32u16.to_le_bytes());
        v.resize(40 + (w as usize * h as usize * 4), 0x40);
        v
    }

    fn ico(images: &[Vec<u8>]) -> Vec<u8> {
        let n = images.len() as u16;
        let mut v = 0u16.to_le_bytes().to_vec();
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&n.to_le_bytes());
        let mut offset = HEADER + ENTRY * images.len();
        let mut dir = Vec::new();
        for img in images {
            dir.extend_from_slice(&[16, 16, 0, 0]);
            dir.extend_from_slice(&1u16.to_le_bytes());
            dir.extend_from_slice(&32u16.to_le_bytes());
            dir.extend_from_slice(&(img.len() as u32).to_le_bytes());
            dir.extend_from_slice(&(offset as u32).to_le_bytes());
            offset += img.len();
        }
        v.extend_from_slice(&dir);
        for img in images {
            v.extend_from_slice(img);
        }
        v
    }

    #[test]
    fn accepts_a_real_icon() {
        let f = ico(&[dib(16, 16)]);
        let out = validate(&f);
        assert_eq!(out.status, Status::Valid, "{}", out.detail);
        assert_eq!(out.length, f.len() as u64);
        assert_eq!(out.evidence_of("images"), Some("1"));
        assert_eq!(out.evidence_of("dib_images"), Some("1"));
    }

    #[test]
    fn accepts_a_multi_image_icon_with_an_embedded_png() {
        let mut png = super::super::png::SIGNATURE.to_vec();
        png.resize(64, 0);
        let f = ico(&[dib(16, 16), png]);
        let out = validate(&f);
        assert_eq!(out.status, Status::Valid, "{}", out.detail);
        assert_eq!(out.evidence_of("png_images"), Some("1"));
        assert_eq!(out.evidence_of("dib_images"), Some("1"));
    }

    /// The reason this validator exists: `00 00 01 00` inside a field of zeros
    /// was 93% of the candidates on the quick-formatted fixture.
    #[test]
    fn rejects_the_signature_followed_by_zeros() {
        let mut v = vec![0x00, 0x00, 0x01, 0x00];
        v.resize(4096, 0);
        let out = validate(&v);
        assert_eq!(out.status, Status::Rejected, "{}", out.detail);
        assert!(out.detail.contains("zero images"), "{}", out.detail);
    }

    #[test]
    fn rejects_chance_matches_in_pseudorandom_data() {
        let mut state = 0x1234_5678u32;
        let mut rejected = 0;
        let trials = 2000;
        for _ in 0..trials {
            let mut v = vec![0x00, 0x00, 0x01, 0x00];
            for _ in 0..512 {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                v.push(state as u8);
            }
            if validate(&v).status == Status::Rejected {
                rejected += 1;
            }
        }
        assert_eq!(rejected, trials, "every random match must be rejected");
    }

    #[test]
    fn trailing_bytes_do_not_extend_the_length() {
        let mut f = ico(&[dib(16, 16)]);
        let real = f.len();
        f.extend_from_slice(&[0xAA; 2048]);
        assert_eq!(validate(&f).length, real as u64);
    }

    #[test]
    fn a_truncated_icon_reports_the_directory_it_could_read() {
        let f = ico(&[dib(32, 32)]);
        let out = validate(&f[..HEADER + ENTRY + 8]);
        assert_eq!(out.status, Status::Partial, "{}", out.detail);
        assert_eq!(out.length, (HEADER + ENTRY) as u64);
    }

    #[test]
    fn rejects_an_image_pointing_into_its_own_directory() {
        let mut f = ico(&[dib(16, 16)]);
        f[HEADER + 12..HEADER + 16].copy_from_slice(&4u32.to_le_bytes());
        assert_eq!(validate(&f).status, Status::Rejected);
    }

    #[test]
    fn rejects_a_payload_that_is_not_an_image() {
        let mut junk = vec![0xEEu8; 64];
        junk[0..4].copy_from_slice(&12u32.to_le_bytes());
        let f = ico(&[junk]);
        assert_eq!(validate(&f).status, Status::Rejected);
    }
}
