//! Windows raw block device backend.
//!
//! Per SPEC.md section 5.1: `CreateFileW` on `\\.\PhysicalDriveN` and
//! `\\.\Volume{GUID}` with `GENERIC_READ` only, `FILE_SHARE_READ |
//! FILE_SHARE_WRITE`, `FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH`.
//!
//! Two properties of this backend matter for the CLI's honesty requirement:
//!
//! * Enumeration never needs elevation. A handle opened with zero access
//!   rights still answers `IOCTL_STORAGE_QUERY_PROPERTY` and
//!   `IOCTL_DISK_GET_DRIVE_GEOMETRY_EX`, so model, serial, geometry, rotation
//!   and TRIM support are all readable as a normal user. Only reading sector
//!   *data* requires Administrator, and that is reported as
//!   [`DeviceError::NeedsElevation`] rather than a generic access error.
//! * `FILE_FLAG_NO_BUFFERING` makes every read alignment-sensitive: the file
//!   offset, the transfer length and the destination buffer address must all be
//!   multiples of the sector size. Callers get that for free by going through
//!   `read_bytes_at`, which bounces through an [`AlignedBuf`].

use crate::align::AlignedBuf;
use crate::error::{DeviceError, Result};
use crate::geometry::{DeviceId, DeviceInfo, DeviceKind, Lba, SectorSize, TrimSupport};
use crate::readonly::{sealed::Sealed, validate_read, ReadOnlyDevice};
use crate::registry::ScanSourceGuard;

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_NO_BUFFERING, FILE_FLAG_WRITE_THROUGH, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Ioctl::{
    PropertyStandardQuery, StorageDeviceProperty, StorageDeviceSeekPenaltyProperty,
    StorageDeviceTrimProperty, DEVICE_SEEK_PENALTY_DESCRIPTOR, DEVICE_TRIM_DESCRIPTOR,
    DISK_GEOMETRY_EX, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, IOCTL_STORAGE_QUERY_PROPERTY,
    STORAGE_DEVICE_DESCRIPTOR, STORAGE_PROPERTY_QUERY,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

// Defined locally rather than imported: these are frozen Win32 ABI values, and
// pinning them here keeps the crate insensitive to windows-sys module churn.
const GENERIC_READ: u32 = 0x8000_0000;
const ERROR_ACCESS_DENIED: u32 = 5;
const ERROR_FILE_NOT_FOUND: u32 = 2;
const ERROR_PATH_NOT_FOUND: u32 = 3;
const ERROR_NOT_READY: u32 = 21;
const ERROR_INVALID_FUNCTION: u32 = 1;

/// Owned Win32 handle that closes itself.
struct Handle(HANDLE);

// The handle is opened read-only and is only used with positional
// DeviceIoControl/ReadFile calls that carry their own OVERLAPPED offset.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            // SAFETY: self.0 is a live handle we opened and have not closed.
            unsafe { CloseHandle(self.0) };
        }
    }
}

