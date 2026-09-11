//! Platform backend selection.
//!
//! Every backend produces a value implementing the sealed
//! [`crate::ReadOnlyDevice`] trait, so callers never branch on platform.

pub mod file;

#[cfg(windows)]
pub mod windows;

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(target_os = "macos")]
pub mod macos;

use crate::error::{DeviceError, Result};
use crate::geometry::{DeviceInfo, SectorSize};
use crate::readonly::ReadOnlyDevice;
use std::path::Path;

/// Extensions treated as raw disk images rather than device nodes.
const IMAGE_EXTENSIONS: &[&str] = &["dd", "raw", "img", "image", "bin"];

/// Decide whether `path` names an image file or a real device node.
///
/// Anything that exists as a regular file is treated as an image regardless of
/// extension, so `scan foo.backup` works; the extension list only matters for
/// paths that do not exist yet.
pub fn looks_like_image(path: &Path) -> bool {
    if path.is_file() {
        return true;
    }
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| IMAGE_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// Open any supported target read-only.
/// Open an image file with the page cache bypassed.
///
/// Only image files: a raw device is already opened unbuffered by its platform
/// backend, so there is no second mode to choose for one.
pub fn open_unbuffered(
    path: &Path,
    sector_size: Option<SectorSize>,
) -> Result<Box<dyn ReadOnlyDevice>> {
    if looks_like_image(path) {
        return Ok(Box::new(file::FileDevice::open_unbuffered(path, sector_size)?));
    }
    // A raw device is unbuffered already; opening it normally is the same thing.
    open(path, sector_size)
}

pub fn open(path: &Path, sector_size: Option<SectorSize>) -> Result<Box<dyn ReadOnlyDevice>> {
    if looks_like_image(path) {
        return Ok(Box::new(file::FileDevice::open(path, sector_size)?));
    }

    #[cfg(windows)]
    {
        return Ok(Box::new(windows::WindowsDevice::open(path)?));
    }
    #[cfg(target_os = "linux")]
    {
        return Ok(Box::new(linux::LinuxDevice::open(path)?));
    }
    #[cfg(target_os = "macos")]
    {
        return Ok(Box::new(macos::MacosDevice::open(path)?));
    }
    #[allow(unreachable_code)]
    Err(DeviceError::Unsupported {
        path: path.to_path_buf(),
        detail: "no raw device backend for this platform".to_string(),
    })
}

/// Enumerate the machine's block devices without reading any of them.
pub fn enumerate() -> Result<Vec<DeviceInfo>> {
    #[cfg(windows)]
    {
        return windows::enumerate();
    }
    #[cfg(target_os = "linux")]
    {
        return linux::enumerate();
    }
    #[cfg(target_os = "macos")]
    {
        return macos::enumerate();
    }
    #[allow(unreachable_code)]
    Ok(Vec::new())
}

/// True when the process can open raw devices for reading.
pub fn is_elevated() -> bool {
    #[cfg(windows)]
    {
        return windows::is_elevated();
    }
    #[cfg(target_os = "linux")]
    {
        return linux::is_elevated();
    }
    #[cfg(target_os = "macos")]
    {
        return macos::is_elevated();
    }
    #[allow(unreachable_code)]
    false
}
