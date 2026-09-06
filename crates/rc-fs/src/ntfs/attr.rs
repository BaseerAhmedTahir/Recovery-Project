//! NTFS attribute parsing.
//!
//! Everything in an MFT record after the header is a sequence of attributes,
//! each with a common header followed by either a resident value or a runlist.

use crate::entry::{Extent, Timestamps};
use crate::error::{FsError, Result};
use crate::ntfs::runlist::decode_runlist;

// Attribute type codes.
pub const ATTR_STANDARD_INFORMATION: u32 = 0x10;
pub const ATTR_ATTRIBUTE_LIST: u32 = 0x20;
pub const ATTR_FILE_NAME: u32 = 0x30;
pub const ATTR_OBJECT_ID: u32 = 0x40;
pub const ATTR_DATA: u32 = 0x80;
pub const ATTR_INDEX_ROOT: u32 = 0x90;
pub const ATTR_INDEX_ALLOCATION: u32 = 0xA0;
pub const ATTR_END: u32 = 0xFFFF_FFFF;

/// $FILE_NAME namespaces.
///
/// A single file commonly carries two $FILE_NAME attributes: the real long
/// name in the Win32 namespace and a generated 8.3 name in the DOS namespace.
/// Taking whichever comes first yields `PHOTO~1.JPG` instead of
/// `photograph of the pier.jpg` for an arbitrary subset of files, which
/// quietly destroys a name-accuracy metric.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Namespace {
    /// Case-sensitive, any character except NUL and `/`.
    Posix = 0,
    /// The normal long filename.
    Win32 = 1,
    /// A generated 8.3 short name.
    Dos = 2,
    /// The name is legal as both, so only one attribute exists.
    Win32AndDos = 3,
}

impl Namespace {
    pub fn from_u8(v: u8) -> Option<Namespace> {
        Some(match v {
            0 => Namespace::Posix,
            1 => Namespace::Win32,
            2 => Namespace::Dos,
            3 => Namespace::Win32AndDos,
            _ => return None,
        })
    }

    /// Preference order when a record carries several names. Higher wins.
    pub fn preference(self) -> u8 {
        match self {
            Namespace::Win32AndDos => 4,
            Namespace::Win32 => 3,
            Namespace::Posix => 2,
            // Last resort: better than no name at all, but it is the 8.3 form.
            Namespace::Dos => 1,
        }
    }
}

/// A parsed attribute header plus a slice of its content.
#[derive(Clone, Debug)]
pub struct Attribute<'a> {
    pub type_code: u32,
    pub name: Option<String>,
    pub non_resident: bool,
    pub flags: u16,
    pub attribute_id: u16,
    /// Resident value, or the raw runlist bytes when non-resident.
    pub body: &'a [u8],
    // Non-resident bookkeeping.
    pub start_vcn: u64,
    pub last_vcn: u64,
    pub allocated_size: u64,
    pub data_size: u64,
    pub initialized_size: u64,
}

impl Attribute<'_> {
    pub fn is_compressed(&self) -> bool {
        self.flags & 0x0001 != 0
    }
    pub fn is_encrypted(&self) -> bool {
        self.flags & 0x4000 != 0
    }
    pub fn is_sparse(&self) -> bool {
        self.flags & 0x8000 != 0
    }

    /// Decode the runlist of a non-resident attribute.
    pub fn extents(&self, max_clusters: u64) -> Result<Vec<Extent>> {
        if !self.non_resident {
            return Ok(Vec::new());
        }
        decode_runlist(self.body, max_clusters)
    }
}

