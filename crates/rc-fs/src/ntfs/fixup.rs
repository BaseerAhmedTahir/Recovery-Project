//! NTFS update sequence (fixup) arrays.
//!
//! **Every multi-sector NTFS structure must go through this before anything
//! else reads it.** That means MFT records (`FILE`), index buffers (`INDX`),
//! `$LogFile` restart pages (`RSTR`) and log record pages (`RCRD`).
//!
//! NTFS protects these against torn writes by replacing the last two bytes of
//! every 512-byte sector with a per-record update sequence number, and stashing
//! the displaced originals in an array in the record header. If the record was
//! written completely, every sector still ends with the same USN. If a write
//! was torn, one sector ends with a stale value and the record is known-bad.
//!
//! Skipping this step is the classic NTFS parsing bug, and it is nasty
//! precisely because the parser still *mostly* works: only two bytes in every
//! 512 are wrong. Those two bytes land in the middle of runlists and attribute
//! headers, so what comes out is not an obvious error but plausible-looking
//! garbage - a data run pointing at the wrong cluster, a length field off by a
//! multiple of 65536. So this module refuses to hand back a record whose USNs
//! do not all match, rather than repairing what it can and carrying on.

use crate::error::{FsError, Result};

/// Result of validating and applying a fixup array.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FixupInfo {
    pub usn: u16,
    pub sectors: usize,
}

/// Validate and apply the update sequence array to `buf` in place.
///
/// `usa_offset` and `usa_count` come from the record header (offsets 0x04 and
/// 0x06 of a `FILE`/`INDX` record). `sector_size` is the volume's sector size,
/// not the cluster size.
///
/// On success every sector's trailing two bytes have been restored to their
/// real values. On failure `buf` is left untouched and the record must be
/// treated as unreadable.
pub fn apply_fixups(
    buf: &mut [u8],
    usa_offset: u16,
    usa_count: u16,
    sector_size: usize,
) -> Result<FixupInfo> {
    let usa_offset = usa_offset as usize;
    let usa_count = usa_count as usize;

    // usa_count counts the USN itself plus one entry per protected sector.
    if usa_count < 2 {
        return Err(FsError::Fixup {
            detail: format!("update sequence array count {usa_count} is too small"),
        });
    }
    let sectors = usa_count - 1;

    if sector_size == 0 || !sector_size.is_power_of_two() {
        return Err(FsError::Fixup {
            detail: format!("invalid sector size {sector_size}"),
        });
    }
    if sectors * sector_size > buf.len() {
        return Err(FsError::Fixup {
            detail: format!(
                "update sequence array covers {} bytes but the record is only {}",
                sectors * sector_size,
                buf.len()
            ),
        });
    }
    // The array itself must lie inside the record and not overlap the first
    // sector's protected tail.
    let usa_end = usa_offset + usa_count * 2;
    if usa_end > buf.len() {
        return Err(FsError::Fixup {
            detail: format!(
                "update sequence array at {usa_offset}..{usa_end} runs past the record"
            ),
        });
    }

    let usn = u16::from_le_bytes([buf[usa_offset], buf[usa_offset + 1]]);

    // Pass 1: verify every protected sector ends with the record's USN.
    // Do this before mutating anything so a bad record leaves buf unchanged.
    for i in 0..sectors {
        let tail = (i + 1) * sector_size - 2;
        let found = u16::from_le_bytes([buf[tail], buf[tail + 1]]);
        if found != usn {
            return Err(FsError::Fixup {
                detail: format!(
                    "sector {i} ends with update sequence number 0x{found:04X}, expected \
                     0x{usn:04X}; the record was torn by an interrupted write and its \
                     contents cannot be trusted"
                ),
            });
        }
    }

    // Pass 2: put the real bytes back.
    for i in 0..sectors {
        let entry = usa_offset + 2 + i * 2;
        let original = [buf[entry], buf[entry + 1]];
        let tail = (i + 1) * sector_size - 2;
        buf[tail] = original[0];
        buf[tail + 1] = original[1];
    }

    Ok(FixupInfo { usn, sectors })
}

