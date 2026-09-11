//! The unbuffered file path must return exactly what the buffered path returns.
//!
//! Unbuffered I/O is alignment-sensitive in three independent dimensions -
//! the file offset, the length, and the *address* of the caller's buffer - and
//! the read path handles that by rounding each request out to a 4 KiB window,
//! staging it through an aligned buffer, and copying out the part asked for.
//! Every piece of that is a place for an off-by-one: the window start, the
//! window length, the skip into the staging buffer, the tail past end of file.
//!
//! A single read cannot catch those, because each one on its own returns
//! plausible bytes. So this compares the unbuffered path against the buffered
//! one across every combination that matters, and fails on the first byte that
//! differs. The same idea as the hardware smoke test's misaligned-buffer check,
//! which is what found the bounce-path bugs in the raw-device backend.
//!
//! This is also the one place the read path's immutability is re-checked for
//! the new mode: opening with a cache-bypass flag changes how reads are served,
//! and must not change what the handle is allowed to do.

use rc_device::{Lba, SectorSize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

/// An image whose every byte is a function of its offset, so a read from the
/// wrong place is detectable rather than merely different.
fn patterned_image(name: &str, len: usize) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "rc-unbuffered-{}-{name}.img",
        std::process::id()
    ));
    let data: Vec<u8> = (0..len)
        .map(|i| {
            let x = i as u32;
            (x ^ (x >> 7) ^ (x >> 13)).wrapping_mul(2_654_435_761) as u8
        })
        .collect();
    std::fs::write(&path, &data).expect("write image");
    path
}

struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn unbuffered_reads_match_buffered_reads_at_every_alignment() {
    // 1 MiB plus a sub-sector tail, so the file does not end on a 4 KiB line
    // and the past-the-end handling is exercised.
    let path = patterned_image("match", (1 << 20) + 3 * 512);
    let _c = Cleanup(path.clone());

    let buffered = rc_device::open(&path, None).expect("open buffered");
    let unbuffered = rc_device::open_unbuffered(&path, None).expect("open unbuffered");
    let ss = SectorSize::S512.as_usize();
    let total = buffered.total_sectors();

    let mut compared = 0usize;
    // Sector-aligned LBAs (the trait's contract), including ones that are not
    // 4 KiB-aligned, which is the case the staging window exists for.
    let lbas: Vec<u64> = vec![0, 1, 3, 7, 8, 9, 15, 63, 64, 65, 511, 1000, total - 8, total - 1];
    // Lengths in sectors, including ones that straddle a 4 KiB boundary.
    let lens: Vec<u64> = vec![1, 2, 7, 8, 9, 16, 33, 256];
    // Buffer address skews: aligned, and deliberately not.
    let skews: Vec<usize> = vec![0, 1, 3, 17, 511, 4095];

    for &lba in &lbas {
        for &n in &lens {
            if lba + n > total {
                continue;
            }
            let want_len = (n as usize) * ss;

            let mut expect = vec![0u8; want_len];
            buffered
                .read_exact_at(Lba(lba), &mut expect)
                .unwrap_or_else(|e| panic!("buffered read lba {lba} n {n}: {e}"));

            for &skew in &skews {
                // Offset into an oversized Vec to control the address alignment.
                let mut backing = vec![0xAAu8; want_len + 8192];
                let base = backing.as_ptr() as usize;
                let pad = (4096 - base % 4096) % 4096;
                let at = pad + skew;
                let window = &mut backing[at..at + want_len];

                unbuffered
                    .read_exact_at(Lba(lba), window)
                    .unwrap_or_else(|e| {
                        panic!("unbuffered read lba {lba} n {n} skew {skew}: {e}")
                    });

                if window[..] != expect[..] {
                    let first = window
                        .iter()
                        .zip(&expect)
                        .position(|(a, b)| a != b)
                        .unwrap_or(0);
                    panic!(
                        "unbuffered read disagreed with buffered: lba {lba}, {n} sectors, \
                         buffer skew {skew}; first difference at byte {first} \
                         (got {:#04x}, want {:#04x})",
                        window[first], expect[first]
                    );
                }
                compared += 1;
            }
        }
    }
    eprintln!("compared {compared} (lba, length, buffer-alignment) combinations");
    assert!(compared > 400, "the grid was meant to be large; only {compared} ran");
}

/// Byte-offset reads go through a second layer of rounding in the trait's
/// provided `read_bytes_at`, on top of the unbuffered window. Both layers have
/// to compose correctly.
#[test]
fn unbuffered_byte_offset_reads_match_buffered() {
    let path = patterned_image("bytes", 256 * 1024 + 700);
    let _c = Cleanup(path.clone());

    let buffered = rc_device::open(&path, None).expect("open buffered");
    let unbuffered = rc_device::open_unbuffered(&path, None).expect("open unbuffered");
    let total = buffered.total_bytes();

    let mut compared = 0;
    for offset in [0u64, 1, 511, 512, 513, 4095, 4096, 4097, 10_001, 131_071] {
        for len in [1usize, 2, 100, 511, 512, 513, 4095, 4096, 4097, 20_000] {
            if offset + len as u64 > total {
                continue;
            }
            let mut a = vec![0u8; len];
            let mut b = vec![0u8; len];
            let na = buffered.read_bytes_at(offset, &mut a).expect("buffered");
            let nb = unbuffered.read_bytes_at(offset, &mut b).expect("unbuffered");
            assert_eq!(na, nb, "byte counts differ at offset {offset} len {len}");
            assert_eq!(a, b, "bytes differ at offset {offset} len {len}");
            compared += 1;
        }
    }
    assert!(compared > 60, "only {compared} combinations ran");
}

