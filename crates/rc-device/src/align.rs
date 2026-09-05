//! Sector-aligned buffers.
//!
//! Windows `FILE_FLAG_NO_BUFFERING` and Linux `O_DIRECT` both require the
//! destination buffer to be aligned to the device's sector size, not just to
//! the natural alignment of `Vec<u8>`. A normal `Vec` will fail those reads with
//! `ERROR_INVALID_PARAMETER` / `EINVAL` in a way that is easy to misdiagnose as
//! a bad sector, so all unbuffered reads go through this type.

use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;

/// A heap buffer whose base address and length are both multiples of `align`.
pub struct AlignedBuf {
    ptr: NonNull<u8>,
    len: usize,
    layout: Layout,
}

// The buffer owns its allocation exclusively; there is no interior sharing.
unsafe impl Send for AlignedBuf {}
unsafe impl Sync for AlignedBuf {}

impl AlignedBuf {
    /// Allocate `len` zeroed bytes aligned to `align`.
    ///
    /// `len` is rounded up to a multiple of `align`. Panics if `align` is not a
    /// power of two; every caller derives it from [`crate::SectorSize`], which
    /// enforces that invariant at construction.
    pub fn new(len: usize, align: usize) -> Self {
        assert!(align.is_power_of_two(), "alignment must be a power of two");
        let len = len.next_multiple_of(align).max(align);
        let layout = Layout::from_size_align(len, align).expect("valid layout");
        // SAFETY: `layout` has a non-zero size (len >= align >= 1) and a valid
        // power-of-two alignment, which is exactly the contract of alloc_zeroed.
        let raw = unsafe { alloc_zeroed(layout) };
        let ptr = NonNull::new(raw).unwrap_or_else(|| std::alloc::handle_alloc_error(layout));
        AlignedBuf { ptr, len, layout }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn alignment(&self) -> usize {
        self.layout.align()
    }

    /// True if this buffer satisfies the alignment an unbuffered read needs.
    #[inline]
    pub fn is_aligned_for(&self, sector_size: u32) -> bool {
        let s = sector_size as usize;
        self.layout.align() % s == 0 && self.len % s == 0
    }
}

impl Deref for AlignedBuf {
    type Target = [u8];
    #[inline]
    fn deref(&self) -> &[u8] {
        // SAFETY: ptr is a live allocation of exactly `len` initialised
        // (zeroed) bytes, owned solely by self.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl DerefMut for AlignedBuf {
    #[inline]
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above; &mut self guarantees exclusive access.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        // SAFETY: ptr came from alloc_zeroed with exactly this layout.
        unsafe { dealloc(self.ptr.as_ptr(), self.layout) }
    }
}

impl std::fmt::Debug for AlignedBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AlignedBuf")
            .field("len", &self.len)
            .field("align", &self.layout.align())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_address_and_length_are_aligned() {
        for align in [512usize, 1024, 4096] {
            let b = AlignedBuf::new(100, align);
            assert_eq!(b.as_ptr() as usize % align, 0, "base address");
            assert_eq!(b.len() % align, 0, "length");
            assert!(b.len() >= 100);
        }
    }

    #[test]
    fn length_rounds_up_to_alignment() {
        assert_eq!(AlignedBuf::new(1, 512).len(), 512);
        assert_eq!(AlignedBuf::new(512, 512).len(), 512);
        assert_eq!(AlignedBuf::new(513, 512).len(), 1024);
    }

    #[test]
    fn contents_start_zeroed_and_are_writable() {
        let mut b = AlignedBuf::new(4096, 4096);
        assert!(b.iter().all(|&x| x == 0));
        b[0] = 0xAB;
        b[4095] = 0xCD;
        assert_eq!(b[0], 0xAB);
        assert_eq!(b[4095], 0xCD);
    }

    #[test]
    fn reports_alignment_suitability() {
        let b = AlignedBuf::new(4096, 4096);
        assert!(b.is_aligned_for(512));
        assert!(b.is_aligned_for(4096));
        let b = AlignedBuf::new(512, 512);
        assert!(b.is_aligned_for(512));
        assert!(!b.is_aligned_for(4096));
    }
}
