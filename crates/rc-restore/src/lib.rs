//! Writing recovered files out (SPEC.md 4.1, 4.2).
//!
//! Everything recovered - a deleted filesystem entry, a carved candidate, a
//! reassembled file - is written here, and only through `rc-image`'s
//! [`OutputSink`], which refuses any destination that resolves to a device
//! being scanned. The output directory is checked before anything is created in
//! it, so not even the directory is made on the source.
//!
//! Each written file is streamed from the device in cluster-sized reads and
//! hashed as it goes, and recorded in `restore-manifest.json` with where its
//! bytes came from and how sure that layout is:
//!
//! - `exact` - the filesystem recorded the runs;
//! - `resident` - the bytes were inside the metadata record;
//! - `assumed-contiguous` - only the first cluster was known (a deleted FAT or
//!   exFAT file) and the rest was assumed to follow it;
//! - `carved` - a carved candidate read contiguously;
//! - `reassembled` - pieces chosen by `rc-bifrag`.
//!
//! Existing files are never overwritten: a name already taken gets ` (1)`,
//! ` (2)`... Names are made safe for the host filesystem, and path components
//! of `.`/`..` are dropped so nothing lands outside the output directory.

use rc_device::ReadOnlyDevice;
use rc_fs::{DataLocation, Entry, Geometry};
use rc_image::{check_destination, OutputSink, SinkOptions, UnknownBackingPolicy};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Image(#[from] rc_image::ImageError),
    #[error(transparent)]
    Device(#[from] rc_device::DeviceError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    NotRestorable(String),
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Layout {
    Exact,
    Resident,
    AssumedContiguous,
    Carved,
    Reassembled,
}

/// A byte range of the device, in file order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Span {
    pub offset: u64,
    pub length: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Restored {
    /// Where it came from: a path on the volume, or a description.
    pub source: String,
    pub dest: PathBuf,
    pub bytes: u64,
    pub sha256: String,
    pub layout: Layout,
    /// Anything the reader of the manifest should know, such as the rating.
    pub notes: Vec<String>,
}

pub struct Restorer {
    root: PathBuf,
    taken: HashSet<PathBuf>,
    written: Vec<Restored>,
}

/// The nearest ancestor of `p` that exists, to check before creating anything.
fn existing_ancestor(p: &Path) -> Option<PathBuf> {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(p)
    };
    abs.ancestors().find(|a| a.exists()).map(Path::to_path_buf)
}

impl Restorer {
    /// Prepare to write under `root`. Refused if `root` or the nearest existing
    /// directory above it is on a device being scanned.
    pub fn new(root: &Path) -> Result<Restorer> {
        check_destination(root, UnknownBackingPolicy::Warn)?;
        if let Some(a) = existing_ancestor(root) {
            check_destination(&a, UnknownBackingPolicy::Warn)?;
        }
        if root.is_file() {
            return Err(Error::NotRestorable(format!(
                "{} is a file, not a directory",
                root.display()
            )));
        }
        std::fs::create_dir_all(root)?;
        Ok(Restorer {
            root: root.to_path_buf(),
            taken: HashSet::new(),
            written: Vec::new(),
        })
    }

    pub fn written(&self) -> &[Restored] {
        &self.written
    }

    /// A destination for `rel` (slash-separated) that is safe and not taken.
    fn destination(&mut self, rel: &str) -> PathBuf {
        let mut p = self.root.clone();
        let parts: Vec<&str> = rel
            .split(['/', '\\'])
            .filter(|s| !s.is_empty() && *s != "." && *s != "..")
            .collect();
        for part in &parts {
            p.push(safe_component(part));
        }
        if parts.is_empty() {
            p.push("unnamed");
        }
        let (stem, ext) = {
            let name = p
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            match name.rsplit_once('.') {
                Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
                _ => (name, String::new()),
            }
        };
        let mut candidate = p.clone();
        let mut n = 1;
        while candidate.exists() || self.taken.contains(&candidate) {
            candidate = p.with_file_name(format!("{stem} ({n}){ext}"));
            n += 1;
        }
        self.taken.insert(candidate.clone());
        candidate
    }

    /// Write `spans` of the device, in order, as `rel` under the root. Sparse
    /// spans (`offset == u64::MAX`) are written as zeros.
    #[allow(clippy::too_many_arguments)]
    pub fn spans(
        &mut self,
        dev: &dyn ReadOnlyDevice,
        rel: &str,
        spans: &[Span],
        total: u64,
        layout: Layout,
        source: String,
        notes: Vec<String>,
    ) -> Result<&Restored> {
        let dest = self.destination(rel);
        let mut sink = OutputSink::create(
            &dest,
            &SinkOptions {
                allow_overwrite: false,
                sparse: false,
                unknown_backing: UnknownBackingPolicy::Warn,
            },
        )?;
        let mut hasher = Sha256::new();
        let mut out = 0u64;
        let mut buf = vec![0u8; 1 << 20];
        'spans: for s in spans {
            let mut done = 0u64;
            while done < s.length {
                if out >= total {
                    break 'spans;
                }
                let want = (s.length - done).min(buf.len() as u64).min(total - out) as usize;
                let chunk = &mut buf[..want];
                if s.offset == u64::MAX {
                    chunk.fill(0);
                } else {
                    let n = dev.read_bytes_at(s.offset + done, chunk)?;
                    chunk[n..].fill(0);
                }
                sink.write_at(out, chunk)?;
                hasher.update(&*chunk);
                out += want as u64;
                done += want as u64;
            }
        }
        sink.set_len(out)?;
        sink.finish()?;
        let mut notes = notes;
        if out < total {
            notes.push(format!(
                "only {out} of {total} bytes had a location; the file is short"
            ));
        }
        self.written.push(Restored {
            source,
            dest,
            bytes: out,
            sha256: hex::encode(hasher.finalize()),
            layout,
            notes,
        });
        Ok(self.written.last().expect("just pushed"))
    }

    /// Write a filesystem entry's content, laid out as its location says.
    pub fn entry(
        &mut self,
        dev: &dyn ReadOnlyDevice,
        geometry: &Geometry,
        entry: &Entry,
        notes: Vec<String>,
    ) -> Result<&Restored> {
        let rel = entry
            .path
            .clone()
            .unwrap_or_else(|| format!("_no_path/{}", entry.name));
        let source = entry.display_path();
        let c = geometry.cluster_bytes.max(1);
        match &entry.location {
            DataLocation::Resident(bytes) => {
                let dest = self.destination(&rel);
                let data = &bytes[..(entry.size as usize).min(bytes.len())];
                let mut sink = OutputSink::create(
                    &dest,
                    &SinkOptions {
                        allow_overwrite: false,
                        sparse: false,
                        unknown_backing: UnknownBackingPolicy::Warn,
                    },
                )?;
                sink.write_at(0, data)?;
                sink.set_len(data.len() as u64)?;
                sink.finish()?;
                self.written.push(Restored {
                    source,
                    dest,
                    bytes: data.len() as u64,
                    sha256: hex::encode(Sha256::digest(data)),
                    layout: Layout::Resident,
                    notes,
                });
                Ok(self.written.last().expect("just pushed"))
            }
            DataLocation::Runs(runs) => {
                let mut spans = Vec::new();
                for r in runs {
                    let length = r.cluster_count * c;
                    if r.sparse {
                        spans.push(Span {
                            offset: u64::MAX,
                            length,
                        });
                        continue;
                    }
                    let offset = geometry.cluster_offset(r.start_cluster).ok_or_else(|| {
                        Error::NotRestorable(format!(
                            "{source}: cluster {} is outside the volume",
                            r.start_cluster
                        ))
                    })?;
                    spans.push(Span { offset, length });
                }
                self.spans(dev, &rel, &spans, entry.size, Layout::Exact, source, notes)
            }
            DataLocation::FirstClusterOnly(first) => {
                let offset = geometry.cluster_offset(*first).ok_or_else(|| {
                    Error::NotRestorable(format!("{source}: first cluster is outside the volume"))
                })?;
                let mut notes = notes;
                notes.push(
                    "only the first cluster was recorded; the rest was assumed to follow it".into(),
                );
                let span = Span {
                    offset,
                    length: entry.size.div_ceil(c) * c,
                };
                self.spans(
                    dev,
                    &rel,
                    &[span],
                    entry.size,
                    Layout::AssumedContiguous,
                    source,
                    notes,
                )
            }
            DataLocation::Unknown => Err(Error::NotRestorable(format!(
                "{source}: where its content was is not known"
            ))),
        }
    }

    /// Write `restore-manifest.json` and return what was written.
    pub fn finish(self) -> Result<Vec<Restored>> {
        let manifest = self.root.join("restore-manifest.json");
        let mut path = manifest.clone();
        let mut n = 1;
        while path.exists() {
            path = self.root.join(format!("restore-manifest ({n}).json"));
            n += 1;
        }
        let json = serde_json::to_vec_pretty(&self.written)
            .map_err(|e| Error::NotRestorable(e.to_string()))?;
        let mut sink = OutputSink::create(
            &path,
            &SinkOptions {
                allow_overwrite: false,
                sparse: false,
                unknown_backing: UnknownBackingPolicy::Warn,
            },
        )?;
        sink.write_at(0, &json)?;
        sink.finish()?;
        Ok(self.written)
    }
}

/// A path component the host filesystem will accept, keeping as much of the
/// original as possible.
pub fn safe_component(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
            c if (c as u32) < 32 => '_',
            c => c,
        })
        .collect();
    while out.ends_with('.') || out.ends_with(' ') {
        out.pop();
    }
    if out.is_empty() {
        out.push('_');
    }
    let stem = out.split('.').next().unwrap_or("").to_ascii_uppercase();
    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    if RESERVED.contains(&stem.as_str()) {
        out.insert(0, '_');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn components_are_made_safe() {
        assert_eq!(safe_component("a:b?.txt"), "a_b_.txt");
        assert_eq!(safe_component("CON.txt"), "_CON.txt");
        assert_eq!(safe_component("trailing. "), "trailing");
        assert_eq!(safe_component("..."), "_");
        assert_eq!(safe_component("résumé.pdf"), "résumé.pdf");
    }
}
