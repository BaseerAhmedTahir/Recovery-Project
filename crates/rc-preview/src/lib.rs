//! `rc-preview` - previews without temp files (SPEC.md 5.9).
//!
//! A preview runs on bytes recovered from a disk that is being recovered, so
//! nothing here writes a file anywhere: images are decoded in memory to a
//! bounded thumbnail, video goes to ffmpeg over pipes (bytes in on stdin, a PNG
//! frame out on stdout, no path arguments), and the hex view is read straight
//! off the device.
//!
//! # ffmpeg's sandbox
//!
//! ffmpeg parses hostile input, so it runs with a hard timeout and, on Windows,
//! inside a Job Object that caps its memory, kills it when the job handle
//! closes, and forbids it starting other processes. On Unix the address space
//! is capped with setrlimit before exec. Filesystem access is **not** denied by
//! the operating system on either - that needs an AppContainer on Windows or
//! namespaces on Linux - and docs/LIMITATIONS.md says so. What keeps ffmpeg
//! from touching files is that it is given no file to touch.
//!
//! # MP4s whose index comes last
//!
//! ffmpeg cannot read such a file from a pipe: it needs to seek to the end for
//! the moov box first. Recorders write exactly that layout. So before piping,
//! an MP4 with moov after mdat is rearranged in memory - moov moved in front and
//! every chunk offset shifted by its size - which is what `-movflags faststart`
//! does on disk.

use image::{GenericImageView, ImageFormat, ImageReader, Limits};
use rc_device::ReadOnlyDevice;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, thiserror::Error)]
pub enum PreviewError {
    #[error("not an image this can decode: {0}")]
    Decode(String),
    #[error("ffmpeg: {0}")]
    Ffmpeg(String),
    #[error("ffmpeg did not finish within {0:?} and was killed")]
    Timeout(Duration),
    #[error("reading the device: {0}")]
    Device(#[from] rc_device::DeviceError),
}

pub type Result<T> = std::result::Result<T, PreviewError>;

/// A decoded, bounded thumbnail, encoded as PNG in memory.
#[derive(Clone, Debug)]
pub struct Thumbnail {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// The original image's dimensions.
    pub source_width: u32,
    pub source_height: u32,
    pub source_format: String,
}

/// The most memory one decode may allocate, and the largest dimensions it may
/// claim. A corrupt header declaring a 60000x60000 image must not allocate
/// 13 GiB before anything notices.
const MAX_ALLOC: u64 = 512 << 20;
const MAX_DIMENSION: u32 = 16384;

/// Decode an image entirely in memory and shrink it to fit `max_dim`.
pub fn thumbnail(bytes: &[u8], max_dim: u32) -> Result<Thumbnail> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| PreviewError::Decode(e.to_string()))?;
    let format = reader
        .format()
        .ok_or_else(|| PreviewError::Decode("unrecognised image format".into()))?;
    let mut limits = Limits::default();
    limits.max_alloc = Some(MAX_ALLOC);
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    reader.limits(limits);
    let img = reader
        .decode()
        .map_err(|e| PreviewError::Decode(e.to_string()))?;
    let (sw, sh) = img.dimensions();
    let thumb = img.thumbnail(max_dim.max(1), max_dim.max(1));
    let mut png = Vec::new();
    thumb
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
        .map_err(|e| PreviewError::Decode(e.to_string()))?;
    Ok(Thumbnail {
        width: thumb.width(),
        height: thumb.height(),
        png,
        source_width: sw,
        source_height: sh,
        source_format: format!("{format:?}").to_lowercase(),
    })
}

#[derive(Clone, Debug)]
pub struct FfmpegOptions {
    pub exe: PathBuf,
    pub timeout: Duration,
    /// Memory cap for the ffmpeg process, in bytes.
    pub memory_limit: u64,
    pub max_dim: u32,
    /// Seconds into the stream to take the frame from.
    pub at_seconds: f64,
    /// Working directory for ffmpeg. `None` inherits this process's. Given only
    /// so a test can hand it an empty directory and check it stays empty.
    pub working_dir: Option<PathBuf>,
}

impl FfmpegOptions {
    /// ffmpeg from PATH, or `None` when it is not installed.
    pub fn find() -> Option<FfmpegOptions> {
        let exe = which("ffmpeg")?;
        Some(FfmpegOptions {
            exe,
            timeout: Duration::from_secs(20),
            memory_limit: 1 << 30,
            max_dim: 320,
            at_seconds: 0.0,
            working_dir: None,
        })
    }
}

