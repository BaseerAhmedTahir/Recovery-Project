//! FAT directory entries, including long-filename chain recovery.
//!
//! A FAT directory is an array of 32-byte entries. A file with a long name is
//! stored as a run of LFN entries immediately followed by its 8.3 entry:
//!
//! ```text
//!   [LFN ord=3 |0x40] [LFN ord=2] [LFN ord=1] [8.3 entry]
//! ```
//!
//! The LFN entries appear in *reverse* order, so the entry physically nearest
//! the 8.3 entry holds the first 13 characters of the name.
//!
//! # Recovering a deleted name
//!
//! Deleting a file sets the first byte of *every* entry in the set - the 8.3
//! entry and all its LFN entries - to `0xE5`. For the 8.3 entry that destroys
//! the first character of the name. For an LFN entry it destroys the ordinal
//! byte, but **not the name characters**, which live at offsets 1-10, 14-25 and
//! 28-31 and survive intact.
//!
//! So the long name is fully recoverable: walk backwards from the 8.3 entry
//! collecting consecutive deleted LFN entries and rebuild the name from their
//! physical order rather than their lost ordinals. Falling back to `_` for the
//! first character is only correct when there is no LFN chain at all, and doing
//! it unconditionally is the difference between passing and failing a
//! name-accuracy bar.
//!
//! The 8.3 checksum stored in each LFN entry lets us both validate that the
//! chain really belongs to this entry and brute-force the lost first character
//! of the short name, since only one byte value in 256 normally reproduces it.

use crate::entry::Timestamps;

pub const ENTRY_SIZE: usize = 32;
pub const DELETED_MARKER: u8 = 0xE5;
pub const END_OF_DIRECTORY: u8 = 0x00;

pub const ATTR_READ_ONLY: u8 = 0x01;
pub const ATTR_HIDDEN: u8 = 0x02;
pub const ATTR_SYSTEM: u8 = 0x04;
pub const ATTR_VOLUME_ID: u8 = 0x08;
pub const ATTR_DIRECTORY: u8 = 0x10;
pub const ATTR_ARCHIVE: u8 = 0x20;
/// An LFN entry is marked by all four of read-only, hidden, system, volume-id.
pub const ATTR_LONG_NAME: u8 = 0x0F;

/// The 8.3 checksum every LFN entry carries.
pub fn short_name_checksum(name: &[u8; 11]) -> u8 {
    let mut sum: u8 = 0;
    for &b in name.iter() {
        sum = ((sum & 1) << 7).wrapping_add(sum >> 1).wrapping_add(b);
    }
    sum
}

/// Brute-force the first byte of a deleted 8.3 name from the LFN checksum.
///
/// Byte 0 was overwritten with 0xE5 on delete. Every other byte survives, and
/// the checksum is stored in each LFN entry, so trying all 256 values recovers
/// the original character. Returns every candidate that reproduces the
/// checksum; more than one is possible but uncommon.
pub fn recover_first_byte(name: &[u8; 11], target_checksum: u8) -> Vec<u8> {
    let mut candidates = Vec::new();
    let mut probe = *name;
    for b in 0u8..=255 {
        probe[0] = b;
        if short_name_checksum(&probe) == target_checksum {
            candidates.push(b);
        }
    }
    candidates
}

/// Is this a plausible first character for an 8.3 name?
fn plausible_short_name_byte(b: u8) -> bool {
    b.is_ascii_uppercase()
        || b.is_ascii_digit()
        || matches!(
            b,
            b'_' | b'-' | b'~' | b'!' | b'#' | b'$' | b'%' | b'&' | b'@' | b'^'
        )
        || b >= 0x80
}

/// A raw 32-byte directory entry, classified.
#[derive(Clone, Debug)]
pub enum RawEntry {
    /// End of the directory; nothing beyond this was ever used.
    End,
    /// A long-filename fragment.
    Lfn {
        /// Ordinal, or `None` when the entry is deleted and it was destroyed.
        ordinal: Option<u8>,
        last_in_chain: bool,
        deleted: bool,
        checksum: u8,
        /// The 13 UTF-16 code units this fragment carries.
        units: Vec<u16>,
    },
    /// An 8.3 entry for a file or directory.
    Short(ShortEntry),
    /// A volume label or something we do not model.
    Other,
}

#[derive(Clone, Debug)]
pub struct ShortEntry {
    pub raw_name: [u8; 11],
    pub attributes: u8,
    pub deleted: bool,
    pub first_cluster: u32,
    pub size: u32,
    pub timestamps: Timestamps,
}