/// Walk the attributes in a fixed-up MFT record.
///
/// `attrs_offset` comes from the record header at 0x14.
pub fn iter_attributes(record: &[u8], attrs_offset: usize) -> Result<Vec<Attribute<'_>>> {
    let mut out = Vec::new();
    let mut pos = attrs_offset;
    // A record cannot hold more attributes than this; the bound stops a
    // corrupt length field from spinning forever.
    const MAX_ATTRS: usize = 512;

    while out.len() < MAX_ATTRS {
        if pos + 4 > record.len() {
            break;
        }
        let type_code = u32::from_le_bytes(record[pos..pos + 4].try_into().unwrap());
        if type_code == ATTR_END {
            break;
        }
        if pos + 16 > record.len() {
            break;
        }
        let length = u32::from_le_bytes(record[pos + 4..pos + 8].try_into().unwrap()) as usize;
        // A zero or unaligned length would loop or desynchronise the walk.
        if length < 16 || length % 8 != 0 || pos + length > record.len() {
            return Err(FsError::Attribute {
                detail: format!(
                    "attribute at offset {pos} declares length {length}, which is invalid \
                     for a {}-byte record",
                    record.len()
                ),
            });
        }

        let non_resident = record[pos + 8] != 0;
        let name_length = record[pos + 9] as usize;
        let name_offset = u16::from_le_bytes([record[pos + 10], record[pos + 11]]) as usize;
        let flags = u16::from_le_bytes([record[pos + 12], record[pos + 13]]);
        let attribute_id = u16::from_le_bytes([record[pos + 14], record[pos + 15]]);

        let name = if name_length > 0 && pos + name_offset + name_length * 2 <= record.len() {
            Some(utf16le_to_string(
                &record[pos + name_offset..pos + name_offset + name_length * 2],
            ))
        } else {
            None
        };

        let mut attr = Attribute {
            type_code,
            name,
            non_resident,
            flags,
            attribute_id,
            body: &[],
            start_vcn: 0,
            last_vcn: 0,
            allocated_size: 0,
            data_size: 0,
            initialized_size: 0,
        };

        if non_resident {
            if pos + 0x40 > record.len() {
                break;
            }
            attr.start_vcn = u64::from_le_bytes(record[pos + 0x10..pos + 0x18].try_into().unwrap());
            attr.last_vcn = u64::from_le_bytes(record[pos + 0x18..pos + 0x20].try_into().unwrap());
            let run_offset = u16::from_le_bytes([record[pos + 0x20], record[pos + 0x21]]) as usize;
            attr.allocated_size =
                u64::from_le_bytes(record[pos + 0x28..pos + 0x30].try_into().unwrap());
            attr.data_size = u64::from_le_bytes(record[pos + 0x30..pos + 0x38].try_into().unwrap());
            attr.initialized_size =
                u64::from_le_bytes(record[pos + 0x38..pos + 0x40].try_into().unwrap());
            if run_offset < length && pos + run_offset <= record.len() {
                attr.body = &record[pos + run_offset..pos + length];
            }
        } else {
            if pos + 0x18 > record.len() {
                break;
            }
            let value_length =
                u32::from_le_bytes(record[pos + 0x10..pos + 0x14].try_into().unwrap()) as usize;
            let value_offset =
                u16::from_le_bytes([record[pos + 0x14], record[pos + 0x15]]) as usize;
            let start = pos + value_offset;
            let end = start.saturating_add(value_length);
            if start <= record.len() && end <= record.len() && end <= pos + length {
                attr.body = &record[start..end];
                attr.data_size = value_length as u64;
            }
        }

        out.push(attr);
        pos += length;
    }

    Ok(out)
}

/// $STANDARD_INFORMATION (0x10).
#[derive(Clone, Copy, Debug, Default)]
pub struct StandardInformation {
    pub timestamps: Timestamps,
    pub dos_flags: u32,
}

pub fn parse_standard_information(body: &[u8]) -> Option<StandardInformation> {
    if body.len() < 0x30 {
        return None;
    }
    Some(StandardInformation {
        timestamps: Timestamps {
            created: filetime_to_unix_nanos(read_u64(body, 0x00)),
            modified: filetime_to_unix_nanos(read_u64(body, 0x08)),
            mft_changed: filetime_to_unix_nanos(read_u64(body, 0x10)),
            accessed: filetime_to_unix_nanos(read_u64(body, 0x18)),
            utc_known: true,
        },
        dos_flags: u32::from_le_bytes(body[0x20..0x24].try_into().ok()?),
    })
}

/// $FILE_NAME (0x30).
#[derive(Clone, Debug)]
pub struct FileNameAttr {
    /// MFT record number of the parent directory (low 48 bits of the ref).
    pub parent_index: u64,
    /// Sequence number of the parent at the time this name was written.
    ///
    /// Validating this against the parent's current sequence is what
    /// distinguishes "this file lived in that directory" from "that MFT slot
    /// has since been reused by something unrelated".
    pub parent_sequence: u16,
    pub timestamps: Timestamps,
    pub allocated_size: u64,
    pub real_size: u64,
    pub flags: u32,
    pub namespace: Namespace,
    pub name: String,
}

pub fn parse_file_name(body: &[u8]) -> Option<FileNameAttr> {
    if body.len() < 0x42 {
        return None;
    }
    let parent_ref = read_u64(body, 0x00);
    let name_length = body[0x40] as usize;
    let namespace = Namespace::from_u8(body[0x41])?;
    let name_end = 0x42 + name_length * 2;
    if name_end > body.len() {
        return None;
    }
    Some(FileNameAttr {
        // A file reference is a 48-bit record index plus a 16-bit sequence.
        parent_index: parent_ref & 0x0000_FFFF_FFFF_FFFF,
        parent_sequence: (parent_ref >> 48) as u16,
        timestamps: Timestamps {
            created: filetime_to_unix_nanos(read_u64(body, 0x08)),
            modified: filetime_to_unix_nanos(read_u64(body, 0x10)),
            mft_changed: filetime_to_unix_nanos(read_u64(body, 0x18)),
            accessed: filetime_to_unix_nanos(read_u64(body, 0x20)),
            utc_known: true,
        },
        allocated_size: read_u64(body, 0x28),
        real_size: read_u64(body, 0x30),
        flags: u32::from_le_bytes(body[0x38..0x3C].try_into().ok()?),
        namespace,
        name: utf16le_to_string(&body[0x42..name_end]),
    })
}