/// Read the standard multi-sector header fields shared by FILE and INDX.
///
/// Returns `(signature, usa_offset, usa_count)`.
pub fn read_multi_sector_header(buf: &[u8]) -> Option<([u8; 4], u16, u16)> {
    if buf.len() < 8 {
        return None;
    }
    let mut sig = [0u8; 4];
    sig.copy_from_slice(&buf[0..4]);
    Some((
        sig,
        u16::from_le_bytes([buf[4], buf[5]]),
        u16::from_le_bytes([buf[6], buf[7]]),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a record with a correctly applied fixup array.
    ///
    /// `payload` is the record's real content; this stamps the USN over each
    /// sector tail and records the displaced bytes in the array, exactly as
    /// NTFS does when writing.
    fn make_record(
        sectors: usize,
        sector_size: usize,
        usn: u16,
        usa_offset: usize,
    ) -> (Vec<u8>, Vec<[u8; 2]>) {
        let mut buf = vec![0u8; sectors * sector_size];
        // Recognisable content so a mis-applied fixup is obvious.
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        buf[0..4].copy_from_slice(b"FILE");
        buf[4..6].copy_from_slice(&(usa_offset as u16).to_le_bytes());
        buf[6..8].copy_from_slice(&((sectors + 1) as u16).to_le_bytes());
        buf[usa_offset..usa_offset + 2].copy_from_slice(&usn.to_le_bytes());

        let mut originals = Vec::new();
        for i in 0..sectors {
            let tail = (i + 1) * sector_size - 2;
            let orig = [buf[tail], buf[tail + 1]];
            originals.push(orig);
            // Stash the originals in the array, stamp the USN over the tail.
            let entry = usa_offset + 2 + i * 2;
            buf[entry] = orig[0];
            buf[entry + 1] = orig[1];
            buf[tail..tail + 2].copy_from_slice(&usn.to_le_bytes());
        }
        (buf, originals)
    }

    #[test]
    fn restores_the_displaced_bytes() {
        let (mut buf, originals) = make_record(2, 512, 0xBEEF, 0x30);
        let info = apply_fixups(&mut buf, 0x30, 3, 512).expect("valid record");
        assert_eq!(info.usn, 0xBEEF);
        assert_eq!(info.sectors, 2);

        for (i, orig) in originals.iter().enumerate() {
            let tail = (i + 1) * 512 - 2;
            assert_eq!(
                [buf[tail], buf[tail + 1]],
                *orig,
                "sector {i} tail was not restored"
            );
        }
    }

    /// The bug this module exists to prevent: without fixups, two bytes per
    /// sector are the USN instead of real data.
    #[test]
    fn skipping_fixups_would_corrupt_two_bytes_per_sector() {
        let (buf, originals) = make_record(2, 512, 0xBEEF, 0x30);
        for (i, orig) in originals.iter().enumerate() {
            let tail = (i + 1) * 512 - 2;
            assert_eq!(
                u16::from_le_bytes([buf[tail], buf[tail + 1]]),
                0xBEEF,
                "before fixups the tail holds the USN, not data"
            );
            assert_ne!(
                [buf[tail], buf[tail + 1]],
                *orig,
                "and that differs from the real content"
            );
        }
    }

    #[test]
    fn rejects_a_torn_record() {
        let (mut buf, _) = make_record(4, 512, 0xBEEF, 0x30);
        // Simulate an interrupted write: the third sector was never updated,
        // so it still carries the previous generation's USN.
        let tail = 3 * 512 - 2;
        buf[tail..tail + 2].copy_from_slice(&0x1234u16.to_le_bytes());

        let before = buf.clone();
        let err = apply_fixups(&mut buf, 0x30, 5, 512).expect_err("torn record must be rejected");
        assert!(
            matches!(err, FsError::Fixup { .. }),
            "expected a fixup error, got {err:?}"
        );
        assert_eq!(
            buf, before,
            "a rejected record must be left untouched, not half-repaired"
        );
    }

    #[test]
    fn rejects_an_array_that_overruns_the_record() {
        let mut buf = vec![0u8; 1024];
        // Claims to protect 100 sectors of 512 bytes in a 1 KiB record.
        let err = apply_fixups(&mut buf, 0x30, 101, 512).expect_err("must reject");
        assert!(matches!(err, FsError::Fixup { .. }));
    }

    #[test]
    fn rejects_an_array_offset_past_the_record() {
        let mut buf = vec![0u8; 1024];
        let err = apply_fixups(&mut buf, 2000, 3, 512).expect_err("must reject");
        assert!(matches!(err, FsError::Fixup { .. }));
    }

    #[test]
    fn rejects_a_degenerate_count() {
        let mut buf = vec![0u8; 1024];
        assert!(apply_fixups(&mut buf, 0x30, 0, 512).is_err());
        assert!(apply_fixups(&mut buf, 0x30, 1, 512).is_err());
    }

    #[test]
    fn handles_a_four_sector_record() {
        // A 4 KiB MFT record on a 512-byte-sector volume: 8 protected sectors.
        let (mut buf, originals) = make_record(8, 512, 0x0042, 0x30);
        apply_fixups(&mut buf, 0x30, 9, 512).expect("valid");
        for (i, orig) in originals.iter().enumerate() {
            let tail = (i + 1) * 512 - 2;
            assert_eq!([buf[tail], buf[tail + 1]], *orig, "sector {i}");
        }
    }

    #[test]
    fn reads_the_multi_sector_header() {
        let (buf, _) = make_record(2, 512, 0xBEEF, 0x30);
        let (sig, off, count) = read_multi_sector_header(&buf).expect("header");
        assert_eq!(&sig, b"FILE");
        assert_eq!(off, 0x30);
        assert_eq!(count, 3);
    }
}