impl ShortEntry {
    pub fn is_directory(&self) -> bool {
        self.attributes & ATTR_DIRECTORY != 0
    }

    /// Render the 8.3 name. `first_byte` overrides byte 0, which is how a
    /// recovered first character is applied to a deleted entry.
    pub fn short_name(&self, first_byte: Option<u8>) -> String {
        let mut raw = self.raw_name;
        if let Some(b) = first_byte {
            raw[0] = b;
        }
        let base: String = raw[0..8]
            .iter()
            .map(|&b| b as char)
            .collect::<String>()
            .trim_end()
            .to_string();
        let ext: String = raw[8..11]
            .iter()
            .map(|&b| b as char)
            .collect::<String>()
            .trim_end()
            .to_string();
        if ext.is_empty() {
            base
        } else {
            format!("{base}.{ext}")
        }
    }
}

/// Classify one 32-byte entry.
pub fn parse_raw(e: &[u8]) -> RawEntry {
    if e.len() < ENTRY_SIZE {
        return RawEntry::Other;
    }
    if e[0] == END_OF_DIRECTORY {
        return RawEntry::End;
    }

    let attributes = e[11];
    let deleted = e[0] == DELETED_MARKER;

    // The LFN attribute check must come before anything else: an LFN entry's
    // other fields overlap the 8.3 layout and would parse as nonsense.
    if attributes & 0x3F == ATTR_LONG_NAME {
        let seq = e[0];
        let mut units = Vec::with_capacity(13);
        for &r in &[1usize, 3, 5, 7, 9] {
            units.push(u16::from_le_bytes([e[r], e[r + 1]]));
        }
        for r in (14usize..26).step_by(2) {
            units.push(u16::from_le_bytes([e[r], e[r + 1]]));
        }
        for r in (28usize..32).step_by(2) {
            units.push(u16::from_le_bytes([e[r], e[r + 1]]));
        }
        return RawEntry::Lfn {
            // On delete the sequence byte becomes 0xE5, taking the ordinal
            // with it. Physical position is then the only ordering available.
            ordinal: if deleted { None } else { Some(seq & 0x1F) },
            last_in_chain: !deleted && (seq & 0x40) != 0,
            deleted,
            checksum: e[13],
            units,
        };
    }

    if attributes & ATTR_VOLUME_ID != 0 {
        return RawEntry::Other;
    }
    // "." and ".." entries carry no recoverable name.
    if e[0] == b'.' {
        return RawEntry::Other;
    }

    let mut raw_name = [0u8; 11];
    raw_name.copy_from_slice(&e[0..11]);

    RawEntry::Short(ShortEntry {
        raw_name,
        attributes,
        deleted,
        first_cluster: ((u16::from_le_bytes([e[20], e[21]]) as u32) << 16)
            | u16::from_le_bytes([e[26], e[27]]) as u32,
        size: u32::from_le_bytes([e[28], e[29], e[30], e[31]]),
        timestamps: Timestamps {
            created: dos_to_unix_nanos(
                u16::from_le_bytes([e[16], e[17]]),
                u16::from_le_bytes([e[14], e[15]]),
                e[13],
            ),
            modified: dos_to_unix_nanos(
                u16::from_le_bytes([e[24], e[25]]),
                u16::from_le_bytes([e[22], e[23]]),
                0,
            ),
            accessed: dos_to_unix_nanos(u16::from_le_bytes([e[18], e[19]]), 0, 0),
            mft_changed: None,
            // FAT timestamps are local time with no timezone recorded at all,
            // so the instant is only as accurate as the writing machine's
            // clock setting. Milestone 8's deletion-date filter must not treat
            // these as UTC.
            utc_known: false,
        },
    })
}

/// The result of rebuilding one file's entry set.
#[derive(Clone, Debug)]
pub struct RecoveredName {
    pub long_name: Option<String>,
    pub short_name: String,
    /// The recovered first character of the 8.3 name, when a deleted entry's
    /// checksum identified it.
    pub recovered_first_byte: Option<u8>,
    /// The LFN chain's checksum matched the 8.3 name, so the two belong
    /// together.
    pub checksum_validated: bool,
    pub notes: Vec<String>,
}

impl RecoveredName {
    pub fn best(&self) -> String {
        self.long_name
            .clone()
            .unwrap_or_else(|| self.short_name.clone())
    }
}