fn wide(path: &str) -> Vec<u16> {
    std::ffi::OsStr::new(path)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Open a device path. `access` of 0 requests metadata-only access, which does
/// not require elevation; `GENERIC_READ` requests the ability to read data.
fn open_handle(path: &str, access: u32, unbuffered: bool) -> std::result::Result<Handle, u32> {
    let wpath = wide(path);
    let mut flags = 0u32;
    if unbuffered {
        flags |= FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH;
    }
    // SAFETY: wpath is a NUL-terminated UTF-16 string that outlives the call;
    // all other arguments are plain scalars or null.
    let h = unsafe {
        CreateFileW(
            wpath.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            flags,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE || h.is_null() {
        // SAFETY: called immediately after the failing call on this thread.
        Err(unsafe { GetLastError() })
    } else {
        Ok(Handle(h))
    }
}

/// Issue an IOCTL whose output is a single POD struct.
///
/// # Safety
/// `T` must be a plain-old-data type matching the layout the IOCTL writes.
unsafe fn ioctl_out<T>(h: HANDLE, code: u32, input: Option<&[u8]>) -> Option<T> {
    let mut out = std::mem::zeroed::<T>();
    let mut returned = 0u32;
    let (in_ptr, in_len) = match input {
        Some(b) => (b.as_ptr() as *const c_void, b.len() as u32),
        None => (std::ptr::null(), 0),
    };
    let ok = DeviceIoControl(
        h,
        code,
        in_ptr,
        in_len,
        &mut out as *mut T as *mut c_void,
        std::mem::size_of::<T>() as u32,
        &mut returned,
        std::ptr::null_mut(),
    );
    if ok != 0 {
        Some(out)
    } else {
        None
    }
}

/// Run a `STORAGE_PROPERTY_QUERY` into a caller-sized byte buffer.
fn storage_query(h: HANDLE, property_id: i32, out: &mut [u8]) -> bool {
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: property_id,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0; 1],
    };
    let qbytes = unsafe {
        std::slice::from_raw_parts(
            &query as *const _ as *const u8,
            std::mem::size_of::<STORAGE_PROPERTY_QUERY>(),
        )
    };
    let mut returned = 0u32;
    // SAFETY: qbytes and out are live slices for the duration of the call.
    unsafe {
        DeviceIoControl(
            h,
            IOCTL_STORAGE_QUERY_PROPERTY,
            qbytes.as_ptr() as *const c_void,
            qbytes.len() as u32,
            out.as_mut_ptr() as *mut c_void,
            out.len() as u32,
            &mut returned,
            std::ptr::null_mut(),
        ) != 0
    }
}

/// Read a NUL-terminated ASCII field addressed by a byte offset inside the
/// STORAGE_DEVICE_DESCRIPTOR blob. Offsets of 0 mean "not supplied".
fn descriptor_string(buf: &[u8], offset: u32) -> Option<String> {
    if offset == 0 || offset as usize >= buf.len() {
        return None;
    }
    let rest = &buf[offset as usize..];
    let end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
    let s = String::from_utf8_lossy(&rest[..end]).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Collect metadata for one device path without reading any data from it.
fn probe(path: &str, kind: DeviceKind) -> Option<DeviceInfo> {
    // Zero access rights: enough for IOCTLs, and works without elevation.
    let h = open_handle(path, 0, false).ok()?;

    // SAFETY: DISK_GEOMETRY_EX is POD and the IOCTL fills it in.
    let geom: DISK_GEOMETRY_EX = unsafe { ioctl_out(h.0, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, None) }?;
    let sector_size = SectorSize::new(geom.Geometry.BytesPerSector).ok()?;
    // On a volume handle the geometry IOCTL is answered by the disk beneath
    // it, so DiskSize is the whole disk's. The volume's own length comes from
    // IOCTL_DISK_GET_LENGTH_INFO, which needs read access; until the device is
    // opened for reading, the filesystem's size stands in (it is never larger
    // than the volume).
    let total_bytes = if kind == DeviceKind::Volume {
        volume_length(h.0).or_else(|| filesystem_size(path))?
    } else {
        geom.DiskSize as u64
    };
    let total_sectors = total_bytes / sector_size.get() as u64;
    if total_sectors == 0 {
        return None;
    }

    let mut model = None;
    let mut serial = None;
    let mut removable = None;
    let mut desc_buf = vec![0u8; 1024];
    if storage_query(h.0, StorageDeviceProperty, &mut desc_buf) {
        // SAFETY: the IOCTL succeeded, so the buffer starts with the descriptor.
        let d = unsafe { &*(desc_buf.as_ptr() as *const STORAGE_DEVICE_DESCRIPTOR) };
        model = descriptor_string(&desc_buf, d.ProductIdOffset);
        serial = descriptor_string(&desc_buf, d.SerialNumberOffset);
        removable = Some(d.RemovableMedia != 0);
    }

    // Absence of a seek penalty means solid state.
    let rotational = unsafe {
        let mut b = [0u8; std::mem::size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>()];
        if storage_query(h.0, StorageDeviceSeekPenaltyProperty, &mut b) {
            let d = &*(b.as_ptr() as *const DEVICE_SEEK_PENALTY_DESCRIPTOR);
            Some(d.IncursSeekPenalty != 0)
        } else {
            None
        }
    };

    let trim = unsafe {
        let mut b = [0u8; std::mem::size_of::<DEVICE_TRIM_DESCRIPTOR>()];
        if storage_query(h.0, StorageDeviceTrimProperty, &mut b) {
            let d = &*(b.as_ptr() as *const DEVICE_TRIM_DESCRIPTOR);
            if d.TrimEnabled != 0 {
                TrimSupport::Yes
            } else {
                TrimSupport::No
            }
        } else {
            TrimSupport::Unknown
        }
    };

    // Can we actually read from it, or would that need elevation?
    let (readable, access_note) = match open_handle(path, GENERIC_READ, true) {
        Ok(_) => (true, None),
        Err(ERROR_ACCESS_DENIED) => (
            false,
            Some("requires Administrator to read sector data".to_string()),
        ),
        Err(code) => (
            false,
            Some(format!("cannot open for reading (error {code})")),
        ),
    };

    let id = match &serial {
        Some(s) if !s.is_empty() => DeviceId::Hardware {
            serial: s.clone(),
            total_bytes,
        },
        _ => DeviceId::Path(path.to_string()),
    };

    Some(DeviceInfo {
        id,
        path: PathBuf::from(path),
        kind,
        model,
        serial,
        sector_size,
        physical_sector_size: None,
        total_sectors,
        rotational,
        trim,
        removable,
        readable,
        access_note,
    })
}

// CTL_CODE(IOCTL_DISK_BASE=7, 0x17, METHOD_BUFFERED, FILE_READ_ACCESS)
const IOCTL_DISK_GET_LENGTH_INFO: u32 = 0x0007_405C;
// CTL_CODE(FILE_DEVICE_FILE_SYSTEM=9, 32, METHOD_NEITHER, FILE_ANY_ACCESS)
const FSCTL_ALLOW_EXTENDED_DASD_IO: u32 = 0x0009_0083;

/// A volume's length in bytes, from a handle with read access.
fn volume_length(h: HANDLE) -> Option<u64> {
    // SAFETY: GET_LENGTH_INFORMATION is a single i64, POD.
    let len: i64 = unsafe { ioctl_out(h, IOCTL_DISK_GET_LENGTH_INFO, None) }?;
    (len > 0).then_some(len as u64)
}

/// The filesystem's size, for a volume path like `\\.\D:`.
fn filesystem_size(path: &str) -> Option<u64> {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let mount = crate::volumes::mount_for_device_path(path)?;
    let w = wide(&mount);
    let (mut free, mut total, mut avail) = (0u64, 0u64, 0u64);
    // SAFETY: w is NUL-terminated; the out-pointers are to live u64s.
    let ok = unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut avail, &mut total, &mut free) };
    (ok != 0 && total > 0).then_some(total)
}

