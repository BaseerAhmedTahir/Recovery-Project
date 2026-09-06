//! FAT12/16/32 parser (SPEC.md section 5.4).
//!
//! Directories are walked recursively, so a file's path comes from the
//! traversal that reached it rather than from any stored parent pointer.
//!
//! The hard part is what deletion destroys. FAT clears the file's entire
//! cluster chain in the FAT when a file is unlinked, keeping only the starting
//! cluster in the directory entry. The layout of a deleted file is therefore
//! **not knowable from the filesystem**: assuming contiguity is a guess, and
//! it is wrong exactly when the file was fragmented. That guess is recorded as
//! [`DataLocation::FirstClusterOnly`] rather than as a set of runs, so nothing
//! downstream mistakes it for fact. `rc-bifrag` in Milestone 4 is what turns
//! the guess into an answer.

pub mod bpb;
pub mod dir;

use crate::entry::{
    DataLocation, Entry, EntryKind, EntryState, Extent, PathConfidence, ScanResult,
};
use crate::error::Result;
use bpb::{Bpb, FatKind};
use dir::{RawEntry, ShortEntry};
use rc_device::ReadOnlyDevice;
use std::collections::HashSet;

/// Bound on directory recursion, so a corrupt volume cannot loop forever.
const MAX_DEPTH: usize = 64;
/// Bound on clusters followed for one directory.
const MAX_DIR_CLUSTERS: usize = 4096;

pub struct FatVolume<'a> {
    device: &'a dyn ReadOnlyDevice,
    base: u64,
    pub bpb: Bpb,
    fat: Vec<u8>,
}

impl<'a> FatVolume<'a> {
    pub fn open(device: &'a dyn ReadOnlyDevice, base: u64) -> Result<Self> {
        let mut sector = vec![0u8; 512.max(device.sector_size().as_usize())];
        device.read_bytes_at(base, &mut sector)?;
        let bpb = bpb::parse_bpb(&sector)?;

        // The FAT is needed to follow live files' chains. A deleted file's
        // chain is gone, so this does not help there.
        let fat_bytes = bpb.fat_size_sectors as u64 * bpb.bytes_per_sector as u64;
        let mut fat = vec![0u8; fat_bytes.min(64 * 1024 * 1024) as usize];
        device.read_bytes_at(base + bpb.fat_offset(0), &mut fat)?;

        Ok(FatVolume {
            device,
            base,
            bpb,
            fat,
        })
    }

