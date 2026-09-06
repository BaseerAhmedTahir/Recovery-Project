//! MFT record parsing and volume-level scanning.

use crate::entry::{
    DataLocation, Entry, EntryKind, EntryState, Extent, PathConfidence, Timestamps,
};
use crate::error::{FsError, Result};
use crate::ntfs::attr::{
    self, Attribute, FileNameAttr, Namespace, ATTR_ATTRIBUTE_LIST, ATTR_DATA, ATTR_FILE_NAME,
    ATTR_INDEX_ROOT, ATTR_STANDARD_INFORMATION,
};
use crate::ntfs::boot::NtfsBoot;
use crate::ntfs::fixup::apply_fixups;

pub const MFT_SIGNATURE: &[u8; 4] = b"FILE";
/// MFT record numbers below this are NTFS's own metadata files.
pub const FIRST_USER_RECORD: u64 = 16;
/// Record 5 is the volume root directory.
pub const ROOT_RECORD: u64 = 5;

/// Record header flags at offset 0x16.
///
/// Per SPEC.md section 5.4: 0x00 deleted file, 0x01 allocated file,
/// 0x02 deleted directory, 0x03 allocated directory.
const FLAG_IN_USE: u16 = 0x0001;
const FLAG_IS_DIRECTORY: u16 = 0x0002;

/// A parsed MFT record.
#[derive(Clone, Debug)]
pub struct MftRecord {
    pub index: u64,
    pub sequence: u16,
    pub in_use: bool,
    pub is_directory: bool,
    /// Zero for a base record; otherwise the record this one extends.
    pub base_record: u64,
    pub names: Vec<FileNameAttr>,
    pub standard_info: Option<attr::StandardInformation>,
    pub data_size: u64,
    pub allocated_size: u64,
    pub resident_data: Option<Vec<u8>>,
    pub extents: Vec<Extent>,
    /// $ATTRIBUTE_LIST entries pointing at other records holding this file's
    /// $DATA. Empty for the common case.
    pub attribute_list: Vec<attr::AttributeListEntry>,
    pub notes: Vec<String>,
}

impl MftRecord {
    /// The name to report, preferring the Win32 long name over the 8.3 form.
    pub fn best_name(&self) -> Option<&FileNameAttr> {
        self.names.iter().max_by_key(|n| n.namespace.preference())
    }
}