fn which(name: &str) -> Option<PathBuf> {
    let exts: &[&str] = if cfg!(windows) {
        &[".exe", ".cmd", ""]
    } else {
        &[""]
    };
    // Beside this executable first: that is where a packaged copy is dropped,
    // and it is found even when PATH has none (ffmpeg is not bundled - see
    // packaging/README.md).
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
    {
        for e in exts {
            let p = dir.join(format!("{name}{e}"));
            if p.is_file() {
                return Some(p);
            }
        }
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            exts.iter()
                .map(|e| dir.join(format!("{name}{e}")))
                .find(|p| p.is_file())
        })
    })
}

/// One video frame as PNG, through ffmpeg over pipes.
pub fn video_frame(bytes: &[u8], opts: &FfmpegOptions) -> Result<Vec<u8>> {
    let input: std::borrow::Cow<[u8]> = match faststart(bytes) {
        Some(v) => std::borrow::Cow::Owned(v),
        None => std::borrow::Cow::Borrowed(bytes),
    };
    let scale = format!(
        "scale={m}:{m}:force_original_aspect_ratio=decrease",
        m = opts.max_dim.max(16)
    );
    let mut cmd = Command::new(&opts.exe);
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"])
        .args(["-ss", &format!("{}", opts.at_seconds.max(0.0))])
        .args(["-i", "pipe:0", "-frames:v", "1", "-vf", &scale])
        .args(["-f", "image2pipe", "-vcodec", "png", "pipe:1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = &opts.working_dir {
        cmd.current_dir(dir);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let cap = opts.memory_limit;
        // SAFETY: setrlimit is async-signal-safe and touches no shared state.
        unsafe {
            cmd.pre_exec(move || {
                let lim = libc::rlimit {
                    rlim_cur: cap as libc::rlim_t,
                    rlim_max: cap as libc::rlim_t,
                };
                libc::setrlimit(libc::RLIMIT_AS, &lim);
                Ok(())
            });
        }
    }

    let mut child = cmd.spawn().map_err(|e| {
        PreviewError::Ffmpeg(format!("could not start {}: {e}", opts.exe.display()))
    })?;
    #[cfg(windows)]
    let _job = sandbox::confine(&child, opts.memory_limit);

    let mut stdin = child.stdin.take().expect("piped");
    let data = input.into_owned();
    let writer = std::thread::spawn(move || {
        // ffmpeg may stop reading once it has its frame; a broken pipe then is
        // expected, not an error.
        let _ = stdin.write_all(&data);
    });
    let mut stdout = child.stdout.take().expect("piped");
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = stdout.read_to_end(&mut out);
        out
    });
    let mut stderr = child.stderr.take().expect("piped");
    let err_reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });

    let deadline = Instant::now() + opts.timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(PreviewError::Timeout(opts.timeout));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => return Err(PreviewError::Ffmpeg(e.to_string())),
        }
    };
    let _ = writer.join();
    let png = reader.join().unwrap_or_default();
    let err = err_reader.join().unwrap_or_default();
    if !status.success() || png.is_empty() {
        return Err(PreviewError::Ffmpeg(format!(
            "exit {:?}: {}",
            status.code(),
            err.trim()
        )));
    }
    Ok(png)
}

#[cfg(windows)]
mod sandbox {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOB_OBJECT_LIMIT_PROCESS_MEMORY,
    };

    /// Closing the job kills whatever is still in it.
    pub struct Job(HANDLE);

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: the handle came from CreateJobObjectW and is closed once.
            unsafe { CloseHandle(self.0) };
        }
    }

    /// Put `child` in a job with a memory cap, one active process, and
    /// kill-on-close. Best effort: a failure leaves the timeout as the guard.
    pub fn confine(child: &std::process::Child, memory: u64) -> Option<Job> {
        // SAFETY: plain Win32 calls with owned, valid handles and a zeroed,
        // correctly sized limit structure.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return None;
            }
            let job = Job(job);
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_PROCESS_MEMORY
                | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
                | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
            info.BasicLimitInformation.ActiveProcessLimit = 1;
            info.ProcessMemoryLimit = memory as usize;
            let ok = SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if ok == 0 {
                return None;
            }
            if AssignProcessToJobObject(job.0, child.as_raw_handle() as HANDLE) == 0 {
                return None;
            }
            Some(job)
        }
    }
}