/// One entry in an $ATTRIBUTE_LIST (0x20).
///
/// When a file is fragmented enough that its attributes no longer fit in one
/// MFT record, NTFS spills them into other records and leaves this list behind
/// as a directory of where they went. Ignoring it makes heavily fragmented
/// files - exactly the ones that matter for recovery - look zero-length.
#[derive(Clone, Debug)]
pub struct AttributeListEntry {
    pub type_code: u32,
    pub start_vcn: u64,
    /// MFT record holding the actual attribute.
    pub record_index: u64,
    pub record_sequence: u16,
    pub attribute_id: u16,
    pub name: Option<String>,
}

pub fn parse_attribute_list(body: &[u8]) -> Vec<AttributeListEntry> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    const MAX_ENTRIES: usize = 4096;

    while pos + 0x18 <= body.len() && out.len() < MAX_ENTRIES {
        let type_code = u32::from_le_bytes(body[pos..pos + 4].try_into().unwrap());
        if type_code == ATTR_END {
            break;
        }
        let length = u16::from_le_bytes([body[pos + 4], body[pos + 5]]) as usize;
        if length < 0x18 || pos + length > body.len() {
            break;
        }
        let name_length = body[pos + 6] as usize;
        let name_offset = body[pos + 7] as usize;
        let start_vcn = read_u64(body, pos + 0x08);
        let reference = read_u64(body, pos + 0x10);
        let attribute_id = u16::from_le_bytes([body[pos + 0x18 - 2], body[pos + 0x18 - 1]]);

        let name = if name_length > 0 && pos + name_offset + name_length * 2 <= body.len() {
            Some(utf16le_to_string(
                &body[pos + name_offset..pos + name_offset + name_length * 2],
            ))
        } else {
            None
        };

        out.push(AttributeListEntry {
            type_code,
            start_vcn,
            record_index: reference & 0x0000_FFFF_FFFF_FFFF,
            record_sequence: (reference >> 48) as u16,
            attribute_id,
            name,
        });
        pos += length;
    }
    out
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn read_u64(b: &[u8], off: usize) -> u64 {
    if off + 8 > b.len() {
        return 0;
    }
    u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
}