/// Parse one MFT record. `buf` is modified in place by the fixup pass.
pub fn parse_record(buf: &mut [u8], index: u64, boot: &NtfsBoot) -> Result<Option<MftRecord>> {
    if buf.len() < 48 {
        return Ok(None);
    }
    if &buf[0..4] != MFT_SIGNATURE {
        // Unused records are zeroed or hold "BAAD"; neither is an error.
        return Ok(None);
    }

    let usa_offset = u16::from_le_bytes([buf[4], buf[5]]);
    let usa_count = u16::from_le_bytes([buf[6], buf[7]]);

    // Fixups FIRST. Nothing below this line may read buf before it runs, or it
    // will read the update sequence number instead of two real bytes out of
    // every sector. See fixup.rs.
    apply_fixups(buf, usa_offset, usa_count, boot.bytes_per_sector as usize)?;

    let sequence = u16::from_le_bytes([buf[0x10], buf[0x11]]);
    let attrs_offset = u16::from_le_bytes([buf[0x14], buf[0x15]]) as usize;
    let flags = u16::from_le_bytes([buf[0x16], buf[0x17]]);
    let bytes_in_use = u32::from_le_bytes(buf[0x18..0x1C].try_into().unwrap()) as usize;
    let base_ref = u64::from_le_bytes(buf[0x20..0x28].try_into().unwrap());

    if attrs_offset < 0x2A || attrs_offset >= buf.len() {
        return Err(FsError::Attribute {
            detail: format!("record {index} has an attribute offset of {attrs_offset}"),
        });
    }

    // Trust bytes_in_use when it is sane; it bounds the attribute walk.
    let usable = if bytes_in_use >= attrs_offset && bytes_in_use <= buf.len() {
        bytes_in_use
    } else {
        buf.len()
    };

    let mut rec = MftRecord {
        index,
        sequence,
        in_use: flags & FLAG_IN_USE != 0,
        is_directory: flags & FLAG_IS_DIRECTORY != 0,
        base_record: base_ref & 0x0000_FFFF_FFFF_FFFF,
        names: Vec::new(),
        standard_info: None,
        data_size: 0,
        allocated_size: 0,
        resident_data: None,
        extents: Vec::new(),
        attribute_list: Vec::new(),
        notes: Vec::new(),
    };

    let attrs = attr::iter_attributes(&buf[..usable], attrs_offset)?;
    let total_clusters = boot.total_clusters();

    for a in &attrs {
        match a.type_code {
            ATTR_STANDARD_INFORMATION => {
                if rec.standard_info.is_none() {
                    rec.standard_info = attr::parse_standard_information(a.body);
                }
            }
            ATTR_FILE_NAME => {
                if let Some(f) = attr::parse_file_name(a.body) {
                    rec.names.push(f);
                }
            }
            ATTR_ATTRIBUTE_LIST => {
                // Only the unnamed list matters, and only when resident here.
                // A non-resident $ATTRIBUTE_LIST needs its own read, which the
                // volume scanner handles.
                if !a.non_resident {
                    rec.attribute_list = attr::parse_attribute_list(a.body);
                } else {
                    rec.notes.push(
                        "$ATTRIBUTE_LIST is non-resident; extra extents were not followed"
                            .to_string(),
                    );
                }
            }
            ATTR_DATA => {
                // The unnamed $DATA stream is the file's content. Named
                // streams are alternate data streams and are not the file.
                if a.name.is_some() {
                    continue;
                }
                collect_data(a, &mut rec, total_clusters);
            }
            // An $I30 index root is the reliable directory marker when the
            // record header flags are ambiguous.
            ATTR_INDEX_ROOT if a.name.as_deref() == Some("$I30") => {
                rec.is_directory = true;
            }
            _ => {}
        }
    }

    Ok(Some(rec))
}

fn collect_data(a: &Attribute<'_>, rec: &mut MftRecord, total_clusters: u64) {
    if a.non_resident {
        // Only the first fragment of a multi-record attribute carries the
        // sizes; later fragments have start_vcn > 0.
        if a.start_vcn == 0 {
            rec.data_size = a.data_size;
            rec.allocated_size = a.allocated_size;
        }
        if a.is_compressed() {
            rec.notes
                .push("$DATA is compressed; extents are compression units".to_string());
        }
        if a.is_encrypted() {
            rec.notes.push(
                "$DATA is EFS-encrypted; content is not recoverable without the key".to_string(),
            );
        }
        match a.extents(total_clusters) {
            Ok(mut e) => rec.extents.append(&mut e),
            Err(err) => rec.notes.push(format!("runlist undecodable: {err}")),
        }
    } else {
        rec.data_size = a.body.len() as u64;
        rec.allocated_size = a.body.len() as u64;
        rec.resident_data = Some(a.body.to_vec());
    }
}

