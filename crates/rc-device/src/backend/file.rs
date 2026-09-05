//! Image-file backend: treats `.dd` / `.raw` / `.img` files as devices.
//!
//! Required by SPEC.md section 5.1 ("Also accept .dd/.raw/.img files as
//! devices, transparently"), and it is what the entire test suite runs against,
//! since fixtures are image files.
//!
//! The file is opened with `File::open`, i.e. read-only. No write handle to a
//! scan source is ever created.

use crate::error::{DeviceError, Result};
use crate::geometry::{DeviceId, DeviceInfo, DeviceKind, Lba, SectorSize, TrimSupport};
use crate::readonly::{sealed::Sealed, validate_read, ReadOnlyDevice};
use crate::registry::ScanSourceGuard;
use std::fs::File;
use std::path::{Path, PathBuf};

pub struct FileDevice {
    file: File,
    info: DeviceInfo,
    _guard: ScanSourceGuard,
}

impl FileDevice {
    /// Open an image file read-only, inferring geometry from its length.
    ///
    /// `sector_size` defaults to 512, which matches every fixture the generator
    /// produces; callers scanning a 4Kn image can override it.
    pub fn open(path: &Path, sector_size: Option<SectorSize>) -> Result<Self> {
        let file = match File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(DeviceError::NotFound(path.to_path_buf()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                return Err(DeviceError::PermissionDenied {
                    path: path.to_path_buf(),
                    detail: e.to_string(),
                })
            }
            Err(e) => return Err(DeviceError::Io(e)),
        };

        let len = file.metadata()?.len();
        let ss = sector_size.unwrap_or(SectorSize::S512);
        if len % ss.get() as u64 != 0 {
            tracing::warn!(
                path = %path.display(),
                len,
                sector_size = ss.get(),
                "image length is not a whole number of sectors; trailing bytes ignored"
            );
        }

        let canonical = std::fs::canonicalize(path)
            .unwrap_or_else(|_| path.to_path_buf())
            .to_string_lossy()
            .to_string();

        let info = DeviceInfo {
            id: DeviceId::File(canonical),
            path: path.to_path_buf(),
            kind: DeviceKind::ImageFile,
            model: None,
            serial: None,
            sector_size: ss,
            physical_sector_size: None,
            total_sectors: len / ss.get() as u64,
            rotational: None,
            // An image file has no controller, so nothing can have been
            // TRIMmed out from under us after the image was taken.
            trim: TrimSupport::No,
            removable: None,
            readable: true,
            access_note: None,
        };

        let guard = ScanSourceGuard::register(info.id.clone(), &info.path);
        Ok(FileDevice {
            file,
            info,
            _guard: guard,
        })
    }

    pub fn path(&self) -> &PathBuf {
        &self.info.path
    }
}

impl Sealed for FileDevice {}

impl ReadOnlyDevice for FileDevice {
    fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn read_at(&self, lba: Lba, buf: &mut [u8]) -> Result<usize> {
        let want = validate_read(&self.info, lba, buf)?;
        let offset = lba.byte_offset(self.info.sector_size);
        read_exact_at_offset(&self.file, &mut buf[..want], offset).map_err(|e| {
            DeviceError::Read {
                path: self.info.path.clone(),
                lba: lba.0,
                source: e,
            }
        })?;
        Ok(want)
    }
}

/// Positional read that does not disturb the file cursor.
///
/// Uses the platform positional-read syscalls so a single `FileDevice` can be
/// shared across the scanner's worker threads without locking.
fn read_exact_at_offset(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.read_exact_at(buf, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let mut done = 0usize;
        while done < buf.len() {
            match file.seek_read(&mut buf[done..], offset + done as u64) {
                Ok(0) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "short read from image file",
                    ))
                }
                Ok(n) => done += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}
