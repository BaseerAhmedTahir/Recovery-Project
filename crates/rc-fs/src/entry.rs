//! The common vocabulary every filesystem parser speaks (SPEC.md section 5.4).

use std::fmt;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// A contiguous run of clusters belonging to a file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Extent {
    /// First cluster on the volume.
    pub start_cluster: u64,
    pub cluster_count: u64,
    /// A sparse run: no clusters are allocated and the range reads as zero.
    /// NTFS encodes these as a run with no offset field at all.
    pub sparse: bool,
}

impl Extent {
    pub fn new(start_cluster: u64, cluster_count: u64) -> Self {
        Extent {
            start_cluster,
            cluster_count,
            sparse: false,
        }
    }

    pub fn sparse(cluster_count: u64) -> Self {
        Extent {
            start_cluster: 0,
            cluster_count,
            sparse: true,
        }
    }

    pub fn byte_len(&self, cluster_bytes: u64) -> u64 {
        self.cluster_count * cluster_bytes
    }
}

/// Where a file's bytes live.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum DataLocation {
    /// The content is stored inside the metadata record itself. NTFS does this
    /// for small files; there are no clusters to recover, the bytes are simply
    /// present.
    Resident(Vec<u8>),
    /// A list of cluster runs.
    Runs(Vec<Extent>),
    /// The first cluster is known but the chain is not, which is the normal
    /// case for a deleted FAT file: FAT zeroes the cluster chain on delete, so
    /// only the starting point survives and the rest is a guess.
    FirstClusterOnly(u64),
    /// Nothing is known about where the content was.
    #[default]
    Unknown,
}

impl DataLocation {
    /// Total clusters described, ignoring sparse runs.
    pub fn allocated_clusters(&self) -> u64 {
        match self {
            DataLocation::Runs(r) => r
                .iter()
                .filter(|e| !e.sparse)
                .map(|e| e.cluster_count)
                .sum(),
            _ => 0,
        }
    }

    /// True when the layout is known exactly rather than assumed.
    pub fn is_exact(&self) -> bool {
        matches!(self, DataLocation::Resident(_) | DataLocation::Runs(_))
    }

    pub fn extent_count(&self) -> usize {
        match self {
            DataLocation::Runs(r) => r.len(),
            _ => 0,
        }
    }
}

/// Timestamps, normalised to Unix epoch nanoseconds UTC.
///
/// The three filesystems disagree profoundly about time, so normalising here
/// keeps that mess out of the rest of the engine:
///
/// * **NTFS** counts 100-nanosecond intervals since 1601-01-01 UTC.
/// * **FAT** stores DOS date/time in *local* time with two-second granularity
///   and no timezone at all, so an exact instant is not recoverable. What is
///   stored is preserved and flagged.
/// * **exFAT** uses the same DOS fields plus a 10-millisecond counter and an
///   explicit UTC offset byte, so it can be converted properly when the offset
///   is present.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Timestamps {
    pub created: Option<i64>,
    pub modified: Option<i64>,
    pub accessed: Option<i64>,
    /// NTFS only: when the MFT record itself last changed.
    pub mft_changed: Option<i64>,
    /// False when the source had no timezone information, i.e. FAT. The
    /// instant is then only accurate to whatever local time the writing
    /// machine was set to.
    pub utc_known: bool,
}

/// Whether an entry was in use when the filesystem was last written.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "lowercase"))]
pub enum EntryState {
    /// Live and referenced. Enumerated only so the scoring engine knows which
    /// clusters are occupied; hidden from the default view per SPEC.md 5.4.
    Allocated,
    /// Deleted. This is what we are here to recover.
    Deleted,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "lowercase"))]
pub enum EntryKind {
    File,
    Directory,
}

/// How confident we are in the reconstructed path.
#[derive(Clone, PartialEq, Eq, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum PathConfidence {
    /// Every ancestor was resolved and validated.
    Exact,
    /// Reached by walking a live directory tree.
    Traversed,
    /// The parent reference pointed at an MFT record that has since been
    /// reused for a different file, so the original directory is gone. The
    /// name is still right; the path is not knowable.
    ///
    /// This case exists because silently attaching a file to whatever now
    /// occupies its old parent slot produces confident, wrong output - the
    /// exact failure mode that makes recovery results untrustworthy.
    ParentReused { detail: String },
    /// The parent could not be read at all.
    ParentUnknown,
}

