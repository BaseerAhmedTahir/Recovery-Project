//! NTFS parser (SPEC.md section 5.4).
//!
//! Reads the `$MFT`, recovers deleted records, and rebuilds each file's
//! original path by following parent references.
//!
//! Order matters here and is enforced by construction: every record goes
//! through [`fixup::apply_fixups`] before any field is read. See that module
//! for why.

pub mod attr;
pub mod boot;
pub mod fixup;
pub mod mft;
pub mod runlist;

use crate::entry::{Entry, Extent, ScanResult};
use crate::error::{FsError, Result};
use boot::NtfsBoot;
use mft::{MftRecord, FIRST_USER_RECORD, ROOT_RECORD};
use rc_device::ReadOnlyDevice;
use std::collections::HashMap;

/// Upper bound on MFT records held in memory at once.
///
/// A 512 MiB fixture has a few thousand; a 4 TB drive can have tens of
/// millions, which is what `rc-index`'s disk-backed store exists for in
/// Milestone 3. Until then this bound keeps a large volume from exhausting
/// memory silently, and the limit is reported rather than hit quietly.
const MAX_RECORDS_IN_MEMORY: u64 = 2_000_000;

// Checked at compile time so the bound cannot be lowered to a value that would
// silently skip records on an ordinary volume.
const _: () = assert!(MAX_RECORDS_IN_MEMORY >= 1_000_000);

pub struct NtfsVolume<'a> {
    device: &'a dyn ReadOnlyDevice,
    /// Byte offset of the volume within the device.
    base: u64,
    pub boot: NtfsBoot,
    /// The $MFT's own cluster runs.
    mft_extents: Vec<Extent>,
    mft_size: u64,
}

impl<'a> NtfsVolume<'a> {
    /// Open the NTFS volume starting `base` bytes into `device`.
    pub fn open(device: &'a dyn ReadOnlyDevice, base: u64) -> Result<Self> {
        let mut sector = vec![0u8; 512.max(device.sector_size().as_usize())];
        device.read_bytes_at(base, &mut sector)?;
        let boot = boot::parse_boot(&sector)?;

        let mut vol = NtfsVolume {
            device,
            base,
            boot,
            mft_extents: Vec::new(),
            mft_size: 0,
        };

        // Record 0 is the $MFT itself; its $DATA runlist is the map we need to
        // read every other record.
        let mut buf = vec![0u8; boot.mft_record_bytes as usize];
        vol.device
            .read_bytes_at(base + boot.mft_offset(), &mut buf)?;
        let rec = mft::parse_record(&mut buf, 0, &boot)?.ok_or_else(|| FsError::Unrecognised {
            detail: "MFT record 0 is not a FILE record; this volume's $MFT is unreadable".into(),
        })?;

        if rec.extents.is_empty() {
            return Err(FsError::Unrecognised {
                detail: "$MFT record 0 has no data runs".into(),
            });
        }
        vol.mft_size = if rec.data_size > 0 {
            rec.data_size
        } else {
            rec.extents.iter().map(|e| e.cluster_count).sum::<u64>() * boot.cluster_bytes()
        };
        vol.mft_extents = rec.extents;
        Ok(vol)
    }

    pub fn record_count(&self) -> u64 {
        self.mft_size / self.boot.mft_record_bytes as u64
    }

    /// Translate a virtual cluster number within the $MFT to a byte offset on
    /// the device.
    fn mft_vcn_offset(&self, vcn: u64) -> Option<u64> {
        let mut seen = 0u64;
        for e in &self.mft_extents {
            if vcn < seen + e.cluster_count {
                if e.sparse {
                    return None;
                }
                let within = vcn - seen;
                return Some(self.base + (e.start_cluster + within) * self.boot.cluster_bytes());
            }
            seen += e.cluster_count;
        }
        None
    }

    /// Byte offset of MFT record `index` on the device.
    fn record_offset(&self, index: u64) -> Option<u64> {
        let rec_bytes = self.boot.mft_record_bytes as u64;
        let cluster_bytes = self.boot.cluster_bytes();
        let byte_in_mft = index * rec_bytes;
        let vcn = byte_in_mft / cluster_bytes;
        let within = byte_in_mft % cluster_bytes;
        self.mft_vcn_offset(vcn).map(|o| o + within)
    }

    /// Read and parse a single record.
    pub fn read_record(&self, index: u64) -> Result<Option<MftRecord>> {
        let Some(off) = self.record_offset(index) else {
            return Ok(None);
        };
        let mut buf = vec![0u8; self.boot.mft_record_bytes as usize];
        if self.device.read_bytes_at(off, &mut buf)? == 0 {
            return Ok(None);
        }
        mft::parse_record(&mut buf, index, &self.boot)
    }