/// Whether a device path can be opened for reading, or why not.
pub(crate) fn can_read(path: &str) -> std::result::Result<(), String> {
    match open_handle(path, GENERIC_READ, true) {
        Ok(_) => Ok(()),
        Err(ERROR_ACCESS_DENIED) => Err("requires Administrator to read sector data".to_string()),
        Err(code) => Err(format!("cannot open for reading (error {code})")),
    }
}

/// True when the current process holds an elevated token.
pub fn is_elevated() -> bool {
    use windows_sys::Win32::Foundation::HANDLE as WHandle;
    use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    const TOKEN_QUERY: u32 = 0x0008;
    let mut token: WHandle = std::ptr::null_mut();
    // SAFETY: token is a valid out-pointer; the handle is closed below.
    unsafe {
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut size = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut _ as *mut c_void,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut size,
        );
        CloseHandle(token);
        ok != 0 && elevation.TokenIsElevated != 0
    }
}

/// Enumerate physical drives by probing `\\.\PhysicalDriveN`.
///
/// Windows has no contiguous-numbering guarantee, so this walks a fixed range
/// and keeps whatever answers. Never reads device contents.
pub fn enumerate() -> Result<Vec<DeviceInfo>> {
    let mut out = Vec::new();
    for n in 0..64u32 {
        let path = format!("\\\\.\\PhysicalDrive{n}");
        match open_handle(&path, 0, false) {
            Ok(_) => {
                if let Some(info) = probe(&path, DeviceKind::PhysicalDisk) {
                    out.push(info);
                }
            }
            Err(ERROR_FILE_NOT_FOUND) | Err(ERROR_PATH_NOT_FOUND) | Err(ERROR_NOT_READY) => {}
            Err(ERROR_ACCESS_DENIED) => {
                tracing::debug!(%path, "access denied during enumeration");
            }
            Err(code) => {
                tracing::trace!(%path, code, "skipping device");
            }
        }
    }
    Ok(out)
}