/// Rebuild a name from an 8.3 entry and the LFN entries that precede it.
///
/// `preceding` must be the entries immediately before the short entry, in
/// on-disk order (so the last element is the one adjacent to it).
pub fn rebuild_name(short: &ShortEntry, preceding: &[RawEntry]) -> RecoveredName {
    let mut notes = Vec::new();

    // Collect the trailing run of LFN entries, nearest-first.
    let mut chain: Vec<(&Option<u8>, &Vec<u16>, u8)> = Vec::new();
    for e in preceding.iter().rev() {
        match e {
            RawEntry::Lfn {
                ordinal,
                units,
                checksum,
                deleted,
                ..
            } => {
                // A live chain must not be attributed to a deleted entry or
                // vice versa; mixing them would splice unrelated names.
                if *deleted != short.deleted {
                    break;
                }
                chain.push((ordinal, units, *checksum));
            }
            _ => break,
        }
    }

    if chain.is_empty() {
        // No long name. Only now is the `_` placeholder the right answer.
        let mut short_name = short.short_name(None);
        if short.deleted {
            short_name = short.short_name(Some(b'_'));
            notes.push(
                "no long-filename entries survive, so the first character of the 8.3 name \
                 is unrecoverable and is shown as '_'"
                    .to_string(),
            );
        }
        return RecoveredName {
            long_name: None,
            short_name,
            recovered_first_byte: None,
            checksum_validated: false,
            notes,
        };
    }

    let stored_checksum = chain[0].2;

    // Validate, and recover the lost first byte while we are at it.
    let mut recovered_first_byte = None;
    let mut checksum_validated = false;
    if short.deleted {
        let candidates: Vec<u8> = recover_first_byte(&short.raw_name, stored_checksum)
            .into_iter()
            .filter(|&b| plausible_short_name_byte(b))
            .collect();
        match candidates.len() {
            0 => notes.push(
                "the long-filename checksum does not match this 8.3 entry under any first \
                 character; the chain may belong to a different file"
                    .to_string(),
            ),
            1 => {
                recovered_first_byte = Some(candidates[0]);
                checksum_validated = true;
            }
            n => {
                recovered_first_byte = Some(candidates[0]);
                checksum_validated = true;
                notes.push(format!(
                    "{n} first characters reproduce the 8.3 checksum; using {:?}",
                    candidates[0] as char
                ));
            }
        }
    } else {
        checksum_validated = short_name_checksum(&short.raw_name) == stored_checksum;
        if !checksum_validated {
            notes.push("the long-filename checksum does not match this 8.3 entry".to_string());
        }
    }

    // Rebuild the name. chain[0] is nearest the 8.3 entry and therefore holds
    // the FIRST 13 characters, so concatenating in collected order is correct
    // whether or not the ordinals survived.
    let mut units: Vec<u16> = Vec::new();
    for (_, u, _) in &chain {
        units.extend_from_slice(u);
    }
    // The name is NUL-terminated and padded with 0xFFFF.
    let end = units
        .iter()
        .position(|&u| u == 0x0000 || u == 0xFFFF)
        .unwrap_or(units.len());
    units.truncate(end);

    let long_name: String = char::decode_utf16(units.iter().copied())
        .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect();

    if short.deleted {
        notes.push(format!(
            "long name recovered from {} surviving LFN entries",
            chain.len()
        ));
    }

    RecoveredName {
        long_name: if long_name.is_empty() {
            None
        } else {
            Some(long_name)
        },
        short_name: short.short_name(recovered_first_byte),
        recovered_first_byte,
        checksum_validated,
        notes,
    }
}

/// DOS date/time to Unix epoch nanoseconds, treating the value as UTC.
///
/// FAT records local time with no offset, so this cannot be exact. The
/// `utc_known: false` flag on [`Timestamps`] carries that caveat forward.
pub fn dos_to_unix_nanos(date: u16, time: u16, tenths: u8) -> Option<i64> {
    if date == 0 {
        return None;
    }
    let year = 1980 + ((date >> 9) & 0x7F) as i64;
    let month = ((date >> 5) & 0x0F) as i64;
    let day = (date & 0x1F) as i64;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let hour = ((time >> 11) & 0x1F) as i64;
    let minute = ((time >> 5) & 0x3F) as i64;
    // FAT stores seconds in two-second units; `tenths` adds finer resolution
    // for the creation time only.
    let second = ((time & 0x1F) * 2) as i64 + (tenths as i64 / 100);

    let days = days_from_civil(year, month, day);
    Some(((days * 86_400) + hour * 3600 + minute * 60 + second) * 1_000_000_000)
}

