//! Image-file backend: treats `.dd` / `.raw` / `.img` files as devices.
//!
//! Required by SPEC.md section 5.1 ("Also accept .dd/.raw/.img files as
//! devices, transparently"), and it is what the entire test suite runs against,
//! since fixtures are image files.
//!
//! The file is opened with `File::open`, i.e. read-only. No write handle to a
//! scan source is ever created.
//!
//! # Unbuffered mode
//!
//! [`FileDevice::open_unbuffered`] opens the image with `FILE_FLAG_NO_BUFFERING`
//! on Windows and `O_DIRECT` on Linux, so reads go to the storage device rather
//! than being served from the operating system's page cache.
//!
//! This exists for one reason: every throughput figure this project had
//! produced came from a 512 MiB fixture that fits in RAM, which means every one
//! of them measured memory bandwidth and none measured a disk. Unbuffered reads
//! are the difference between a benchmark and a number.
//!
//! Unbuffered I/O is alignment-sensitive in all three dimensions - offset,
//! length and buffer *address* - and the alignment that matters is the
//! underlying volume's physical sector, which can be 4096 on a modern drive even
//! when the image's own logical sector is 512. So every read here is rounded out
//! to [`UNBUFFERED_ALIGN`] and staged through an [`AlignedBuf`], unless the
//! caller's request already satisfies all three, in which case it is read in
//! place. The scanner's 4 MiB block reads at 4 MiB offsets take the direct path.

use crate::align::AlignedBuf;
use crate::error::{DeviceError, Result};
use crate::geometry::{DeviceId, DeviceInfo, DeviceKind, Lba, SectorSize, TrimSupport};
use crate::readonly::{sealed::Sealed, validate_read, ReadOnlyDevice};
use crate::registry::ScanSourceGuard;
use std::fs::File;
use std::path::{Path, PathBuf};

/// Alignment used for every unbuffered read.
///
/// 4096 rather than the image's 512-byte logical sector, because the rule is
/// set by the physical sector of the volume the image *lives on*, and 4Kn
/// drives are common. 4096 is a multiple of every sector size in use, so it is
/// correct on 512e and 4Kn volumes alike; reading a little more than asked is
/// the only cost.
pub const UNBUFFERED_ALIGN: usize = 4096;

pub struct FileDevice {
    file: File,
    info: DeviceInfo,
    /// Set when the file was opened to bypass the page cache.
    unbuffered: bool,
    _guard: ScanSourceGuard,
}

impl FileDevice {
    /// Open an image file read-only, inferring geometry from its length.
    ///
    /// `sector_size` defaults to 512, which matches every fixture the generator
    /// produces; callers scanning a 4Kn image can override it.
    pub fn open(path: &Path, sector_size: Option<SectorSize>) -> Result<Self> {
        Self::open_mode(path, sector_size, false)
    }

    /// Open an image file read-only with the page cache bypassed.
    ///
    /// Reads then reflect the storage device the image sits on, not RAM. See
    /// the module documentation for why this exists and what it costs.
    pub fn open_unbuffered(path: &Path, sector_size: Option<SectorSize>) -> Result<Self> {
        Self::open_mode(path, sector_size, true)
    }

    pub fn is_unbuffered(&self) -> bool {
        self.unbuffered
    }

    fn open_mode(path: &Path, sector_size: Option<SectorSize>, unbuffered: bool) -> Result<Self> {
        let opened = if unbuffered {
            open_uncached(path)
        } else {
            File::open(path)
        };
        let file = match opened {
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
            unbuffered,
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
        let result = if self.unbuffered {
            read_unbuffered(
                &self.file,
                &mut buf[..want],
                offset,
                self.info.total_bytes(),
            )
        } else {
            read_exact_at_offset(&self.file, &mut buf[..want], offset)
        };
        result.map_err(|e| DeviceError::Read {
            path: self.info.path.clone(),
            lba: lba.0,
            source: e,
        })?;
        Ok(want)
    }
}

/// Open read-only with the page cache bypassed. Never grants write access: the
/// flag changes how reads are served, not what the handle may do.
fn open_uncached(path: &Path) -> std::io::Result<File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_NO_BUFFERING. Defined here rather than pulled in from
        // windows-sys because the file backend has no other Win32 dependency.
        const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;
        opts.custom_flags(FILE_FLAG_NO_BUFFERING);
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_DIRECT);
    }
    // macOS has no open-time flag; F_NOCACHE is set after opening. Everything
    // else falls back to a buffered handle, which is correct but not uncached.
    let file = opts.open(path)?;
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::io::AsRawFd;
        // SAFETY: fcntl on a descriptor we own, with an integer argument.
        unsafe {
            libc::fcntl(file.as_raw_fd(), libc::F_NOCACHE, 1);
        }
    }
    Ok(file)
}

/// Read `buf.len()` bytes at `offset` through an unbuffered handle.
///
/// Rounds the range out to [`UNBUFFERED_ALIGN`], reads the covering window into
/// an aligned staging buffer, and copies out the part asked for. When the
/// caller's buffer address, offset and length are all already aligned it reads
/// in place instead, which is the case the scanner's block reads hit.
///
/// Unlike the buffered path, a short read is not automatically an error: the
/// rounded-up window may legitimately extend past the end of the file. It is an
/// error only if the bytes the caller actually asked for were not all returned.
fn read_unbuffered(file: &File, buf: &mut [u8], offset: u64, file_len: u64) -> std::io::Result<()> {
    let a = UNBUFFERED_ALIGN as u64;
    let aligned_in_place = offset % a == 0
        && (buf.len() as u64) % a == 0
        && (buf.as_ptr() as usize) % UNBUFFERED_ALIGN == 0;

    if aligned_in_place {
        let got = read_available(file, buf, offset)?;
        return if got == buf.len() {
            Ok(())
        } else {
            Err(short(got, buf.len()))
        };
    }

    let start = offset - offset % a;
    let end = offset + buf.len() as u64;
    let end_up = end.div_ceil(a) * a;
    let window = (end_up - start) as usize;

    let mut staging = AlignedBuf::new(window, UNBUFFERED_ALIGN);
    let got = read_available(file, &mut staging[..window], start)?;

    let skip = (offset - start) as usize;
    // Everything the caller asked for must have been covered. Past the end of
    // the file the read is allowed to stop early; before it, it is not.
    let need = skip + buf.len();
    let available_in_file = file_len.saturating_sub(start) as usize;
    if need > available_in_file || got < need {
        return Err(short(got.saturating_sub(skip), buf.len()));
    }
    buf.copy_from_slice(&staging[skip..skip + buf.len()]);
    Ok(())
}

/// Read as much as the file holds at `offset`, up to `buf.len()`.
fn read_available(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
    let mut done = 0usize;
    while done < buf.len() {
        let n = positional_read(file, &mut buf[done..], offset + done as u64);
        match n {
            Ok(0) => break,
            Ok(n) => done += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(done)
}

fn positional_read(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.read_at(buf, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        file.seek_read(buf, offset)
    }
}

fn short(got: usize, want: usize) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::UnexpectedEof,
        format!("short unbuffered read: {got} of {want} bytes"),
    )
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
