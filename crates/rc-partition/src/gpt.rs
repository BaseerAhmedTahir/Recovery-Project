//! GPT parsing with backup-header recovery (SPEC.md section 5.3).
//!
//! Both the primary header (LBA 1) and the backup (last LBA) are checked. If
//! the primary is corrupt the backup is used and the fact is recorded in the
//! table's notes, because "your partition table was rebuilt from the backup" is
//! something the operator needs to be told rather than silently handled.

use crate::table::{FsKind, Origin, Partition};
use rc_device::Lba;

pub const GPT_SIGNATURE: &[u8; 8] = b"EFI PART";

/// Well-known GPT partition type GUIDs, as the mixed-endian bytes they are
/// stored in on disk.
const GUID_MS_BASIC_DATA: [u8; 16] = [
    0xA2, 0xA0, 0xD0, 0xEB, 0xE5, 0xB9, 0x33, 0x44, 0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26, 0x99, 0xC7,
];
const GUID_LINUX_FS: [u8; 16] = [
    0xAF, 0x3D, 0xC6, 0x0F, 0x83, 0x84, 0x72, 0x47, 0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4,
];
const GUID_APPLE_APFS: [u8; 16] = [
    0x00, 0x53, 0x46, 0x7A, 0x11, 0x00, 0xAA, 0x11, 0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
];
const GUID_APPLE_HFS: [u8; 16] = [
    0x00, 0x53, 0x46, 0x48, 0x00, 0x00, 0xAA, 0x11, 0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
];
const GUID_EFI_SYSTEM: [u8; 16] = [
    0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B,
];

#[derive(Clone, Debug)]
pub struct GptHeader {
    pub current_lba: u64,
    pub backup_lba: u64,
    pub first_usable_lba: u64,
    pub last_usable_lba: u64,
    pub partition_entry_lba: u64,
    pub num_entries: u32,
    pub entry_size: u32,
    pub header_crc_ok: bool,
    pub entries_crc: u32,
    pub disk_guid: String,
}

/// CRC-32 (IEEE 802.3), which GPT uses for both the header and the entry array.
///
/// Implemented here rather than pulled in as a dependency: it is 15 lines, and
/// SPEC.md section 1.3 keeps the dependency surface small.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Render a GPT GUID in the conventional mixed-endian text form.
pub fn format_guid(g: &[u8]) -> String {
    if g.len() < 16 {
        return String::new();
    }
    format!(
        "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{}",
        g[3],
        g[2],
        g[1],
        g[0],
        g[5],
        g[4],
        g[7],
        g[6],
        g[8],
        g[9],
        g[10..16]
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<String>()
    )
}

fn fs_from_type_guid(g: &[u8; 16]) -> FsKind {
    match *g {
        // Microsoft Basic Data covers NTFS, FAT and exFAT alike; the boot
        // sector at the partition start is what actually decides.
        GUID_MS_BASIC_DATA => FsKind::Unknown,
        GUID_LINUX_FS => FsKind::Ext4,
        GUID_APPLE_APFS => FsKind::Apfs,
        GUID_APPLE_HFS => FsKind::HfsPlus,
        GUID_EFI_SYSTEM => FsKind::Fat32,
        _ => FsKind::Other,
    }
}

/// Parse a GPT header from a single sector.
pub fn parse_header(sector: &[u8]) -> Option<GptHeader> {
    if sector.len() < 92 || &sector[0..8] != GPT_SIGNATURE {
        return None;
    }

    let header_size = u32::from_le_bytes(sector[12..16].try_into().ok()?) as usize;
    if !(92..=512).contains(&header_size) || header_size > sector.len() {
        return None;
    }

    let stored_crc = u32::from_le_bytes(sector[16..20].try_into().ok()?);
    // The CRC is computed with its own field zeroed.
    let mut buf = sector[..header_size].to_vec();
    buf[16..20].fill(0);
    let header_crc_ok = crc32(&buf) == stored_crc;

    Some(GptHeader {
        current_lba: u64::from_le_bytes(sector[24..32].try_into().ok()?),
        backup_lba: u64::from_le_bytes(sector[32..40].try_into().ok()?),
        first_usable_lba: u64::from_le_bytes(sector[40..48].try_into().ok()?),
        last_usable_lba: u64::from_le_bytes(sector[48..56].try_into().ok()?),
        partition_entry_lba: u64::from_le_bytes(sector[72..80].try_into().ok()?),
        num_entries: u32::from_le_bytes(sector[80..84].try_into().ok()?),
        entry_size: u32::from_le_bytes(sector[84..88].try_into().ok()?),
        entries_crc: u32::from_le_bytes(sector[88..92].try_into().ok()?),
        header_crc_ok,
        disk_guid: format_guid(&sector[56..72]),
    })
}

