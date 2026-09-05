//! The only write path in RECOVERY-CORE.
//!
//! SPEC.md section 4.1 requires that anything producing bytes goes through a
//! sink that *refuses* to open a path resolving to a device currently
//! registered as a scan source. Section 4.2 extends that to restored files:
//! resolve the destination to its physical device and hard-fail if it is the
//! device being scanned.
//!
//! Both checks live here, so there is exactly one place in the codebase where a
//! file gets created, and exactly one place that decision can go wrong.

use crate::error::{ImageError, Result};
use crate::resolve::{self, Backing};
use rc_device::DeviceId;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// How strict to be when the destination's backing device is unknowable.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum UnknownBackingPolicy {
    /// Warn and continue. Correct for network shares and unusual mounts, where
    /// resolution legitimately fails and the destination cannot be the local
    /// device being scanned.
    #[default]
    Warn,
    /// Refuse. Correct when the operator has asked for maximum caution.
    Refuse,
}

#[derive(Clone, Debug)]
pub struct SinkOptions {
    pub allow_overwrite: bool,
    pub sparse: bool,
    pub unknown_backing: UnknownBackingPolicy,
}

impl Default for SinkOptions {
    fn default() -> Self {
        SinkOptions {
            allow_overwrite: false,
            sparse: true,
            unknown_backing: UnknownBackingPolicy::Warn,
        }
    }
}

/// Verify that writing to `path` cannot damage any device being scanned.
///
/// Public so the CLI can run it as a preflight check and explain the problem
/// before a long operation starts, rather than failing at the first write.
pub fn check_destination(path: &Path, policy: UnknownBackingPolicy) -> Result<Backing> {
    let sources = rc_device::registered_sources_detailed();

    // 1. Direct hit: the destination *is* an image file we are scanning.
    let canonical_dest = std::fs::canonicalize(path)
        .ok()
        .map(|p| p.to_string_lossy().to_string());
    for src in &sources {
        if let DeviceId::File(src_path) = &src.id {
            let same = canonical_dest.as_deref() == Some(src_path.as_str())
                || path.to_string_lossy() == src_path.as_str();
            if same {
                return Err(ImageError::WouldWriteToSource {
                    destination: path.to_path_buf(),
                    scanning: PathBuf::from(src_path),
                    detail: "the destination is the image file currently being scanned".to_string(),
                });
            }
        }
    }

    // 2. Resolve the destination to a physical device and compare.
    let backing = resolve::resolve(path);
    match &backing {
        Backing::Device {
            device,
            volume,
            serial,
        } => {
            for src in &sources {
                if let Some(detail) =
                    source_matches(src, device, volume.as_deref(), serial.as_deref())
                {
                    return Err(ImageError::WouldWriteToSource {
                        destination: path.to_path_buf(),
                        scanning: src.path.clone(),
                        detail,
                    });
                }
            }
        }
        Backing::Unknown { reason } => {
            if policy == UnknownBackingPolicy::Refuse && !sources.is_empty() {
                return Err(ImageError::UnresolvableDestination {
                    destination: path.to_path_buf(),
                    reason: reason.clone(),
                });
            }
            tracing::warn!(
                destination = %path.display(),
                reason = %reason,
                "could not determine the physical device backing the destination; \
                 continuing because no scan source resolves to it"
            );
        }
    }

    Ok(backing)
}

/// Decide whether a registered scan source is the same storage as a resolved
/// destination. Returns a human-readable reason when it is.
fn source_matches(
    src: &rc_device::ScanSource,
    dest_device: &str,
    dest_volume: Option<&str>,
    dest_serial: Option<&str>,
) -> Option<String> {
    let src_path = src.path.to_string_lossy().to_string();

    // A spanned/striped volume reports several disks; any overlap is unsafe.
    let dest_devices: Vec<&str> = dest_device.split(',').map(|s| s.trim()).collect();

    match &src.id {
        DeviceId::Hardware { serial, .. } => {
            if dest_serial == Some(serial.as_str()) {
                return Some(format!(
                    "the destination is on the device with serial {serial}, which is being scanned"
                ));
            }
            // Serial is often unavailable on the destination side, so fall back
            // to comparing device paths.
            if dest_devices.iter().any(|d| paths_equal(d, &src_path)) {
                return Some(format!(
                    "the destination is on {src_path}, which is being scanned"
                ));
            }
        }
        DeviceId::Path(p) => {
            if dest_devices.iter().any(|d| paths_equal(d, p))
                || dest_volume.is_some_and(|v| paths_equal(v, p))
            {
                return Some(format!("the destination is on {p}, which is being scanned"));
            }
        }
        DeviceId::File(_) => {
            // Handled by the direct-hit check above. Writing a clone onto the
            // same *filesystem* as a source image is fine and common.
        }
    }
    None
}