/// Decode UTF-16LE, handling surrogate pairs.
///
/// `char::decode_utf16` is what makes emoji come out right: a name containing
/// U+1F389 is two code units on disk and must become one `char`, not two
/// replacement characters.
pub fn utf16le_to_string(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    char::decode_utf16(units)
        .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

/// Windows FILETIME (100 ns since 1601-01-01 UTC) to Unix epoch nanoseconds.
pub fn filetime_to_unix_nanos(ft: u64) -> Option<i64> {
    if ft == 0 {
        return None;
    }
    // Seconds between 1601-01-01 and 1970-01-01.
    const EPOCH_DIFF_100NS: i64 = 116_444_736_000_000_000;
    let rel = ft as i64 - EPOCH_DIFF_100NS;
    rel.checked_mul(100)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespace_preference_favours_the_long_name() {
        assert!(Namespace::Win32.preference() > Namespace::Dos.preference());
        assert!(Namespace::Win32AndDos.preference() > Namespace::Win32.preference());
        assert!(Namespace::Posix.preference() > Namespace::Dos.preference());
    }

    #[test]
    fn decodes_utf16_including_surrogate_pairs() {
        // "emoji_" + U+1F389 (party popper) encoded as a surrogate pair.
        let mut b = Vec::new();
        for c in "emoji_".encode_utf16() {
            b.extend_from_slice(&c.to_le_bytes());
        }
        for c in '\u{1F389}'.encode_utf16(&mut [0u16; 2]).iter() {
            b.extend_from_slice(&c.to_le_bytes());
        }
        let s = utf16le_to_string(&b);
        assert_eq!(s, "emoji_\u{1F389}");
        assert_eq!(s.chars().count(), 7, "the pair must become one char");
    }

    #[test]
    fn decodes_non_ascii_scripts() {
        for original in ["日本語のファイル", "Кириллица", "café_niño", "ملف عربي"]
        {
            let mut b = Vec::new();
            for c in original.encode_utf16() {
                b.extend_from_slice(&c.to_le_bytes());
            }
            assert_eq!(utf16le_to_string(&b), original);
        }
    }

    #[test]
    fn filetime_conversion() {
        // 1970-01-01T00:00:00Z is exactly the epoch difference.
        assert_eq!(filetime_to_unix_nanos(116_444_736_000_000_000), Some(0));
        // One second later.
        assert_eq!(
            filetime_to_unix_nanos(116_444_736_000_000_000 + 10_000_000),
            Some(1_000_000_000)
        );
        assert_eq!(filetime_to_unix_nanos(0), None, "zero means 'not set'");
    }

    /// Build a resident attribute for testing the walker.
    fn resident_attr(type_code: u32, value: &[u8]) -> Vec<u8> {
        let header = 0x18usize;
        let total = (header + value.len()).next_multiple_of(8);
        let mut a = vec![0u8; total];
        a[0..4].copy_from_slice(&type_code.to_le_bytes());
        a[4..8].copy_from_slice(&(total as u32).to_le_bytes());
        a[8] = 0; // resident
        a[0x10..0x14].copy_from_slice(&(value.len() as u32).to_le_bytes());
        a[0x14..0x16].copy_from_slice(&(header as u16).to_le_bytes());
        a[header..header + value.len()].copy_from_slice(value);
        a
    }

    #[test]
    fn walks_resident_attributes_and_stops_at_the_terminator() {
        let mut rec = vec![0u8; 1024];
        let a1 = resident_attr(ATTR_STANDARD_INFORMATION, &[0xAA; 0x30]);
        let a2 = resident_attr(ATTR_DATA, b"hello");
        let mut pos = 0x38;
        rec[pos..pos + a1.len()].copy_from_slice(&a1);
        pos += a1.len();
        rec[pos..pos + a2.len()].copy_from_slice(&a2);
        pos += a2.len();
        rec[pos..pos + 4].copy_from_slice(&ATTR_END.to_le_bytes());

        let attrs = iter_attributes(&rec, 0x38).unwrap();
        assert_eq!(attrs.len(), 2);
        assert_eq!(attrs[0].type_code, ATTR_STANDARD_INFORMATION);
        assert_eq!(attrs[1].type_code, ATTR_DATA);
        assert_eq!(attrs[1].body, b"hello");
    }

    #[test]
    fn rejects_an_attribute_with_a_bogus_length() {
        let mut rec = vec![0u8; 1024];
        rec[0x38..0x3C].copy_from_slice(&ATTR_DATA.to_le_bytes());
        rec[0x3C..0x40].copy_from_slice(&0u32.to_le_bytes()); // length 0
        assert!(
            iter_attributes(&rec, 0x38).is_err(),
            "a zero length would loop forever"
        );
    }

    #[test]
    fn parses_a_file_name_attribute() {
        let name = "photo.jpg";
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut body = vec![0u8; 0x42 + units.len() * 2];
        // parent reference: index 5, sequence 3
        let parent_ref: u64 = 5 | (3u64 << 48);
        body[0x00..0x08].copy_from_slice(&parent_ref.to_le_bytes());
        body[0x30..0x38].copy_from_slice(&1234u64.to_le_bytes()); // real size
        body[0x40] = units.len() as u8;
        body[0x41] = Namespace::Win32 as u8;
        for (i, u) in units.iter().enumerate() {
            body[0x42 + i * 2..0x44 + i * 2].copy_from_slice(&u.to_le_bytes());
        }

        let fna = parse_file_name(&body).expect("parses");
        assert_eq!(fna.name, "photo.jpg");
        assert_eq!(fna.parent_index, 5);
        assert_eq!(
            fna.parent_sequence, 3,
            "sequence must be split from the index"
        );
        assert_eq!(fna.real_size, 1234);
        assert_eq!(fna.namespace, Namespace::Win32);
    }

    #[test]
    fn parses_an_attribute_list() {
        let mut body = vec![0u8; 0x20 * 2];
        for (i, (t, vcn, rec)) in [(ATTR_DATA, 0u64, 20u64), (ATTR_DATA, 100, 21)]
            .iter()
            .enumerate()
        {
            let o = i * 0x20;
            body[o..o + 4].copy_from_slice(&t.to_le_bytes());
            body[o + 4..o + 6].copy_from_slice(&0x20u16.to_le_bytes());
            body[o + 0x08..o + 0x10].copy_from_slice(&vcn.to_le_bytes());
            let reference: u64 = rec | (1u64 << 48);
            body[o + 0x10..o + 0x18].copy_from_slice(&reference.to_le_bytes());
        }
        let entries = parse_attribute_list(&body);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].record_index, 20);
        assert_eq!(entries[1].record_index, 21);
        assert_eq!(entries[1].start_vcn, 100);
        assert_eq!(entries[1].record_sequence, 1);
    }
}
