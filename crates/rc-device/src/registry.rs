//! Registry of devices currently open as scan sources.
//!
//! This is one half of the safety invariant in SPEC.md section 4.1: an output
//! sink must refuse to open any path that resolves to a device we are currently
//! reading. `rc-device` records what is being scanned; `rc-image::OutputSink`
//! consults that record before it will create a file.
//!
//! Registration is tied to a RAII guard held by the open device, so the entry
//! disappears when the device is closed even if the caller panics.

use crate::geometry::DeviceId;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// A device currently open for scanning.
///
/// Carries the path as well as the id, because `rc-image` compares a resolved
/// destination against the *path* a device was opened by (`\\.\PhysicalDrive1`,
/// `/dev/sda`) as well as against its hardware identity.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ScanSource {
    pub id: DeviceId,
    pub path: PathBuf,
}

struct Entry {
    count: usize,
    path: PathBuf,
}

fn table() -> &'static Mutex<HashMap<DeviceId, Entry>> {
    static TABLE: OnceLock<Mutex<HashMap<DeviceId, Entry>>> = OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// RAII registration for a device opened as a scan source.
///
/// Held inside the open device handle. Dropping it decrements the refcount, so
/// the same device opened twice stays registered until the last handle closes.
#[derive(Debug)]
pub struct ScanSourceGuard {
    id: DeviceId,
}

impl ScanSourceGuard {
    pub(crate) fn register(id: DeviceId, path: &Path) -> ScanSourceGuard {
        let mut t = table().lock().unwrap_or_else(|e| e.into_inner());
        t.entry(id.clone())
            .and_modify(|e| e.count += 1)
            .or_insert_with(|| Entry {
                count: 1,
                path: path.to_path_buf(),
            });
        tracing::debug!(device = %id, path = %path.display(), "registered as scan source");
        ScanSourceGuard { id }
    }

    pub fn id(&self) -> &DeviceId {
        &self.id
    }
}

impl Drop for ScanSourceGuard {
    fn drop(&mut self) {
        let mut t = table().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = t.get_mut(&self.id) {
            entry.count -= 1;
            if entry.count == 0 {
                t.remove(&self.id);
                tracing::debug!(device = %self.id, "unregistered as scan source");
            }
        }
    }
}

/// True if `id` names a device currently open for scanning.
pub fn is_registered_source(id: &DeviceId) -> bool {
    table()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(id)
}

/// Snapshot of the ids of every device currently registered as a scan source.
pub fn registered_sources() -> Vec<DeviceId> {
    table()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .keys()
        .cloned()
        .collect()
}

/// Snapshot of every registered scan source, with the path it was opened by.
///
/// `rc-image::sink` uses this to decide whether a write destination resolves to
/// storage that is currently being scanned.
pub fn registered_sources_detailed() -> Vec<ScanSource> {
    table()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|(id, entry)| ScanSource {
            id: id.clone(),
            path: entry.path.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: &str) -> DeviceId {
        DeviceId::File(n.to_string())
    }

    #[test]
    fn guard_registers_and_unregisters() {
        let key = id("guard_registers_and_unregisters");
        assert!(!is_registered_source(&key));
        {
            let _g = ScanSourceGuard::register(key.clone(), Path::new("/test"));
            assert!(is_registered_source(&key));
        }
        assert!(!is_registered_source(&key));
    }

    #[test]
    fn nested_guards_are_refcounted() {
        let key = id("nested_guards_are_refcounted");
        let a = ScanSourceGuard::register(key.clone(), Path::new("/test"));
        let b = ScanSourceGuard::register(key.clone(), Path::new("/test"));
        assert!(is_registered_source(&key));
        drop(a);
        assert!(
            is_registered_source(&key),
            "still registered while a second handle is open"
        );
        drop(b);
        assert!(!is_registered_source(&key));
    }

    #[test]
    fn unrelated_devices_do_not_collide() {
        let a = id("unrelated_a");
        let b = id("unrelated_b");
        let _g = ScanSourceGuard::register(a.clone(), Path::new("/test"));
        assert!(is_registered_source(&a));
        assert!(!is_registered_source(&b));
    }
}