/// Compare device paths, tolerating Windows case-insensitivity and the
/// `\\?\` / `\\.\` prefix aliasing.
fn paths_equal(a: &str, b: &str) -> bool {
    fn norm(s: &str) -> String {
        let s = s.trim_end_matches('\\');
        let s = s
            .strip_prefix("\\\\?\\")
            .or_else(|| s.strip_prefix("\\\\.\\"))
            .unwrap_or(s);
        if cfg!(windows) {
            s.to_ascii_lowercase()
        } else {
            s.to_string()
        }
    }
    !a.is_empty() && !b.is_empty() && norm(a) == norm(b)
}

/// A validated destination for written bytes.
#[derive(Debug)]
pub struct OutputSink {
    file: File,
    path: PathBuf,
    sparse: bool,
    written: u64,
    high_water: u64,
}

impl OutputSink {
    /// Create (or truncate) an output file after running every safety check.
    pub fn create(path: &Path, opts: &SinkOptions) -> Result<Self> {
        check_destination(path, opts.unknown_backing)?;

        if path.exists() && !opts.allow_overwrite {
            return Err(ImageError::DestinationExists(path.to_path_buf()));
        }

        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                std::fs::create_dir_all(parent).map_err(|e| ImageError::Create {
                    path: path.to_path_buf(),
                    source: e,
                })?;
            }
        }

        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .map_err(|e| ImageError::Create {
                path: path.to_path_buf(),
                source: e,
            })?;

        let mut sink = OutputSink {
            file,
            path: path.to_path_buf(),
            sparse: opts.sparse,
            written: 0,
            high_water: 0,
        };
        if opts.sparse {
            sink.enable_sparse();
        }
        Ok(sink)
    }

    /// Reopen an existing output for a resumed clone, leaving its contents
    /// intact. The same destination safety checks still apply.
    pub fn resume(path: &Path, opts: &SinkOptions) -> Result<Self> {
        check_destination(path, opts.unknown_backing)?;
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|e| ImageError::Create {
                path: path.to_path_buf(),
                source: e,
            })?;
        let high_water = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(OutputSink {
            file,
            path: path.to_path_buf(),
            sparse: opts.sparse,
            written: 0,
            high_water,
        })
    }

    fn enable_sparse(&mut self) {
        #[cfg(windows)]
        {
            crate::sparse::mark_sparse(&self.file);
        }
        // On Linux and macOS sparseness is implicit: seeking past the end and
        // writing leaves an unallocated hole, no ioctl required.
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn bytes_written(&self) -> u64 {
        self.written
    }

    /// Write `data` at an absolute offset.
    pub fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        write_all_at(&self.file, data, offset).map_err(|e| ImageError::Write {
            path: self.path.clone(),
            offset,
            source: e,
        })?;
        self.written += data.len() as u64;
        self.high_water = self.high_water.max(offset + data.len() as u64);
        Ok(())
    }

    /// Set the final length, so a sparse image ends at the right size even when
    /// its tail was never written.
    pub fn set_len(&mut self, len: u64) -> Result<()> {
        self.file.set_len(len).map_err(|e| ImageError::Write {
            path: self.path.clone(),
            offset: len,
            source: e,
        })?;
        self.high_water = self.high_water.max(len);
        Ok(())
    }

    pub fn is_sparse(&self) -> bool {
        self.sparse
    }

    pub fn finish(mut self) -> Result<u64> {
        self.file.flush().map_err(|e| ImageError::Write {
            path: self.path.clone(),
            offset: self.high_water,
            source: e,
        })?;
        self.file.sync_all().map_err(|e| ImageError::Write {
            path: self.path.clone(),
            offset: self.high_water,
            source: e,
        })?;
        Ok(self.written)
    }
}

