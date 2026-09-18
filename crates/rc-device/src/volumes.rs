//! Drive letters: the volumes a person actually thinks in.
//!
//! A physical disk (`\\.\PhysicalDrive0`) is the right thing to scan when a
//! partition table is damaged or a drive was formatted, but people lose files
//! from "C:" or "the USB stick, E:". Windows exposes each lettered volume as a
//! raw device of its own, `\\.\E:`, which starts directly with the
//! filesystem's boot sector and is read exactly like a disk.
//!
//! Listing volumes needs no elevation. Reading one does, like a disk.
//!
//! Safety: a volume opened for scanning is registered under its stable
//! `\\?\Volume{GUID}` name as well as its drive-letter path, so the output
//! sink refuses any destination on the same volume however it is named. Other
//! partitions of the same physical disk do not overlap it and stay writable;
//! scanning the whole disk still refuses every partition on it.

use std::path::PathBuf;

/// One lettered volume.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct VolumeInfo {
    /// `C`, `D`, ...
    pub letter: char,
    /// The raw device to open: `\\.\C:`.
    pub device_path: PathBuf,
    /// Where it is mounted: `C:\`.
    pub mount: String,
    /// The name the person gave it, if any ("Data", "SANDISK").
    pub label: Option<String>,
    /// `NTFS`, `FAT32`, `exFAT`, ...
    pub filesystem: Option<String>,
    pub total_bytes: u64,
    pub free_bytes: u64,
    /// A USB stick or card reader rather than an internal drive.
    pub removable: bool,
    /// Whether this process can read its sectors.
    pub readable: bool,
    /// Why not, when it cannot.
    pub note: Option<String>,
}