/// Convert a record into an `Entry`, resolving its path against an index of
/// every record's sequence number and name.
///
/// `resolve` maps an MFT index to `(sequence, name, parent_index,
/// parent_sequence)` for directories, and is how the path is walked upwards.
pub fn record_to_entry(
    rec: &MftRecord,
    boot: &NtfsBoot,
    resolve: &dyn Fn(u64) -> Option<(u16, String, u64, u16)>,
) -> Entry {
    let name_attr = rec.best_name();
    let name = name_attr.map(|n| n.name.clone()).unwrap_or_default();

    let mut notes = rec.notes.clone();
    if let Some(n) = name_attr {
        if n.namespace == Namespace::Dos && rec.names.len() == 1 {
            notes.push(
                "only an 8.3 DOS name survives for this record; the long name is lost".to_string(),
            );
        }
    }

    let (path, confidence) = match name_attr {
        None => (None, PathConfidence::ParentUnknown),
        Some(n) => build_path(n, &name, resolve),
    };

    let timestamps = rec
        .standard_info
        .map(|si| si.timestamps)
        .or_else(|| name_attr.map(|n| n.timestamps))
        .unwrap_or(Timestamps {
            utc_known: true,
            ..Default::default()
        });

    let size = if rec.data_size > 0 {
        rec.data_size
    } else {
        name_attr.map(|n| n.real_size).unwrap_or(0)
    };

    let location = if let Some(d) = &rec.resident_data {
        DataLocation::Resident(d.clone())
    } else if !rec.extents.is_empty() {
        DataLocation::Runs(rec.extents.clone())
    } else {
        DataLocation::Unknown
    };

    let _ = boot;
    Entry {
        id: rec.index,
        name,
        path,
        path_confidence: confidence,
        kind: if rec.is_directory {
            EntryKind::Directory
        } else {
            EntryKind::File
        },
        state: if rec.in_use {
            EntryState::Allocated
        } else {
            EntryState::Deleted
        },
        size,
        allocated_size: rec.allocated_size,
        timestamps,
        location,
        parent_id: name_attr.map(|n| n.parent_index),
        notes,
    }
}