/// An MP4 with moov after mdat, rearranged so moov comes first; `None` when
/// the layout is already streamable or the box tree does not parse.
pub fn faststart(d: &[u8]) -> Option<Vec<u8>> {
    let boxes = top_boxes(d)?;
    let moov = boxes.iter().position(|b| &b.2 == b"moov")?;
    let mdat = boxes.iter().position(|b| &b.2 == b"mdat")?;
    if moov < mdat {
        return None;
    }
    let (moov_at, moov_len, _) = boxes[moov];
    let (mdat_at, _, _) = boxes[mdat];
    let mut moov_box = d[moov_at..moov_at + moov_len].to_vec();
    shift_chunk_offsets(&mut moov_box, moov_len as u64)?;
    let mut out = Vec::with_capacity(d.len());
    out.extend_from_slice(&d[..mdat_at]);
    out.extend_from_slice(&moov_box);
    out.extend_from_slice(&d[mdat_at..moov_at]);
    out.extend_from_slice(&d[moov_at + moov_len..]);
    Some(out)
}

fn be32(d: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(d.get(at..at + 4)?.try_into().ok()?))
}

fn be64(d: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(d.get(at..at + 8)?.try_into().ok()?))
}

/// (offset, length, type) of each top-level box.
fn top_boxes(d: &[u8]) -> Option<Vec<(usize, usize, [u8; 4])>> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at + 8 <= d.len() {
        let size32 = be32(d, at)? as u64;
        let ty: [u8; 4] = d[at + 4..at + 8].try_into().ok()?;
        let size = match size32 {
            0 => (d.len() - at) as u64,
            1 => be64(d, at + 8)?,
            s => s,
        };
        if size < 8 || at as u64 + size > d.len() as u64 {
            return None;
        }
        out.push((at, size as usize, ty));
        at += size as usize;
    }
    Some(out)
}

/// Add `by` to every stco and co64 entry inside a moov box.
fn shift_chunk_offsets(moov: &mut [u8], by: u64) -> Option<()> {
    const CONTAINERS: [&[u8; 4]; 6] = [b"moov", b"trak", b"mdia", b"minf", b"stbl", b"edts"];
    fn walk(buf: &mut [u8], start: usize, end: usize, by: u64) -> Option<()> {
        let mut at = start;
        while at + 8 <= end {
            let size = be32(buf, at)? as usize;
            if size < 8 || at + size > end {
                return None;
            }
            let ty: [u8; 4] = buf[at + 4..at + 8].try_into().ok()?;
            if CONTAINERS.contains(&&ty) {
                walk(buf, at + 8, at + size, by)?;
            } else if &ty == b"stco" || &ty == b"co64" {
                let n = be32(buf, at + 12)? as usize;
                let wide = &ty == b"co64";
                let step = if wide { 8 } else { 4 };
                for k in 0..n {
                    let p = at + 16 + k * step;
                    if wide {
                        let v = be64(buf, p)?.checked_add(by)?;
                        buf.get_mut(p..p + 8)?.copy_from_slice(&v.to_be_bytes());
                    } else {
                        let v = u32::try_from(be32(buf, p)? as u64 + by).ok()?;
                        buf.get_mut(p..p + 4)?.copy_from_slice(&v.to_be_bytes());
                    }
                }
            }
            at += size;
        }
        Some(())
    }
    let len = moov.len();
    walk(moov, 8, len, by)
}

// ---------------------------------------------------------------------------
// hex view
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SpanKind {
    Header,
    Footer,
    FragmentBoundary,
    Damage,
    Partition,
    Other,
}

/// An annotated byte range, absolute device offsets, end exclusive.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Span {
    pub start: u64,
    pub end: u64,
    pub kind: SpanKind,
    pub label: String,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct HexView {
    pub offset: u64,
    pub bytes: Vec<u8>,
    /// The spans that overlap this range, clipped to it.
    pub spans: Vec<Span>,
}

/// Up to this many bytes per request; the GUI pages.
pub const MAX_HEX_BYTES: u64 = 1 << 20;