/// Every lettered local volume: fixed and removable drives. Network shares,
/// optical drives and RAM disks are left out; there is nothing to recover
/// from them this way. Empty on platforms without drive letters.
pub fn enumerate_volumes() -> Vec<VolumeInfo> {
    #[cfg(windows)]
    {
        windows_impl::enumerate()
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

/// The stable `\\?\Volume{GUID}\` name of a lettered volume or a volume path.
/// `None` when Windows cannot say, or on other platforms.
pub fn volume_guid(path: &str) -> Option<String> {
    #[cfg(windows)]
    {
        windows_impl::volume_guid(path)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        None
    }
}

/// `C:\` for `\\.\C:`, `\\?\C:` or `C:`; `None` for anything else.
pub fn mount_for_device_path(path: &str) -> Option<String> {
    let s = path
        .strip_prefix("\\\\.\\")
        .or_else(|| path.strip_prefix("\\\\?\\"))
        .unwrap_or(path);
    let mut chars = s.chars();
    let letter = chars.next()?;
    if !letter.is_ascii_alphabetic() || chars.next() != Some(':') {
        return None;
    }
    let rest: String = chars.collect();
    (rest.is_empty() || rest == "\\").then(|| format!("{}:\\", letter.to_ascii_uppercase()))
}

#[cfg(windows)]
mod windows_impl {
    use super::{mount_for_device_path, VolumeInfo};
    use std::os::windows::ffi::OsStrExt;
    use std::path::PathBuf;
    use windows_sys::Win32::Storage::FileSystem::{
        GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDriveStringsW, GetVolumeInformationW,
        GetVolumeNameForVolumeMountPointW,
    };

    // GetDriveTypeW results; frozen Win32 values.
    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_FIXED: u32 = 3;

    fn wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    fn from_wide(buf: &[u16]) -> String {
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end])
    }

    pub fn volume_guid(path: &str) -> Option<String> {
        let mount = if path.starts_with("\\\\?\\Volume{") {
            format!("{}\\", path.trim_end_matches('\\'))
        } else {
            mount_for_device_path(path)?
        };
        let wmount = wide(&mount);
        let mut out = vec![0u16; 64];
        // SAFETY: wmount is NUL-terminated; out is a live buffer of its length.
        let ok = unsafe {
            GetVolumeNameForVolumeMountPointW(wmount.as_ptr(), out.as_mut_ptr(), out.len() as u32)
        };
        if ok == 0 {
            return None;
        }
        Some(from_wide(&out))
    }

    pub fn enumerate() -> Vec<VolumeInfo> {
        let mut buf = vec![0u16; 512];
        // SAFETY: buf is a live buffer of the length passed.
        let n = unsafe { GetLogicalDriveStringsW(buf.len() as u32, buf.as_mut_ptr()) } as usize;
        if n == 0 || n > buf.len() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for root in buf[..n].split(|&c| c == 0).filter(|s| !s.is_empty()) {
            let mount = String::from_utf16_lossy(root);
            let Some(letter) = mount.chars().next().filter(|c| c.is_ascii_alphabetic()) else {
                continue;
            };
            let wroot = wide(&mount);
            // SAFETY: wroot is NUL-terminated.
            let kind = unsafe { GetDriveTypeW(wroot.as_ptr()) };
            if kind != DRIVE_FIXED && kind != DRIVE_REMOVABLE {
                continue;
            }

            let mut label = vec![0u16; 261];
            let mut fs = vec![0u16; 261];
            // SAFETY: every buffer is live and its length is passed with it;
            // the optional out-parameters we do not want are null.
            let have_info = unsafe {
                GetVolumeInformationW(
                    wroot.as_ptr(),
                    label.as_mut_ptr(),
                    label.len() as u32,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    fs.as_mut_ptr(),
                    fs.len() as u32,
                )
            } != 0;

            let (mut free, mut total, mut _avail_free) = (0u64, 0u64, 0u64);
            // SAFETY: out-pointers are to live u64s.
            unsafe { GetDiskFreeSpaceExW(wroot.as_ptr(), &mut free, &mut total, &mut _avail_free) };

            let device_path = format!("\\\\.\\{}:", letter.to_ascii_uppercase());
            let (readable, note) = match super::super::backend::windows_can_read(&device_path) {
                Ok(()) => (true, None),
                Err(note) => (false, Some(note)),
            };
            let nonempty = |s: String| if s.trim().is_empty() { None } else { Some(s) };
            out.push(VolumeInfo {
                letter: letter.to_ascii_uppercase(),
                device_path: PathBuf::from(device_path),
                mount,
                label: if have_info {
                    nonempty(from_wide(&label))
                } else {
                    None
                },
                filesystem: if have_info {
                    nonempty(from_wide(&fs))
                } else {
                    None
                },
                total_bytes: total,
                free_bytes: free,
                removable: kind == DRIVE_REMOVABLE,
                readable,
                note: if have_info {
                    note
                } else {
                    Some("no media, or the drive is not ready".to_string())
                },
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_paths_map_to_their_mount() {
        assert_eq!(mount_for_device_path("\\\\.\\C:").as_deref(), Some("C:\\"));
        assert_eq!(mount_for_device_path("\\\\?\\d:").as_deref(), Some("D:\\"));
        assert_eq!(mount_for_device_path("e:").as_deref(), Some("E:\\"));
        assert_eq!(mount_for_device_path("E:\\").as_deref(), Some("E:\\"));
        assert_eq!(mount_for_device_path("\\\\.\\PhysicalDrive0"), None);
        assert_eq!(mount_for_device_path("C:\\Users"), None);
        assert_eq!(mount_for_device_path("disk.img"), None);
    }

    /// Listing needs no elevation and must find the volume this test runs
    /// from, with a size and a filesystem.
    #[cfg(windows)]
    #[test]
    fn the_volume_holding_this_crate_is_listed() {
        let here = std::env::current_dir().unwrap();
        let letter = here
            .to_string_lossy()
            .chars()
            .next()
            .unwrap()
            .to_ascii_uppercase();
        let vols = enumerate_volumes();
        let v = vols
            .iter()
            .find(|v| v.letter == letter)
            .unwrap_or_else(|| panic!("{letter}: not in {vols:#?}"));
        assert!(v.total_bytes > 0 && v.free_bytes <= v.total_bytes, "{v:#?}");
        assert!(v.filesystem.is_some(), "{v:#?}");
        assert_eq!(v.device_path, PathBuf::from(format!("\\\\.\\{letter}:")));
        let guid = volume_guid(&v.device_path.to_string_lossy()).expect("a volume GUID");
        assert!(guid.starts_with("\\\\?\\Volume{"), "{guid}");
    }
}
