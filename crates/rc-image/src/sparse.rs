//! Sparse output support.
//!
//! A clone of a mostly-empty 2 TB drive should not occupy 2 TB on the
//! destination. On Linux and macOS this is automatic: seeking past the end of a
//! file and writing leaves an unallocated hole. NTFS requires the file to be
//! explicitly marked sparse first, otherwise `set_len` physically allocates and
//! zero-fills.

use std::fs::File;

/// Mark a file sparse. No-op on platforms where it is the default behaviour.
#[cfg(windows)]
pub fn mark_sparse(file: &File) -> bool {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Ioctl::FSCTL_SET_SPARSE;
    use windows_sys::Win32::System::IO::DeviceIoControl;

    let handle = file.as_raw_handle() as isize as *mut c_void;
    let mut returned = 0u32;
    // SAFETY: handle is a live writable file handle owned by `file`; the IOCTL
    // takes no input or output buffer.
    let ok = unsafe {
        DeviceIoControl(
            handle,
            FSCTL_SET_SPARSE,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        // Not fatal: FAT32 and exFAT destinations have no sparse support, and a
        // dense clone is still a correct clone.
        tracing::debug!("FSCTL_SET_SPARSE was refused; the output will be dense");
        false
    } else {
        true
    }
}

#[cfg(not(windows))]
pub fn mark_sparse(_file: &File) -> bool {
    // Unix filesystems create holes implicitly on sparse writes.
    true
}

/// True if `buf` is entirely zero.
///
/// A run of zeros can be skipped instead of written, which is what keeps a
/// sparse clone small. Compares 8 bytes at a time; the optimiser turns this
/// into vector loads.
pub fn is_all_zero(buf: &[u8]) -> bool {
    let (prefix, words, suffix) = unsafe { buf.align_to::<u64>() };
    prefix.iter().all(|&b| b == 0)
        && words.iter().all(|&w| w == 0)
        && suffix.iter().all(|&b| b == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_all_zero_buffers() {
        assert!(is_all_zero(&[]));
        assert!(is_all_zero(&[0u8; 1]));
        assert!(is_all_zero(&[0u8; 4096]));
        assert!(is_all_zero(&vec![0u8; 100_000]));
    }

    #[test]
    fn detects_non_zero_bytes_anywhere() {
        for pos in [0usize, 1, 7, 8, 63, 511, 4095] {
            let mut b = vec![0u8; 4096];
            b[pos] = 1;
            assert!(!is_all_zero(&b), "byte at {pos} should be detected");
        }
    }

    /// The unaligned prefix/suffix path must be exercised, not just the fast
    /// aligned middle.
    #[test]
    fn handles_unaligned_slices() {
        let buf = vec![0u8; 4096];
        for start in 0..16 {
            assert!(is_all_zero(&buf[start..]));
        }
        let mut buf = vec![0u8; 4096];
        buf[3] = 0xFF;
        for start in 0..3 {
            assert!(!is_all_zero(&buf[start..]), "start={start}");
        }
        for start in 4..16 {
            assert!(is_all_zero(&buf[start..]), "start={start}");
        }
    }
}