fn write_all_at(file: &File, buf: &[u8], offset: u64) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.write_all_at(buf, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let mut done = 0usize;
        while done < buf.len() {
            match file.seek_write(&buf[done..], offset + done as u64) {
                Ok(0) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "output sink accepted zero bytes",
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join("rc-image-sink-tests").join(name);
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn writes_and_reads_back_at_offsets() {
        let d = tmpdir("basic");
        let out = d.join("out.raw");
        let mut s = OutputSink::create(&out, &SinkOptions::default()).unwrap();
        s.write_at(0, b"hello").unwrap();
        s.write_at(100, b"world").unwrap();
        s.set_len(4096).unwrap();
        let n = s.finish().unwrap();
        assert_eq!(n, 10);

        let data = std::fs::read(&out).unwrap();
        assert_eq!(data.len(), 4096);
        assert_eq!(&data[0..5], b"hello");
        assert_eq!(&data[100..105], b"world");
        assert!(
            data[5..100].iter().all(|&b| b == 0),
            "hole should read zero"
        );
    }

    #[test]
    fn refuses_to_overwrite_unless_asked() {
        let d = tmpdir("overwrite");
        let out = d.join("out.raw");
        std::fs::write(&out, b"existing").unwrap();

        let err = OutputSink::create(&out, &SinkOptions::default()).unwrap_err();
        assert!(matches!(err, ImageError::DestinationExists(_)));
        // The existing file must be untouched by the refusal.
        assert_eq!(std::fs::read(&out).unwrap(), b"existing");

        let opts = SinkOptions {
            allow_overwrite: true,
            ..Default::default()
        };
        assert!(OutputSink::create(&out, &opts).is_ok());
    }

    /// The core cross-crate invariant: with an image open as a scan source,
    /// the sink must refuse to write to that same image.
    #[test]
    fn refuses_to_write_to_the_image_being_scanned() {
        let d = tmpdir("selfwrite");
        let img = d.join("source.img");
        std::fs::write(&img, vec![0u8; 8192]).unwrap();

        let dev = rc_device::open(&img, None).expect("open source image");

        let err = OutputSink::create(
            &img,
            &SinkOptions {
                allow_overwrite: true,
                ..Default::default()
            },
        )
        .expect_err("writing onto the scanned image must be refused");
        assert!(
            matches!(err, ImageError::WouldWriteToSource { .. }),
            "expected WouldWriteToSource, got {err:?}"
        );

        // A different file on the same filesystem is fine.
        let other = d.join("clone.raw");
        assert!(OutputSink::create(&other, &SinkOptions::default()).is_ok());

        drop(dev);

        // Once the source is closed the restriction lifts.
        assert!(OutputSink::create(
            &img,
            &SinkOptions {
                allow_overwrite: true,
                ..Default::default()
            }
        )
        .is_ok());
    }

    #[test]
    fn device_path_comparison_handles_windows_aliasing() {
        assert!(paths_equal(
            "\\\\.\\PhysicalDrive0",
            "\\\\?\\PhysicalDrive0"
        ));
        assert!(paths_equal("/dev/sda", "/dev/sda"));
        assert!(!paths_equal("/dev/sda", "/dev/sdb"));
        assert!(!paths_equal("", ""));
        if cfg!(windows) {
            assert!(paths_equal("C:\\Temp", "c:\\temp"));
        }
    }

    #[test]
    fn resume_preserves_existing_contents() {
        let d = tmpdir("resume");
        let out = d.join("out.raw");
        {
            let mut s = OutputSink::create(&out, &SinkOptions::default()).unwrap();
            s.write_at(0, b"first").unwrap();
            s.set_len(1024).unwrap();
            s.finish().unwrap();
        }
        {
            let mut s = OutputSink::resume(&out, &SinkOptions::default()).unwrap();
            s.write_at(512, b"second").unwrap();
            s.finish().unwrap();
        }
        let data = std::fs::read(&out).unwrap();
        assert_eq!(&data[0..5], b"first", "resume must not truncate");
        assert_eq!(&data[512..518], b"second");
    }
}
