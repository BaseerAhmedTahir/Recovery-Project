//! Peak resident set size, for the one Milestone 3 criterion that is about
//! memory rather than correctness.
//!
//! SPEC.md section 5.8 requires peak RSS under ~2 GB regardless of drive
//! size. That is not a number you can assert by reading the code: whether an
//! index is genuinely disk-backed shows up only as the shape of a curve, so
//! the test grows the candidate count tenfold and checks that peak memory does
//! not follow.
//!
//! **Peak, not current.** Current RSS after a run says nothing - the peak may
//! have happened in the middle and been freed. Every platform here reports a
//! high-water mark the OS maintains, which is the number that matters and the
//! one that cannot be gamed by dropping a buffer before measuring.
//!
//! Returns `None` rather than guessing when the platform will not say.

/// Peak resident set size of this process in bytes, if the OS reports one.
pub fn peak_rss_bytes() -> Option<u64> {
    imp::peak_rss_bytes()
}

/// Human-readable, for test output.
pub fn format_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    pub fn peak_rss_bytes() -> Option<u64> {
        // SAFETY: the struct is fully initialised before the call except for
        // `cb`, which the API requires to be its own size, and the pointer is
        // to a local that outlives the call. GetCurrentProcess returns a
        // pseudo-handle that needs no closing.
        unsafe {
            let mut counters: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
            counters.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
            if GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) == 0 {
                return None;
            }
            Some(counters.PeakWorkingSetSize as u64)
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    /// `VmHWM` in `/proc/self/status` is the high-water mark, in kB.
    pub fn peak_rss_bytes() -> Option<u64> {
        let text = std::fs::read_to_string("/proc/self/status").ok()?;
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("VmHWM:") {
                let kb: u64 = rest.trim().split_whitespace().next()?.parse().ok()?;
                return Some(kb * 1024);
            }
        }
        None
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
mod imp {
    /// `ru_maxrss` is bytes on macOS and kilobytes on the BSDs, which is a
    /// long-standing wart. Only macOS is a target here, so treat it as bytes
    /// and say so rather than silently reporting a number a thousand times off
    /// somewhere else.
    pub fn peak_rss_bytes() -> Option<u64> {
        // SAFETY: getrusage fills a struct we own; the pointer is valid for
        // the duration of the call.
        unsafe {
            let mut usage: libc::rusage = std::mem::zeroed();
            if libc::getrusage(libc::RUSAGE_SELF, &mut usage) != 0 {
                return None;
            }
            let raw = usage.ru_maxrss as u64;
            if cfg!(target_os = "macos") {
                Some(raw)
            } else {
                Some(raw * 1024)
            }
        }
    }
}

#[cfg(not(any(windows, unix)))]
mod imp {
    pub fn peak_rss_bytes() -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_a_plausible_peak_on_a_supported_platform() {
        let Some(peak) = peak_rss_bytes() else {
            // Only a genuinely unsupported platform may reach here, and this
            // build targets Windows, Linux and macOS.
            panic!("no peak RSS on a platform that should report one");
        };
        // A running Rust test process is at least a megabyte and nowhere near
        // a terabyte. The point is to catch a unit mix-up - kB read as bytes
        // is the classic one - not to pin a number.
        assert!(
            peak > (1 << 20) && peak < (1 << 40),
            "implausible peak RSS {peak} ({}); check the units",
            format_bytes(peak)
        );
    }

    /// The high-water mark must not fall when memory is released, or the whole
    /// measurement is meaningless.
    #[test]
    fn the_peak_does_not_go_down() {
        let before = peak_rss_bytes().expect("peak rss");
        {
            let mut hog: Vec<u8> = Vec::with_capacity(64 << 20);
            hog.resize(64 << 20, 0xAB);
            // Touch it so the pages are genuinely resident.
            for i in (0..hog.len()).step_by(4096) {
                hog[i] = hog[i].wrapping_add(1);
            }
            std::hint::black_box(&hog);
        }
        let after = peak_rss_bytes().expect("peak rss");
        assert!(
            after >= before,
            "peak fell from {} to {} after freeing",
            format_bytes(before),
            format_bytes(after)
        );
    }

    #[test]
    fn formats_sizes_readably() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.0 KiB");
        assert_eq!(format_bytes(3 << 20), "3.0 MiB");
        assert_eq!(format_bytes(2 << 30), "2.0 GiB");
    }
}