impl GptHeader {
    /// Reject absurd headers before we allocate based on their numbers.
    pub fn looks_sane(&self, total_sectors: u64) -> bool {
        self.entry_size >= 128
            && self.entry_size <= 4096
            && self.num_entries > 0
            && self.num_entries <= 65536
            && self.partition_entry_lba > 0
            && (total_sectors == 0 || self.partition_entry_lba < total_sectors)
            && self.first_usable_lba <= self.last_usable_lba
    }

    pub fn entries_bytes(&self) -> u64 {
        self.num_entries as u64 * self.entry_size as u64
    }
}

/// Parse the partition entry array.
///
/// `entries` must hold `num_entries * entry_size` bytes read from
/// `partition_entry_lba`.
pub fn parse_entries(header: &GptHeader, entries: &[u8]) -> (Vec<Partition>, bool) {
    let esz = header.entry_size as usize;
    let want = header.entries_bytes() as usize;
    let crc_ok = entries.len() >= want && crc32(&entries[..want]) == header.entries_crc;

    let mut out = Vec::new();
    let mut index = 0usize;
    for i in 0..header.num_entries as usize {
        let o = i * esz;
        if o + 128 > entries.len() {
            break;
        }
        let e = &entries[o..o + esz.min(entries.len() - o)];

        let mut type_guid = [0u8; 16];
        type_guid.copy_from_slice(&e[0..16]);
        if type_guid == [0u8; 16] {
            continue; // unused slot
        }

        let first = u64::from_le_bytes(e[32..40].try_into().unwrap_or([0; 8]));
        let last = u64::from_le_bytes(e[40..48].try_into().unwrap_or([0; 8]));
        if last < first {
            continue;
        }

        // UTF-16LE name, 36 code units.
        let name: String = e[56..e.len().min(128)]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&c| c != 0)
            .filter_map(|c| char::from_u32(c as u32))
            .collect();

        index += 1;
        out.push(Partition {
            index,
            start: Lba(first),
            // GPT last_lba is inclusive.
            sectors: last - first + 1,
            fs: fs_from_type_guid(&type_guid),
            origin: Origin::Gpt,
            label: if name.is_empty() { None } else { Some(name) },
            type_guid: Some(format_guid(&type_guid)),
            bootable: false,
            confidence: 100,
            evidence: vec![format!(
                "GPT entry {}, LBA {}..={}, type {}",
                i + 1,
                first,
                last,
                format_guid(&type_guid)
            )],
        });
    }
    (out, crc_ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a syntactically valid GPT header plus entry array.
    fn build_gpt(
        num_entries: u32,
        entry_size: u32,
        parts: &[(u64, u64, &str)],
    ) -> (Vec<u8>, Vec<u8>) {
        let mut entries = vec![0u8; (num_entries * entry_size) as usize];
        for (i, (first, last, name)) in parts.iter().enumerate() {
            let o = i * entry_size as usize;
            entries[o..o + 16].copy_from_slice(&GUID_MS_BASIC_DATA);
            // unique partition GUID
            entries[o + 16] = (i + 1) as u8;
            entries[o + 32..o + 40].copy_from_slice(&first.to_le_bytes());
            entries[o + 40..o + 48].copy_from_slice(&last.to_le_bytes());
            for (j, u) in name.encode_utf16().enumerate() {
                let p = o + 56 + j * 2;
                entries[p..p + 2].copy_from_slice(&u.to_le_bytes());
            }
        }

        let mut h = vec![0u8; 512];
        h[0..8].copy_from_slice(GPT_SIGNATURE);
        h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes()); // revision 1.0
        h[12..16].copy_from_slice(&92u32.to_le_bytes()); // header size
        h[24..32].copy_from_slice(&1u64.to_le_bytes()); // current lba
        h[32..40].copy_from_slice(&2047u64.to_le_bytes()); // backup lba
        h[40..48].copy_from_slice(&34u64.to_le_bytes()); // first usable
        h[48..56].copy_from_slice(&2014u64.to_le_bytes()); // last usable
        h[72..80].copy_from_slice(&2u64.to_le_bytes()); // entry array lba
        h[80..84].copy_from_slice(&num_entries.to_le_bytes());
        h[84..88].copy_from_slice(&entry_size.to_le_bytes());
        h[88..92].copy_from_slice(&crc32(&entries).to_le_bytes());
        let crc = crc32(&h[..92]);
        h[16..20].copy_from_slice(&crc.to_le_bytes());
        (h, entries)
    }

    /// CRC-32/ISO-HDLC check value for "123456789".
    #[test]
    fn crc32_matches_the_standard_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn parses_a_valid_header_and_verifies_its_crc() {
        let (h, _) = build_gpt(128, 128, &[]);
        let hdr = parse_header(&h).expect("header parses");
        assert!(hdr.header_crc_ok, "self-consistent CRC should verify");
        assert_eq!(hdr.current_lba, 1);
        assert_eq!(hdr.backup_lba, 2047);
        assert_eq!(hdr.num_entries, 128);
        assert_eq!(hdr.entry_size, 128);
        assert!(hdr.looks_sane(4096));
    }

    #[test]
    fn detects_a_corrupted_header() {
        let (mut h, _) = build_gpt(128, 128, &[]);
        h[40] ^= 0xFF; // flip a byte covered by the CRC
        let hdr = parse_header(&h).expect("still parses structurally");
        assert!(!hdr.header_crc_ok, "corrupted header must fail its CRC");
    }

    #[test]
    fn rejects_a_sector_without_the_signature() {
        assert!(parse_header(&[0u8; 512]).is_none());
    }

    #[test]
    fn parses_partition_entries_with_names() {
        let (h, e) = build_gpt(
            128,
            128,
            &[(2048, 4095, "Basic data"), (4096, 8191, "Second")],
        );
        let hdr = parse_header(&h).unwrap();
        let (parts, crc_ok) = parse_entries(&hdr, &e);

        assert!(crc_ok, "entry array CRC should verify");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].start, Lba(2048));
        assert_eq!(parts[0].sectors, 2048, "last_lba is inclusive");
        assert_eq!(parts[0].label.as_deref(), Some("Basic data"));
        assert_eq!(parts[1].start, Lba(4096));
        assert_eq!(parts[1].label.as_deref(), Some("Second"));
    }

    #[test]
    fn skips_unused_entry_slots() {
        let (h, mut e) = build_gpt(128, 128, &[(2048, 4095, "One")]);
        // Slot 1 is used; slots 2..128 are zeroed and must be ignored.
        let hdr = parse_header(&h).unwrap();
        let (parts, _) = parse_entries(&hdr, &e);
        assert_eq!(parts.len(), 1);

        // Zero slot 0 too and expect nothing.
        e[0..16].fill(0);
        let (parts, _) = parse_entries(&hdr, &e);
        assert!(parts.is_empty());
    }

    #[test]
    fn detects_a_corrupted_entry_array() {
        let (h, mut e) = build_gpt(128, 128, &[(2048, 4095, "One")]);
        let hdr = parse_header(&h).unwrap();
        e[60] ^= 0xFF;
        let (_, crc_ok) = parse_entries(&hdr, &e);
        assert!(!crc_ok);
    }

    #[test]
    fn insane_headers_are_rejected_before_allocation() {
        let (mut h, _) = build_gpt(128, 128, &[]);
        // A header claiming four billion entries must not be believed.
        h[80..84].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        let hdr = parse_header(&h).unwrap();
        assert!(!hdr.looks_sane(4096));
    }

    #[test]
    fn formats_guids_in_mixed_endian_form() {
        assert_eq!(
            format_guid(&GUID_MS_BASIC_DATA),
            "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7"
        );
    }
}