impl PathConfidence {
    pub fn is_trustworthy(&self) -> bool {
        matches!(self, PathConfidence::Exact | PathConfidence::Traversed)
    }
}

impl fmt::Display for PathConfidence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PathConfidence::Exact => f.write_str("exact"),
            PathConfidence::Traversed => f.write_str("traversed"),
            PathConfidence::ParentReused { .. } => f.write_str("parent-reused"),
            PathConfidence::ParentUnknown => f.write_str("parent-unknown"),
        }
    }
}

/// One file or directory recovered from filesystem metadata.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Entry {
    /// Filesystem-specific record id: MFT record number, or a synthetic id for
    /// FAT and exFAT derived from the directory entry's location.
    pub id: u64,
    /// The name alone, with no path.
    pub name: String,
    /// Reconstructed path relative to the volume root, using `/` separators.
    /// `None` when the parent could not be established.
    pub path: Option<String>,
    pub path_confidence: PathConfidence,
    pub kind: EntryKind,
    pub state: EntryState,
    /// Logical size in bytes.
    pub size: u64,
    /// Bytes actually allocated on disk, when known.
    pub allocated_size: u64,
    pub timestamps: Timestamps,
    pub location: DataLocation,
    /// Parent record id, when the filesystem records one.
    pub parent_id: Option<u64>,
    /// Free-form notes explaining anything unusual about this entry, surfaced
    /// by the CLI so a surprising result can be understood rather than
    /// guessed at.
    pub notes: Vec<String>,
}

impl Entry {
    pub fn is_deleted(&self) -> bool {
        self.state == EntryState::Deleted
    }

    /// Path if trustworthy, otherwise just the name.
    pub fn display_path(&self) -> String {
        match (&self.path, self.path_confidence.is_trustworthy()) {
            (Some(p), true) => p.clone(),
            _ => format!("?/{}", self.name),
        }
    }
}

/// What a parser found, plus what it could not do.
#[derive(Clone, Debug, Default)]
pub struct ScanResult {
    pub entries: Vec<Entry>,
    /// Records that were present but unreadable, with the reason. Reported
    /// rather than silently dropped, so a low recovery count is explicable.
    pub damaged: Vec<String>,
    pub notes: Vec<String>,
}

impl ScanResult {
    pub fn deleted(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(|e| e.is_deleted())
    }

    pub fn allocated(&self) -> impl Iterator<Item = &Entry> {
        self.entries
            .iter()
            .filter(|e| e.state == EntryState::Allocated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extent_arithmetic() {
        let e = Extent::new(100, 8);
        assert_eq!(e.byte_len(4096), 32768);
        assert!(!e.sparse);
        let s = Extent::sparse(4);
        assert!(s.sparse);
        assert_eq!(s.start_cluster, 0);
    }

    #[test]
    fn allocated_clusters_ignores_sparse_runs() {
        let loc = DataLocation::Runs(vec![
            Extent::new(10, 4),
            Extent::sparse(100),
            Extent::new(200, 6),
        ]);
        assert_eq!(loc.allocated_clusters(), 10);
        assert_eq!(loc.extent_count(), 3);
    }

    #[test]
    fn only_exact_layouts_are_marked_exact() {
        assert!(DataLocation::Runs(vec![Extent::new(1, 1)]).is_exact());
        assert!(DataLocation::Resident(vec![1, 2, 3]).is_exact());
        assert!(
            !DataLocation::FirstClusterOnly(5).is_exact(),
            "a FAT deleted file's chain is a guess, not a fact"
        );
        assert!(!DataLocation::Unknown.is_exact());
    }

    #[test]
    fn path_confidence_gates_display() {
        let mut e = Entry {
            id: 1,
            name: "photo.jpg".into(),
            path: Some("dcim/photo.jpg".into()),
            path_confidence: PathConfidence::Exact,
            kind: EntryKind::File,
            state: EntryState::Deleted,
            size: 100,
            allocated_size: 4096,
            timestamps: Timestamps::default(),
            location: DataLocation::Unknown,
            parent_id: Some(5),
            notes: vec![],
        };
        assert_eq!(e.display_path(), "dcim/photo.jpg");

        // A stale parent must not present a confident path.
        e.path_confidence = PathConfidence::ParentReused {
            detail: "sequence 3 != 7".into(),
        };
        assert_eq!(e.display_path(), "?/photo.jpg");
        assert!(!e.path_confidence.is_trustworthy());
    }
}