    fn read_cluster(&self, cluster: u32) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; self.bpb.cluster_bytes() as usize];
        if let Some(off) = self.bpb.cluster_offset(cluster) {
            self.device.read_bytes_at(self.base + off, &mut buf)?;
        }
        Ok(buf)
    }

    /// Scan the whole volume.
    pub fn scan(&self) -> Result<ScanResult> {
        let mut result = ScanResult::default();
        let mut visited: HashSet<u32> = HashSet::new();

        // The root directory is a fixed region on FAT12/16 and a normal
        // cluster chain on FAT32.
        let root_bytes = match self.bpb.kind {
            FatKind::Fat32 => {
                let chain = bpb::follow_chain(
                    &self.fat,
                    &self.bpb,
                    self.bpb.root_cluster,
                    MAX_DIR_CLUSTERS,
                );
                visited.insert(self.bpb.root_cluster);
                let mut b = Vec::new();
                for c in chain {
                    b.extend_from_slice(&self.read_cluster(c)?);
                }
                b
            }
            _ => {
                let n = self.bpb.root_dir_sectors() as u64 * self.bpb.bytes_per_sector as u64;
                let mut b = vec![0u8; n as usize];
                self.device
                    .read_bytes_at(self.base + self.bpb.root_dir_offset(), &mut b)?;
                b
            }
        };

        self.walk_directory(&root_bytes, "", 0, &mut visited, &mut result)?;

        result.notes.push(format!(
            "{:?} volume, {} clusters of {} bytes; {} entries recovered",
            self.bpb.kind,
            self.bpb.cluster_count,
            self.bpb.cluster_bytes(),
            result.entries.len()
        ));
        Ok(result)
    }

    /// Parse one directory's bytes, recursing into subdirectories.
    fn walk_directory(
        &self,
        data: &[u8],
        prefix: &str,
        depth: usize,
        visited: &mut HashSet<u32>,
        result: &mut ScanResult,
    ) -> Result<()> {
        if depth > MAX_DEPTH {
            result.notes.push(format!(
                "directory nesting past {MAX_DEPTH} levels was not followed"
            ));
            return Ok(());
        }

        // Classify every slot first; rebuilding a name needs to look backwards
        // at the entries preceding the 8.3 entry.
        let raws: Vec<RawEntry> = data
            .chunks_exact(dir::ENTRY_SIZE)
            .map(dir::parse_raw)
            .collect();

        let mut subdirs: Vec<(String, u32, bool)> = Vec::new();

        for (i, r) in raws.iter().enumerate() {
            let RawEntry::Short(short) = r else {
                // `End` does not terminate the walk: entries beyond it can
                // still hold recoverable deleted names, and a deleted
                // directory's tail is exactly where they live.
                continue;
            };

            let recovered = dir::rebuild_name(short, &raws[..i]);
            let name = recovered.best();
            if name.is_empty() {
                continue;
            }

            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };

            let mut notes = recovered.notes.clone();
            if short.deleted && !recovered.checksum_validated && recovered.long_name.is_some() {
                notes.push(
                    "the long name was recovered but its checksum could not be tied to this \
                     8.3 entry; it may belong to a neighbouring file"
                        .to_string(),
                );
            }

            let entry = self.to_entry(short, &name, &path, notes);

            if short.is_directory() {
                subdirs.push((path.clone(), short.first_cluster, short.deleted));
            }
            result.entries.push(entry);
        }

        for (path, cluster, deleted) in subdirs {
            if cluster < 2 || cluster > self.bpb.max_cluster() {
                continue;
            }
            if !visited.insert(cluster) {
                continue; // already walked, or a cycle
            }

            let bytes = if deleted {
                // A deleted directory's chain is gone from the FAT, so only
                // its first cluster can be located with certainty.
                self.read_cluster(cluster)?
            } else {
                let chain = bpb::follow_chain(&self.fat, &self.bpb, cluster, MAX_DIR_CLUSTERS);
                let mut b = Vec::new();
                for c in chain {
                    b.extend_from_slice(&self.read_cluster(c)?);
                }
                b
            };

            self.walk_directory(&bytes, &path, depth + 1, visited, result)?;
        }

        Ok(())
    }

    fn to_entry(
        &self,
        short: &ShortEntry,
        name: &str,
        path: &str,
        mut notes: Vec<String>,
    ) -> Entry {
        let location = if short.is_directory() || short.first_cluster < 2 {
            DataLocation::Unknown
        } else if short.deleted {
            // The chain was zeroed on delete. Only the start survives.
            notes.push(
                "FAT clears the cluster chain when a file is deleted, so only the first \
                 cluster is known; the rest of the layout is not recoverable from the \
                 filesystem"
                    .to_string(),
            );
            DataLocation::FirstClusterOnly(short.first_cluster as u64)
        } else {
            let clusters =
                bpb::follow_chain(&self.fat, &self.bpb, short.first_cluster, MAX_DIR_CLUSTERS);
            DataLocation::Runs(runs_from_clusters(&clusters))
        };

        Entry {
            id: short.first_cluster as u64,
            name: name.to_string(),
            path: Some(path.to_string()),
            // The path came from walking the tree that contains it.
            path_confidence: PathConfidence::Traversed,
            kind: if short.is_directory() {
                EntryKind::Directory
            } else {
                EntryKind::File
            },
            state: if short.deleted {
                EntryState::Deleted
            } else {
                EntryState::Allocated
            },
            size: short.size as u64,
            allocated_size: (short.size as u64).next_multiple_of(self.bpb.cluster_bytes().max(1)),
            timestamps: short.timestamps,
            location,
            parent_id: None,
            notes,
        }
    }
}

/// Collapse a cluster list into contiguous runs.
pub fn runs_from_clusters(clusters: &[u32]) -> Vec<Extent> {
    let mut out: Vec<Extent> = Vec::new();
    for &c in clusters {
        match out.last_mut() {
            Some(last) if last.start_cluster + last.cluster_count == c as u64 => {
                last.cluster_count += 1;
            }
            _ => out.push(Extent::new(c as u64, 1)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contiguous_clusters_collapse_into_one_run() {
        assert_eq!(
            runs_from_clusters(&[10, 11, 12, 13]),
            vec![Extent::new(10, 4)]
        );
    }

    #[test]
    fn gaps_produce_separate_runs() {
        let runs = runs_from_clusters(&[10, 11, 20, 21, 22, 40]);
        assert_eq!(
            runs,
            vec![Extent::new(10, 2), Extent::new(20, 3), Extent::new(40, 1)]
        );
    }

    #[test]
    fn an_empty_chain_produces_no_runs() {
        assert!(runs_from_clusters(&[]).is_empty());
    }

    #[test]
    fn a_deleted_file_never_claims_exact_runs() {
        // The distinction that keeps a guess from being reported as fact.
        let guess = DataLocation::FirstClusterOnly(42);
        assert!(!guess.is_exact());
        assert!(DataLocation::Runs(vec![Extent::new(42, 3)]).is_exact());
    }
}