/// The scanner's large sequential reads should take the in-place path. This
/// checks they are correct, not that they are fast - the speed is measured by
/// the benchmark, which is where a number like that belongs.
#[test]
fn large_aligned_block_reads_are_correct() {
    let path = patterned_image("block", 8 << 20);
    let _c = Cleanup(path.clone());

    let buffered = rc_device::open(&path, None).expect("open buffered");
    let unbuffered = rc_device::open_unbuffered(&path, None).expect("open unbuffered");

    let block = 4 << 20;
    for lba in [0u64, (4 << 20) / 512] {
        let mut a = rc_device::AlignedBuf::new(block, 4096);
        let mut b = rc_device::AlignedBuf::new(block, 4096);
        buffered.read_exact_at(Lba(lba), &mut a[..block]).expect("buffered");
        unbuffered.read_exact_at(Lba(lba), &mut b[..block]).expect("unbuffered");
        assert!(a[..block] == b[..block], "4 MiB block at lba {lba} differs");
    }
}

/// Opening with a cache-bypass flag must not change what the handle can do.
/// The same invariant the Milestone 1 immutability test asserts, for the new
/// open mode.
#[test]
fn reading_unbuffered_does_not_modify_the_image() {
    let path = patterned_image("immutable", 2 << 20);
    let _c = Cleanup(path.clone());

    let before = Sha256::digest(std::fs::read(&path).expect("read"));
    {
        let dev = rc_device::open_unbuffered(&path, None).expect("open unbuffered");
        let mut buf = vec![0u8; 64 * 1024];
        let mut lba = 0u64;
        while lba < dev.total_sectors() {
            let n = (buf.len() / 512).min((dev.total_sectors() - lba) as usize);
            dev.read_exact_at(Lba(lba), &mut buf[..n * 512]).expect("read");
            lba += n as u64;
        }
    }
    let after = Sha256::digest(std::fs::read(&path).expect("read"));
    assert_eq!(before, after, "an unbuffered read modified the image");
}

/// Prove the cache-bypass flag actually took effect.
///
/// Every test above would pass identically if the flag were silently ignored,
/// because a buffered handle returns the same bytes. What only a genuinely
/// unbuffered handle does is *refuse* a misaligned read: Windows fails it with
/// ERROR_INVALID_PARAMETER and Linux with EINVAL. So open a handle the same way
/// `open_unbuffered` does and ask it for one byte at offset 1. If that
/// succeeds, the page cache was never bypassed and the benchmark built on this
/// mode would be measuring RAM again.
///
/// The same reasoning as the smoke test reporting whether its bounce path was
/// actually exercised, rather than passing because an allocation happened to
/// land aligned.
#[cfg(any(windows, target_os = "linux"))]
#[test]
fn the_uncached_handle_really_rejects_a_misaligned_read() {
    let path = patterned_image("enforced", 1 << 20);
    let _c = Cleanup(path.clone());

    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        opts.custom_flags(0x2000_0000); // FILE_FLAG_NO_BUFFERING
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc_o_direct());
    }
    let file = opts.open(&path).expect("open uncached");

    let mut one = [0u8; 1];
    #[cfg(windows)]
    let result = {
        use std::os::windows::fs::FileExt;
        file.seek_read(&mut one, 1)
    };
    #[cfg(target_os = "linux")]
    let result = {
        use std::os::unix::fs::FileExt;
        file.read_at(&mut one, 1)
    };

    assert!(
        result.is_err(),
        "a one-byte read at offset 1 succeeded on a handle opened to bypass the \
         page cache, so the flag did not take effect and reads are still being \
         served from RAM"
    );

    // And the same handle must still serve an aligned read, or the refusal
    // above could be some unrelated failure rather than alignment enforcement.
    let mut aligned = rc_device::AlignedBuf::new(4096, 4096);
    #[cfg(windows)]
    let ok = {
        use std::os::windows::fs::FileExt;
        file.seek_read(&mut aligned[..4096], 0)
    };
    #[cfg(target_os = "linux")]
    let ok = {
        use std::os::unix::fs::FileExt;
        file.read_at(&mut aligned[..4096], 0)
    };
    assert_eq!(
        ok.expect("an aligned read on the same handle should succeed"),
        4096,
        "the aligned read returned a short count"
    );
}

#[cfg(target_os = "linux")]
fn libc_o_direct() -> i32 {
    // O_DIRECT is 0o40000 on x86-64 and aarch64 Linux.
    0o40000
}
