//! `rc-partition` - partition discovery for RECOVERY-CORE.
//!
//! Three ways a partition becomes known, tried in order (SPEC.md section 5.3):
//!
//! 1. **GPT**, primary header first, falling back to the backup if the primary
//!    fails its CRC. Recovering from the backup is recorded in the notes.
//! 2. **MBR**, including recognising a protective MBR as "this is really GPT".
//! 3. **Signature scan**, when neither table is usable: look for filesystem
//!    boot sectors and superblocks and rebuild a plausible table *in memory*.
//!
//! Nothing here writes to the device. A reconstructed table is a hypothesis,
//! carries a confidence score and its supporting evidence, and is labelled as
//! inferred so the CLI never presents a guess as a fact.

pub mod error;
pub mod gpt;
pub mod mbr;
pub mod scan;
pub mod table;

pub use error::{PartitionError, Result};
pub use scan::{scan_for_filesystems, Candidate, ScanOptions};
pub use table::{FsKind, Origin, Partition, PartitionTable};

use rc_device::{Lba, ReadOnlyDevice};

/// How hard to look for partitions.
#[derive(Clone, Debug, Default)]
pub struct DiscoverOptions {
    /// Run the signature scan even when a partition table was found. Useful
    /// when a table exists but is known to be stale.
    pub always_scan: bool,
    pub scan: ScanOptions,
}

/// Discover a device's partitions.
pub fn discover(device: &dyn ReadOnlyDevice, opts: &DiscoverOptions) -> Result<PartitionTable> {
    let ss = device.sector_size();
    let total = device.total_sectors();
    let mut out = PartitionTable::default();

    // --- sector 0: MBR or protective MBR ---------------------------------
    let mut sector0 = vec![0u8; ss.as_usize()];
    let have_sector0 = device.read_at(Lba(0), &mut sector0).is_ok();
    let mbr_entries = if have_sector0 {
        mbr::parse_mbr(&sector0)
    } else {
        out.notes
            .push("sector 0 is unreadable; relying on the backup GPT and a scan".to_string());
        None
    };

    if let Some(e) = &mbr_entries {
        out.protective_mbr = mbr::is_protective(e);
    }

    // --- GPT --------------------------------------------------------------
    let gpt_found = read_gpt(device, &mut out)?;

    // --- MBR, if this is not a GPT disk ----------------------------------
    if !gpt_found {
        if let Some(e) = &mbr_entries {
            if !out.protective_mbr && mbr::looks_plausible(e, total) {
                out.partitions.extend(mbr::to_partitions(e));
            }
        }
    }

    // --- fall back to a signature scan ------------------------------------
    if out.partitions.is_empty() || opts.always_scan {
        // A device with no table at all is usually one filesystem occupying the
        // whole device, which is exactly how the basic fixtures are built.
        if have_sector0 {
            if let Some(c) = scan::identify_boot_sector(&sector0, 0, total) {
                out.partitions.push(Partition {
                    index: 1,
                    start: Lba(0),
                    sectors: if c.sectors > 0 { c.sectors } else { total },
                    fs: c.fs,
                    origin: Origin::WholeDevice,
                    label: None,
                    type_guid: None,
                    bootable: false,
                    confidence: c.confidence,
                    evidence: c.evidence,
                });
                out.notes.push(
                    "no partition table; the whole device is a single filesystem".to_string(),
                );
            }
        }

        if out.partitions.is_empty() {
            out.notes
                .push("no usable partition table; scanning for filesystem signatures".to_string());
            let cands = scan::scan_for_filesystems(device, &opts.scan)?;
            let found = cands.len();
            out.partitions
                .extend(scan::candidates_to_partitions(cands, ss));
            out.notes.push(format!(
                "signature scan found {found} candidate(s); {} kept after overlap resolution",
                out.partitions.len()
            ));
        }
    }

    out.normalise();
    Ok(out)
}