pub fn hex_view(
    dev: &dyn ReadOnlyDevice,
    offset: u64,
    len: u64,
    spans: &[Span],
) -> Result<HexView> {
    let len = len
        .min(MAX_HEX_BYTES)
        .min(dev.total_bytes().saturating_sub(offset));
    let mut bytes = vec![0u8; len as usize];
    let n = dev.read_bytes_at(offset, &mut bytes)?;
    bytes.truncate(n);
    let end = offset + n as u64;
    let spans = spans
        .iter()
        .filter(|s| s.start < end && s.end > offset)
        .map(|s| Span {
            start: s.start.max(offset),
            end: s.end.min(end),
            ..s.clone()
        })
        .collect();
    Ok(HexView {
        offset,
        bytes,
        spans,
    })
}

/// Spans for a carved candidate: its header, its end, where a validator placed
/// damage, and the boundaries of any reassembled pieces.
pub fn candidate_spans(
    offset: u64,
    length: u64,
    header_len: u64,
    evidence: &[(String, String)],
    pieces: &[(u64, u64)],
) -> Vec<Span> {
    let mut out = vec![Span {
        start: offset,
        end: offset + header_len.max(1),
        kind: SpanKind::Header,
        label: "header".into(),
    }];
    if length > 0 {
        out.push(Span {
            start: offset + length - 1,
            end: offset + length,
            kind: SpanKind::Footer,
            label: "last byte the structure vouches for".into(),
        });
    }
    for (k, v) in evidence {
        if k == "damage_at" || k == "spliced_after" {
            if let Ok(at) = v.parse::<u64>() {
                out.push(Span {
                    start: offset + at,
                    end: offset + at + 1,
                    kind: SpanKind::Damage,
                    label: k.clone(),
                });
            }
        }
    }
    for (i, (o, l)) in pieces.iter().enumerate().skip(1) {
        out.push(Span {
            start: *o,
            end: *o + 1,
            kind: SpanKind::FragmentBoundary,
            label: format!("fragment {} begins ({} bytes)", i + 1, l),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bx(ty: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(ty);
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn faststart_moves_moov_and_shifts_offsets() {
        let ftyp = bx(b"ftyp", b"isom\0\0\0\0isom");
        let mdat = bx(b"mdat", &[7u8; 32]);
        let first_sample = (ftyp.len() + 8) as u32;
        let mut stco_body = vec![0, 0, 0, 0, 0, 0, 0, 1];
        stco_body.extend_from_slice(&first_sample.to_be_bytes());
        let stco = bx(b"stco", &stco_body);
        let moov = bx(
            b"moov",
            &bx(b"trak", &bx(b"mdia", &bx(b"minf", &bx(b"stbl", &stco)))),
        );
        let file = [ftyp.clone(), mdat.clone(), moov.clone()].concat();
        let fixed = faststart(&file).expect("rearranged");
        assert_eq!(fixed.len(), file.len());
        assert_eq!(&fixed[ftyp.len() + 4..ftyp.len() + 8], b"moov");
        let entry_at = fixed.windows(4).position(|w| w == b"stco").unwrap() + 12;
        let entry = u32::from_be_bytes(fixed[entry_at..entry_at + 4].try_into().unwrap());
        assert_eq!(entry, first_sample + moov.len() as u32);
        // The shifted offset points at the same sample bytes.
        assert_eq!(fixed[entry as usize], 7);
        assert!(faststart(&fixed).is_none(), "already streamable");
    }

    #[test]
    fn spans_are_clipped_to_the_view() {
        let s = [Span {
            start: 10,
            end: 100,
            kind: SpanKind::Header,
            label: "h".into(),
        }];
        let overlap: Vec<_> = s
            .iter()
            .filter(|x| x.start < 60 && x.end > 50)
            .map(|x| (x.start.max(50), x.end.min(60)))
            .collect();
        assert_eq!(overlap, vec![(50, 60)]);
        let c = candidate_spans(
            1000,
            50,
            3,
            &[("damage_at".into(), "20".into())],
            &[(1000, 30), (5000, 20)],
        );
        assert!(c
            .iter()
            .any(|x| x.kind == SpanKind::Damage && x.start == 1020));
        assert!(c
            .iter()
            .any(|x| x.kind == SpanKind::FragmentBoundary && x.start == 5000));
    }
}