/// Walk parent references up to the root, validating sequence numbers.
///
/// An MFT record number alone is not a stable identity: when a file is deleted
/// its record is eventually reused, and the sequence number is incremented each
/// time. A deleted file's `$FILE_NAME` records the parent's sequence *as it was*,
/// so comparing that against the parent's current sequence distinguishes
///
///   * "this file really did live in that directory" from
///   * "that MFT slot now holds something else entirely".
///
/// Getting this wrong attaches recovered files to plausible but wrong
/// directories, which is worse than reporting an unknown path: it produces
/// confident output that cannot be trusted.
fn build_path(
    name_attr: &FileNameAttr,
    name: &str,
    resolve: &dyn Fn(u64) -> Option<(u16, String, u64, u16)>,
) -> (Option<String>, PathConfidence) {
    // The root directory is its own parent.
    if name_attr.parent_index == ROOT_RECORD {
        return (Some(name.to_string()), PathConfidence::Exact);
    }

    let mut components = vec![name.to_string()];
    let mut current_index = name_attr.parent_index;
    let mut expected_sequence = name_attr.parent_sequence;

    // Bounded: a corrupt volume can contain a parent cycle.
    for _ in 0..256 {
        if current_index == ROOT_RECORD {
            components.reverse();
            return (Some(components.join("/")), PathConfidence::Exact);
        }

        let Some((actual_sequence, parent_name, grandparent, grandparent_seq)) =
            resolve(current_index)
        else {
            return (None, PathConfidence::ParentUnknown);
        };

        if actual_sequence != expected_sequence {
            return (
                None,
                PathConfidence::ParentReused {
                    detail: format!(
                        "MFT record {current_index} now has sequence {actual_sequence} but this \
                         file recorded parent sequence {expected_sequence}; that directory has \
                         been reused and the original path is not recoverable"
                    ),
                },
            );
        }

        components.push(parent_name);
        current_index = grandparent;
        expected_sequence = grandparent_seq;
    }

    (None, PathConfidence::ParentUnknown)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ntfs::boot::parse_boot;

    fn test_boot() -> NtfsBoot {
        let mut b = vec![0u8; 512];
        b[3..11].copy_from_slice(b"NTFS    ");
        b[11..13].copy_from_slice(&512u16.to_le_bytes());
        b[13] = 8;
        b[0x28..0x30].copy_from_slice(&1_048_575u64.to_le_bytes());
        b[0x30..0x38].copy_from_slice(&4u64.to_le_bytes());
        b[0x40] = 0xF6;
        parse_boot(&b).unwrap()
    }

    /// Assemble a 1024-byte MFT record with a correct fixup array.
    fn build_record(index: u64, sequence: u16, flags: u16, attrs: &[Vec<u8>]) -> Vec<u8> {
        let size = 1024usize;
        let sector = 512usize;
        let sectors = size / sector;
        let usa_offset = 0x30usize;
        let attrs_offset = 0x38usize;

        let mut buf = vec![0u8; size];
        buf[0..4].copy_from_slice(MFT_SIGNATURE);
        buf[4..6].copy_from_slice(&(usa_offset as u16).to_le_bytes());
        buf[6..8].copy_from_slice(&((sectors + 1) as u16).to_le_bytes());
        buf[0x10..0x12].copy_from_slice(&sequence.to_le_bytes());
        buf[0x14..0x16].copy_from_slice(&(attrs_offset as u16).to_le_bytes());
        buf[0x16..0x18].copy_from_slice(&flags.to_le_bytes());

        let mut pos = attrs_offset;
        for a in attrs {
            buf[pos..pos + a.len()].copy_from_slice(a);
            pos += a.len();
        }
        buf[pos..pos + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        pos += 4;
        buf[0x18..0x1C].copy_from_slice(&(pos as u32).to_le_bytes());

        // Apply fixups the way NTFS would when writing.
        let usn: u16 = 0x0001;
        buf[usa_offset..usa_offset + 2].copy_from_slice(&usn.to_le_bytes());
        for i in 0..sectors {
            let tail = (i + 1) * sector - 2;
            let orig = [buf[tail], buf[tail + 1]];
            let entry = usa_offset + 2 + i * 2;
            buf[entry] = orig[0];
            buf[entry + 1] = orig[1];
            buf[tail..tail + 2].copy_from_slice(&usn.to_le_bytes());
        }
        let _ = index;
        buf
    }

    fn resident(type_code: u32, value: &[u8]) -> Vec<u8> {
        let header = 0x18usize;
        let total = (header + value.len()).next_multiple_of(8);
        let mut a = vec![0u8; total];
        a[0..4].copy_from_slice(&type_code.to_le_bytes());
        a[4..8].copy_from_slice(&(total as u32).to_le_bytes());
        a[0x10..0x14].copy_from_slice(&(value.len() as u32).to_le_bytes());
        a[0x14..0x16].copy_from_slice(&(header as u16).to_le_bytes());
        a[header..header + value.len()].copy_from_slice(value);
        a
    }

    fn file_name_value(parent: u64, parent_seq: u16, ns: Namespace, name: &str) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut v = vec![0u8; 0x42 + units.len() * 2];
        let pref: u64 = parent | ((parent_seq as u64) << 48);
        v[0x00..0x08].copy_from_slice(&pref.to_le_bytes());
        v[0x30..0x38].copy_from_slice(&4321u64.to_le_bytes());
        v[0x40] = units.len() as u8;
        v[0x41] = ns as u8;
        for (i, u) in units.iter().enumerate() {
            v[0x42 + i * 2..0x44 + i * 2].copy_from_slice(&u.to_le_bytes());
        }
        v
    }

    #[test]
    fn parses_a_deleted_file_record() {
        let boot = test_boot();
        let attrs = vec![
            resident(ATTR_STANDARD_INFORMATION, &[0u8; 0x30]),
            resident(
                ATTR_FILE_NAME,
                &file_name_value(5, 1, Namespace::Win32, "deleted.jpg"),
            ),
            resident(ATTR_DATA, b"content"),
        ];
        // flags 0x00 => deleted file
        let mut buf = build_record(30, 2, 0x0000, &attrs);
        let rec = parse_record(&mut buf, 30, &boot).unwrap().expect("record");

        assert!(!rec.in_use, "flags 0x00 means deleted");
        assert!(!rec.is_directory);
        assert_eq!(rec.best_name().unwrap().name, "deleted.jpg");
        assert_eq!(rec.resident_data.as_deref(), Some(&b"content"[..]));
    }

    #[test]
    fn record_flags_map_to_state_and_kind() {
        let boot = test_boot();
        let attrs = vec![resident(
            ATTR_FILE_NAME,
            &file_name_value(5, 1, Namespace::Win32, "x"),
        )];
        for (flags, in_use, is_dir) in [
            (0x0000u16, false, false), // deleted file
            (0x0001, true, false),     // allocated file
            (0x0002, false, true),     // deleted directory
            (0x0003, true, true),      // allocated directory
        ] {
            let mut buf = build_record(40, 1, flags, &attrs);
            let rec = parse_record(&mut buf, 40, &boot).unwrap().unwrap();
            assert_eq!(rec.in_use, in_use, "flags {flags:#06x}");
            assert_eq!(rec.is_directory, is_dir, "flags {flags:#06x}");
        }
    }

    /// The 8.3 name must never win when a long name is present.
    #[test]
    fn prefers_the_win32_name_over_the_dos_name() {
        let boot = test_boot();
        let attrs = vec![
            // DOS name deliberately first, which is how it often appears.
            resident(
                ATTR_FILE_NAME,
                &file_name_value(5, 1, Namespace::Dos, "PHOTOG~1.JPG"),
            ),
            resident(
                ATTR_FILE_NAME,
                &file_name_value(5, 1, Namespace::Win32, "photograph of the pier.jpg"),
            ),
        ];
        let mut buf = build_record(50, 1, 0x0000, &attrs);
        let rec = parse_record(&mut buf, 50, &boot).unwrap().unwrap();
        assert_eq!(
            rec.best_name().unwrap().name,
            "photograph of the pier.jpg",
            "taking the first $FILE_NAME would have returned the 8.3 name"
        );
    }

    #[test]
    fn falls_back_to_the_dos_name_when_it_is_all_there_is() {
        let boot = test_boot();
        let attrs = vec![resident(
            ATTR_FILE_NAME,
            &file_name_value(5, 1, Namespace::Dos, "OLDFIL~1.TXT"),
        )];
        let mut buf = build_record(51, 1, 0x0000, &attrs);
        let rec = parse_record(&mut buf, 51, &boot).unwrap().unwrap();
        assert_eq!(rec.best_name().unwrap().name, "OLDFIL~1.TXT");

        let e = record_to_entry(&rec, &boot, &|_| None);
        assert!(
            e.notes.iter().any(|n| n.contains("8.3")),
            "the CLI should be told the long name is gone"
        );
    }

    #[test]
    fn a_torn_record_is_rejected_rather_than_parsed() {
        let boot = test_boot();
        let attrs = vec![resident(
            ATTR_FILE_NAME,
            &file_name_value(5, 1, Namespace::Win32, "x.txt"),
        )];
        let mut buf = build_record(60, 1, 0x0000, &attrs);
        // Corrupt the second sector's USN.
        buf[1022] = 0xFF;
        buf[1023] = 0xFF;
        let err = parse_record(&mut buf, 60, &boot).unwrap_err();
        assert!(matches!(err, FsError::Fixup { .. }));
    }

    // --- path reconstruction -------------------------------------------

    #[test]
    fn builds_a_path_through_matching_parent_sequences() {
        // root(5) <- docs(20, seq 1) <- file(parent 20, seq 1)
        let fna = FileNameAttr {
            parent_index: 20,
            parent_sequence: 1,
            timestamps: Timestamps::default(),
            allocated_size: 0,
            real_size: 0,
            flags: 0,
            namespace: Namespace::Win32,
            name: "report.pdf".into(),
        };
        let resolve = |idx: u64| -> Option<(u16, String, u64, u16)> {
            match idx {
                20 => Some((1, "docs".into(), ROOT_RECORD, 5)),
                _ => None,
            }
        };
        let (path, conf) = build_path(&fna, "report.pdf", &resolve);
        assert_eq!(path.as_deref(), Some("docs/report.pdf"));
        assert_eq!(conf, PathConfidence::Exact);
    }

    #[test]
    fn builds_a_deeply_nested_path() {
        // Nine levels, matching the deep fixture entry.
        let depth = 9u64;
        let resolve = move |idx: u64| -> Option<(u16, String, u64, u16)> {
            if (100..100 + depth).contains(&idx) {
                let level = idx - 100;
                let parent = if level == 0 { ROOT_RECORD } else { idx - 1 };
                Some((1, format!("lvl{level}"), parent, 1))
            } else {
                None
            }
        };
        let fna = FileNameAttr {
            parent_index: 100 + depth - 1,
            parent_sequence: 1,
            timestamps: Timestamps::default(),
            allocated_size: 0,
            real_size: 0,
            flags: 0,
            namespace: Namespace::Win32,
            name: "buried.jpg".into(),
        };
        let (path, conf) = build_path(&fna, "buried.jpg", &resolve);
        assert_eq!(conf, PathConfidence::Exact);
        assert_eq!(
            path.as_deref(),
            Some("lvl0/lvl1/lvl2/lvl3/lvl4/lvl5/lvl6/lvl7/lvl8/buried.jpg")
        );
    }

    /// The failure mode that makes recovery output untrustworthy.
    #[test]
    fn a_reused_parent_yields_unknown_path_not_a_wrong_one() {
        let fna = FileNameAttr {
            parent_index: 20,
            parent_sequence: 3,
            timestamps: Timestamps::default(),
            allocated_size: 0,
            real_size: 0,
            flags: 0,
            namespace: Namespace::Win32,
            name: "secret.pdf".into(),
        };
        // Record 20 has since been reused: its sequence is now 7, not 3.
        let resolve = |idx: u64| -> Option<(u16, String, u64, u16)> {
            match idx {
                20 => Some((7, "SomeOtherFolder".into(), ROOT_RECORD, 5)),
                _ => None,
            }
        };
        let (path, conf) = build_path(&fna, "secret.pdf", &resolve);
        assert_eq!(path, None, "must not claim a path");
        match conf {
            PathConfidence::ParentReused { detail } => {
                assert!(detail.contains("reused"), "explains itself: {detail}");
            }
            other => panic!("expected ParentReused, got {other:?}"),
        }
    }

    #[test]
    fn an_unresolvable_parent_yields_unknown_path() {
        let fna = FileNameAttr {
            parent_index: 999,
            parent_sequence: 1,
            timestamps: Timestamps::default(),
            allocated_size: 0,
            real_size: 0,
            flags: 0,
            namespace: Namespace::Win32,
            name: "orphan.txt".into(),
        };
        let (path, conf) = build_path(&fna, "orphan.txt", &|_| None);
        assert_eq!(path, None);
        assert_eq!(conf, PathConfidence::ParentUnknown);
    }

    #[test]
    fn a_parent_cycle_terminates() {
        // Two directories claiming each other as parent.
        let resolve = |idx: u64| -> Option<(u16, String, u64, u16)> {
            match idx {
                30 => Some((1, "a".into(), 31, 1)),
                31 => Some((1, "b".into(), 30, 1)),
                _ => None,
            }
        };
        let fna = FileNameAttr {
            parent_index: 30,
            parent_sequence: 1,
            timestamps: Timestamps::default(),
            allocated_size: 0,
            real_size: 0,
            flags: 0,
            namespace: Namespace::Win32,
            name: "loop.txt".into(),
        };
        let (path, conf) = build_path(&fna, "loop.txt", &resolve);
        assert_eq!(path, None, "a cycle must not hang or produce a huge path");
        assert_eq!(conf, PathConfidence::ParentUnknown);
    }

    #[test]
    fn a_file_directly_in_the_root_has_a_bare_path() {
        let fna = FileNameAttr {
            parent_index: ROOT_RECORD,
            parent_sequence: 5,
            timestamps: Timestamps::default(),
            allocated_size: 0,
            real_size: 0,
            flags: 0,
            namespace: Namespace::Win32,
            name: "top.txt".into(),
        };
        let (path, conf) = build_path(&fna, "top.txt", &|_| None);
        assert_eq!(path.as_deref(), Some("top.txt"));
        assert_eq!(conf, PathConfidence::Exact);
    }
}