    /// Scan the whole MFT.
    ///
    /// Allocated entries are returned alongside deleted ones because the
    /// scoring engine needs to know which clusters are occupied by live files
    /// (SPEC.md section 5.4); the CLI hides them by default.
    pub fn scan(&self) -> Result<ScanResult> {
        let total = self.record_count();
        let mut result = ScanResult::default();

        if total > MAX_RECORDS_IN_MEMORY {
            result.notes.push(format!(
                "the $MFT holds {total} records, above the in-memory limit of \
                 {MAX_RECORDS_IN_MEMORY}; only the first {MAX_RECORDS_IN_MEMORY} were scanned. \
                 A disk-backed index lands in Milestone 3."
            ));
        }
        let scan_limit = total.min(MAX_RECORDS_IN_MEMORY);

        // Pass 1: read every record.
        let mut records: Vec<MftRecord> = Vec::new();
        for index in 0..scan_limit {
            match self.read_record(index) {
                Ok(Some(r)) => records.push(r),
                Ok(None) => {}
                Err(e) => result.damaged.push(format!("MFT record {index}: {e}")),
            }
        }

        // Pass 2: index directories so paths can be walked upwards. Both live
        // and deleted directories are indexed, because a deleted file's parent
        // is frequently a deleted directory.
        let mut dirs: HashMap<u64, (u16, String, u64, u16)> = HashMap::new();
        for r in &records {
            if !r.is_directory {
                continue;
            }
            if let Some(n) = r.best_name() {
                dirs.insert(
                    r.index,
                    (
                        r.sequence,
                        n.name.clone(),
                        n.parent_index,
                        n.parent_sequence,
                    ),
                );
            }
        }
        // The root is its own parent and has no useful $FILE_NAME.
        dirs.insert(
            ROOT_RECORD,
            (
                dirs.get(&ROOT_RECORD).map(|d| d.0).unwrap_or(5),
                String::new(),
                ROOT_RECORD,
                0,
            ),
        );

        let resolve = |idx: u64| -> Option<(u16, String, u64, u16)> { dirs.get(&idx).cloned() };

        // Pass 3: convert. Extension records (base_record != 0) are skipped;
        // their attributes belong to the base record.
        for r in &records {
            if r.base_record != 0 {
                continue;
            }
            if r.index < FIRST_USER_RECORD {
                continue; // NTFS's own metadata files
            }
            if r.names.is_empty() {
                continue; // nothing recoverable without a name
            }
            let mut entry = mft::record_to_entry(r, &self.boot, &resolve);
            if !r.attribute_list.is_empty() {
                self.follow_attribute_list(r, &mut entry, &mut result);
            }
            result.entries.push(entry);
        }

        result.notes.push(format!(
            "scanned {} of {} MFT records; {} usable entries, {} damaged",
            scan_limit,
            total,
            result.entries.len(),
            result.damaged.len()
        ));
        Ok(result)
    }

    /// Follow $ATTRIBUTE_LIST into extension records and merge their $DATA
    /// extents into the base entry.
    ///
    /// A file fragmented past what one MFT record can describe spills its
    /// $DATA into other records. Without this the file reports the extents of
    /// its first fragment only, or none at all, which makes heavily fragmented
    /// files - the ones that matter most in a real recovery - look empty.
    fn follow_attribute_list(&self, rec: &MftRecord, entry: &mut Entry, result: &mut ScanResult) {
        use crate::entry::DataLocation;

        let mut extra: Vec<Extent> = Vec::new();
        let mut followed = 0usize;
        for e in &rec.attribute_list {
            if e.type_code != attr::ATTR_DATA || e.record_index == rec.index {
                continue;
            }
            match self.read_record(e.record_index) {
                Ok(Some(ext)) => {
                    if ext.sequence != e.record_sequence {
                        entry.notes.push(format!(
                            "$ATTRIBUTE_LIST points at MFT record {} sequence {} but that \
                             record now has sequence {}; its extents were not merged",
                            e.record_index, e.record_sequence, ext.sequence
                        ));
                        continue;
                    }
                    extra.extend(ext.extents.iter().copied());
                    followed += 1;
                }
                Ok(None) => {}
                Err(err) => result.damaged.push(format!(
                    "extension record {} for {}: {err}",
                    e.record_index, entry.name
                )),
            }
        }

        if !extra.is_empty() {
            let mut all = match &entry.location {
                DataLocation::Runs(r) => r.clone(),
                _ => Vec::new(),
            };
            all.extend(extra);
            entry.location = DataLocation::Runs(all);
            entry.notes.push(format!(
                "extents merged from {followed} extension record(s) via $ATTRIBUTE_LIST"
            ));
        }
    }
}