/// Read the GPT, preferring the primary and falling back to the backup.
///
/// Returns true if a GPT was found and its entries were parsed.
fn read_gpt(device: &dyn ReadOnlyDevice, out: &mut PartitionTable) -> Result<bool> {
    let ss = device.sector_size();
    let total = device.total_sectors();
    if total < 2 {
        return Ok(false);
    }

    let mut primary = vec![0u8; ss.as_usize()];
    let primary_hdr = if device.read_at(Lba(1), &mut primary).is_ok() {
        gpt::parse_header(&primary)
    } else {
        None
    };

    let mut backup = vec![0u8; ss.as_usize()];
    let backup_hdr = if device.read_at(Lba(total - 1), &mut backup).is_ok() {
        gpt::parse_header(&backup)
    } else {
        None
    };

    out.primary_gpt_ok = primary_hdr.as_ref().is_some_and(|h| h.header_crc_ok);
    out.backup_gpt_ok = backup_hdr.as_ref().is_some_and(|h| h.header_crc_ok);

    // Prefer a CRC-valid primary; otherwise a CRC-valid backup; otherwise a
    // structurally-parseable primary as a last resort.
    let (hdr, from_backup) = match (&primary_hdr, &backup_hdr) {
        (Some(p), _) if p.header_crc_ok => (Some(p.clone()), false),
        (_, Some(b)) if b.header_crc_ok => {
            out.notes.push(
                "the primary GPT header is missing or failed its CRC; recovered from the \
                 backup GPT at the end of the device"
                    .to_string(),
            );
            (Some(b.clone()), true)
        }
        (Some(p), _) => {
            out.notes
                .push("the GPT header failed its CRC; parsing it anyway".to_string());
            (Some(p.clone()), false)
        }
        _ => (None, false),
    };

    let Some(hdr) = hdr else { return Ok(false) };
    if !hdr.looks_sane(total) {
        out.notes
            .push("the GPT header contains implausible values; ignoring it".to_string());
        return Ok(false);
    }

    let want = hdr.entries_bytes();
    // Guard against a header that would have us allocate gigabytes.
    if want > 4 * 1024 * 1024 {
        out.notes.push(format!(
            "the GPT claims a {want}-byte entry array, which is not credible; ignoring it"
        ));
        return Ok(false);
    }

    let mut entries = vec![0u8; want as usize];
    let at = hdr.partition_entry_lba * ss.get() as u64;
    if device.read_bytes_at(at, &mut entries).is_err() {
        out.notes
            .push("the GPT entry array is unreadable".to_string());
        return Ok(false);
    }

    let (mut parts, crc_ok) = gpt::parse_entries(&hdr, &entries);
    if !crc_ok {
        out.notes.push(
            "the GPT entry array failed its CRC; entries may be incomplete or wrong".to_string(),
        );
    }
    if from_backup {
        for p in &mut parts {
            p.origin = Origin::GptBackup;
            p.confidence = 90;
        }
    }
    let found = !parts.is_empty();
    out.partitions.extend(parts);
    Ok(found)
}

/// Refine each partition's filesystem guess by reading its boot sector.
///
/// GPT type GUIDs and MBR type bytes are unreliable: "Microsoft Basic Data"
/// covers NTFS, FAT and exFAT alike, and type 0x07 is used for all three. The
/// boot sector is authoritative, so this is worth a read per partition.
pub fn refine_filesystems(device: &dyn ReadOnlyDevice, tbl: &mut PartitionTable) {
    let ss = device.sector_size();
    for p in &mut tbl.partitions {
        let mut buf = vec![0u8; ss.as_usize()];
        if device.read_at(p.start, &mut buf).is_err() {
            continue;
        }
        if let Some(c) = scan::identify_boot_sector(&buf, p.start.0, device.total_sectors()) {
            if c.fs != p.fs {
                p.evidence.push(format!(
                    "boot sector at LBA {} identifies this as {} (table said {})",
                    p.start, c.fs, p.fs
                ));
                p.fs = c.fs;
            } else {
                p.evidence.push(format!("boot sector confirms {}", c.fs));
            }
        }
    }
}