/// Days since the Unix epoch (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lfn_entry(seq: u8, checksum: u8, chars: &[u16]) -> Vec<u8> {
        let mut e = vec![0u8; 32];
        e[0] = seq;
        e[11] = ATTR_LONG_NAME;
        e[13] = checksum;
        let mut it = chars.iter().copied().chain(std::iter::repeat(0xFFFF));
        for &r in &[1usize, 3, 5, 7, 9] {
            e[r..r + 2].copy_from_slice(&it.next().unwrap().to_le_bytes());
        }
        for r in (14usize..26).step_by(2) {
            e[r..r + 2].copy_from_slice(&it.next().unwrap().to_le_bytes());
        }
        for r in (28usize..32).step_by(2) {
            e[r..r + 2].copy_from_slice(&it.next().unwrap().to_le_bytes());
        }
        e
    }

    fn short_entry(name: &[u8; 11], attrs: u8, cluster: u32, size: u32) -> Vec<u8> {
        let mut e = vec![0u8; 32];
        e[0..11].copy_from_slice(name);
        e[11] = attrs;
        e[20..22].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
        e[26..28].copy_from_slice(&((cluster & 0xFFFF) as u16).to_le_bytes());
        e[28..32].copy_from_slice(&size.to_le_bytes());
        e[16..18].copy_from_slice(&0x5000u16.to_le_bytes()); // create date
        e[24..26].copy_from_slice(&0x5000u16.to_le_bytes()); // modify date
        e
    }

    /// Build the on-disk entry set for a long name, live or deleted.
    fn entry_set(name: &str, short: &[u8; 11], deleted: bool) -> (Vec<RawEntry>, ShortEntry) {
        let units: Vec<u16> = name.encode_utf16().collect();
        let checksum = short_name_checksum(short);
        let chunks: Vec<&[u16]> = units.chunks(13).collect();
        let n = chunks.len();

        let mut raws = Vec::new();
        // Physical order: highest ordinal first.
        for (i, chunk) in chunks.iter().enumerate().rev() {
            let ord = (i + 1) as u8;
            let seq = if i == n - 1 { ord | 0x40 } else { ord };
            let mut e = lfn_entry(seq, checksum, chunk);
            if deleted {
                e[0] = DELETED_MARKER;
            }
            raws.push(parse_raw(&e));
        }

        let mut s = short_entry(short, ATTR_ARCHIVE, 100, 4321);
        if deleted {
            s[0] = DELETED_MARKER;
        }
        let short_parsed = match parse_raw(&s) {
            RawEntry::Short(se) => se,
            other => panic!("expected a short entry, got {other:?}"),
        };
        (raws, short_parsed)
    }

    #[test]
    fn checksum_matches_the_documented_algorithm() {
        // A known pair: "PHOTOG~1JPG".
        let name = b"PHOTOG~1JPG";
        let c = short_name_checksum(name);
        // Recomputing by hand must agree.
        let mut sum: u8 = 0;
        for &b in name.iter() {
            sum = ((sum & 1) << 7).wrapping_add(sum >> 1).wrapping_add(b);
        }
        assert_eq!(c, sum);
    }

    /// The headline case: a deleted long name must come back intact.
    #[test]
    fn recovers_a_deleted_long_name_from_the_lfn_chain() {
        let name = "photograph of the pier.jpg";
        let (raws, short) = entry_set(name, b"PHOTOG~1JPG", true);
        let rec = rebuild_name(&short, &raws);
        assert_eq!(
            rec.long_name.as_deref(),
            Some(name),
            "the LFN entries survive deletion and hold the whole name"
        );
        assert!(rec.checksum_validated);
    }

    #[test]
    fn recovers_the_lost_first_character_of_the_short_name() {
        let (raws, short) = entry_set("readme file.txt", b"README~1TXT", true);
        let rec = rebuild_name(&short, &raws);
        assert_eq!(
            rec.recovered_first_byte,
            Some(b'R'),
            "the LFN checksum identifies the byte that 0xE5 destroyed"
        );
        assert!(rec.short_name.starts_with('R'));
    }

    #[test]
    fn recovers_non_ascii_names_including_surrogate_pairs() {
        for name in [
            "日本語のファイル.jpg",
            "café_niño_αβγ.txt",
            "Кириллица_Ωμέγα.png",
            "emoji_\u{1F389}\u{1F525}_test.txt",
            "ملف_عربي.txt",
        ] {
            let (raws, short) = entry_set(name, b"XXXXXX~1TXT", true);
            let rec = rebuild_name(&short, &raws);
            assert_eq!(
                rec.long_name.as_deref(),
                Some(name),
                "failed to recover {name:?}"
            );
        }
    }

    /// 250 characters needs 20 LFN entries, the format's maximum.
    #[test]
    fn recovers_a_name_at_the_length_limit() {
        let name = format!("long_{}_.txt", "n".repeat(240));
        assert_eq!(name.len(), 250);
        let (raws, short) = entry_set(&name, b"LONGNA~1TXT", true);
        assert_eq!(
            raws.len(),
            20,
            "250 chars over 13 per entry needs 20 entries"
        );
        let rec = rebuild_name(&short, &raws);
        assert_eq!(rec.long_name.as_deref(), Some(name.as_str()));
    }

    #[test]
    fn chain_order_is_taken_from_physical_position_not_ordinals() {
        // Deleted entries have no ordinals at all, so if the code depended on
        // them the name would come out scrambled.
        let name = "abcdefghijklmnopqrstuvwxyz0123456789.txt";
        let (raws, short) = entry_set(name, b"ABCDEF~1TXT", true);
        for r in &raws {
            match r {
                RawEntry::Lfn { ordinal, .. } => {
                    assert!(ordinal.is_none(), "a deleted LFN entry has no ordinal")
                }
                _ => panic!("expected LFN entries"),
            }
        }
        assert_eq!(rebuild_name(&short, &raws).long_name.as_deref(), Some(name));
    }

    #[test]
    fn falls_back_to_underscore_only_without_an_lfn_chain() {
        let mut s = short_entry(b"REPORT  PDF", ATTR_ARCHIVE, 50, 900);
        s[0] = DELETED_MARKER;
        let short = match parse_raw(&s) {
            RawEntry::Short(se) => se,
            _ => unreachable!(),
        };
        let rec = rebuild_name(&short, &[]);
        assert_eq!(rec.long_name, None);
        assert!(
            rec.short_name.starts_with('_'),
            "with no LFN there is nothing to recover the first character from"
        );
        assert!(rec.notes.iter().any(|n| n.contains("unrecoverable")));
    }

    #[test]
    fn a_live_chain_is_not_spliced_onto_a_deleted_entry() {
        // Live LFN entries followed by a deleted 8.3 entry must not combine.
        let (live_raws, _) = entry_set("live name.txt", b"LIVENA~1TXT", false);
        let mut s = short_entry(b"REPORT  PDF", ATTR_ARCHIVE, 50, 900);
        s[0] = DELETED_MARKER;
        let short = match parse_raw(&s) {
            RawEntry::Short(se) => se,
            _ => unreachable!(),
        };
        let rec = rebuild_name(&short, &live_raws);
        assert_eq!(
            rec.long_name, None,
            "a live chain belongs to a different file"
        );
    }

    #[test]
    fn parses_cluster_numbers_from_both_halves() {
        let e = short_entry(b"FILE    TXT", ATTR_ARCHIVE, 0x0002_ABCD, 1234);
        match parse_raw(&e) {
            RawEntry::Short(s) => {
                assert_eq!(s.first_cluster, 0x0002_ABCD, "high and low words combine");
                assert_eq!(s.size, 1234);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn detects_end_of_directory_and_lfn_entries() {
        assert!(matches!(parse_raw(&[0u8; 32]), RawEntry::End));
        let l = lfn_entry(0x41, 0xAB, &[b'a' as u16]);
        assert!(matches!(parse_raw(&l), RawEntry::Lfn { .. }));
    }

    #[test]
    fn volume_labels_and_dot_entries_are_ignored() {
        let mut v = short_entry(b"NO NAME    ", ATTR_VOLUME_ID, 0, 0);
        v[11] = ATTR_VOLUME_ID;
        assert!(matches!(parse_raw(&v), RawEntry::Other));

        let d = short_entry(b".          ", ATTR_DIRECTORY, 2, 0);
        assert!(matches!(parse_raw(&d), RawEntry::Other));
    }

    #[test]
    fn dos_timestamps_convert_and_are_flagged_as_local_time() {
        // 2024-01-01 00:00:00 -> date = (44 << 9) | (1 << 5) | 1
        let date: u16 = (44 << 9) | (1 << 5) | 1;
        let ns = dos_to_unix_nanos(date, 0, 0).expect("valid date");
        assert_eq!(ns, 1_704_067_200 * 1_000_000_000, "2024-01-01T00:00:00");

        let e = short_entry(b"FILE    TXT", ATTR_ARCHIVE, 2, 0);
        match parse_raw(&e) {
            RawEntry::Short(s) => assert!(
                !s.timestamps.utc_known,
                "FAT has no timezone, and callers must know that"
            ),
            _ => unreachable!(),
        }
        assert_eq!(dos_to_unix_nanos(0, 0, 0), None, "a zero date means unset");
    }
}