pub struct WindowsDevice {
    handle: Handle,
    info: DeviceInfo,
    _guard: ScanSourceGuard,
    /// A volume is also registered under its `\\?\Volume{GUID}` name, the name
    /// the output sink resolves destinations to. See `volumes.rs`.
    _aliases: Vec<ScanSourceGuard>,
}

impl WindowsDevice {
    pub fn open(path: &Path) -> Result<Self> {
        let p = path.to_string_lossy().to_string();

        let info = probe(&p, classify(&p)).ok_or_else(|| DeviceError::Unsupported {
            path: path.to_path_buf(),
            detail: "device did not answer geometry queries".to_string(),
        })?;

        let handle = match open_handle(&p, GENERIC_READ, true) {
            Ok(h) => h,
            Err(ERROR_ACCESS_DENIED) => {
                return Err(DeviceError::NeedsElevation {
                    path: path.to_path_buf(),
                    hint: "run from an Administrator shell to read raw sectors",
                })
            }
            Err(ERROR_FILE_NOT_FOUND) | Err(ERROR_PATH_NOT_FOUND) => {
                return Err(DeviceError::NotFound(path.to_path_buf()))
            }
            Err(code) => {
                return Err(DeviceError::PermissionDenied {
                    path: path.to_path_buf(),
                    detail: format!("CreateFileW failed with error {code}"),
                })
            }
        };

        let mut info = info;
        let mut aliases = Vec::new();
        if info.kind == DeviceKind::Volume {
            // The true length, now that the handle can read.
            if let Some(len) = volume_length(handle.0) {
                info.total_sectors = len / info.sector_size.get() as u64;
            }
            // NTFS keeps a backup boot sector past the end of the filesystem;
            // without this, reads of the volume's last sectors are refused.
            // It widens what may be *read*; nothing here can write.
            let mut returned = 0u32;
            // SAFETY: a live handle and no buffers.
            unsafe {
                DeviceIoControl(
                    handle.0,
                    FSCTL_ALLOW_EXTENDED_DASD_IO,
                    std::ptr::null(),
                    0,
                    std::ptr::null_mut(),
                    0,
                    &mut returned,
                    std::ptr::null_mut(),
                )
            };
            // Without its stable name the output sink could not recognise a
            // destination on this volume, so refuse rather than scan unguarded.
            let guid = crate::volumes::volume_guid(&p).ok_or_else(|| DeviceError::Unsupported {
                path: path.to_path_buf(),
                detail: "Windows did not report this volume's identity, so recovered files \
                         could not be kept off it; scan the physical disk instead"
                    .to_string(),
            })?;
            aliases.push(ScanSourceGuard::register(
                crate::geometry::DeviceId::Path(guid.clone()),
                Path::new(&guid),
            ));
        }

        let guard = ScanSourceGuard::register(info.id.clone(), &info.path);
        Ok(WindowsDevice {
            handle,
            info,
            _guard: guard,
            _aliases: aliases,
        })
    }
}

fn classify(path: &str) -> DeviceKind {
    if path.contains("PhysicalDrive") {
        DeviceKind::PhysicalDisk
    } else {
        DeviceKind::Volume
    }
}

impl Sealed for WindowsDevice {}

impl ReadOnlyDevice for WindowsDevice {
    fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn read_at(&self, lba: Lba, buf: &mut [u8]) -> Result<usize> {
        let want = validate_read(&self.info, lba, buf)?;
        let offset = lba.byte_offset(self.info.sector_size);
        let ss = self.info.sector_size;

        // FILE_FLAG_NO_BUFFERING requires the destination *address* to be
        // sector-aligned, not just its length. A caller-supplied &mut [u8]
        // carries no such guarantee, so an unaligned buffer is read through an
        // aligned bounce buffer and copied out afterwards.
        //
        // The two cases are separate branches rather than a shared `dst`
        // binding so the borrows stay disjoint: copying out of a bounce buffer
        // while still holding a mutable borrow of `buf` does not compile.
        if (buf.as_ptr() as usize) % ss.as_usize() == 0 {
            self.read_unbuffered(&mut buf[..want], offset, lba)
        } else {
            let mut bounce = AlignedBuf::new(want, ss.as_usize());
            let done = self.read_unbuffered(&mut bounce[..want], offset, lba)?;
            buf[..done].copy_from_slice(&bounce[..done]);
            Ok(done)
        }
    }
}

impl WindowsDevice {
    /// Issue positional `ReadFile` calls until `dst` is full or the device
    /// stops returning data.
    ///
    /// `dst` must already satisfy the alignment `FILE_FLAG_NO_BUFFERING`
    /// demands; [`ReadOnlyDevice::read_at`] is what guarantees that.
    fn read_unbuffered(&self, dst: &mut [u8], offset: u64, lba: Lba) -> Result<usize> {
        use windows_sys::Win32::Storage::FileSystem::ReadFile;
        use windows_sys::Win32::System::IO::OVERLAPPED;

        let ss = self.info.sector_size;
        let want = dst.len();
        let mut done = 0usize;

        while done < want {
            // The offset travels in the OVERLAPPED rather than a file pointer,
            // so a single handle can be shared across scanner threads.
            let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
            let at = offset + done as u64;
            ov.Anonymous.Anonymous.Offset = (at & 0xFFFF_FFFF) as u32;
            ov.Anonymous.Anonymous.OffsetHigh = (at >> 32) as u32;

            let mut read = 0u32;
            // SAFETY: the handle is a live read-only device handle, and
            // dst[done..] is a live buffer of exactly (want - done) bytes.
            let ok = unsafe {
                ReadFile(
                    self.handle.0,
                    dst[done..].as_mut_ptr(),
                    (want - done) as u32,
                    &mut read,
                    &mut ov,
                )
            };
            if ok == 0 {
                // SAFETY: read immediately after the failing call on this thread.
                let code = unsafe { GetLastError() };
                if code == ERROR_INVALID_FUNCTION {
                    return Err(DeviceError::Unsupported {
                        path: self.info.path.clone(),
                        detail: "device rejected the read (unsupported operation)".to_string(),
                    });
                }
                return Err(DeviceError::Read {
                    path: self.info.path.clone(),
                    lba: lba.0 + (done / ss.as_usize()) as u64,
                    source: std::io::Error::from_raw_os_error(code as i32),
                });
            }
            if read == 0 {
                break; // end of device
            }
            done += read as usize;
        }
        Ok(done)
    }
}
