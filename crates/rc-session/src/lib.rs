//! `rc-session` - carving that survives being killed (SPEC.md 5.8).
//!
//! A carve of a large drive runs for hours, and a recovery tool is exactly the
//! kind of program that gets interrupted: the machine sleeps, the user presses
//! Ctrl+C, the process is killed. This makes a carve resumable, and resumable to
//! **identical results** - the same candidates an uninterrupted run would have
//! produced, not an approximation.
//!
//! # How
//!
//! The device range is scanned in segments ([`rc_carve::scan::scan_segment`]),
//! which produce exactly what one scan would wherever the boundaries fall; a test
//! in rc-carve proves that on a real fixture. After each segment, its candidates
//! and a checkpoint describing everything committed so far go into the candidate
//! index **in one SQLite transaction** ([`rc_index::CandidateIndex::commit_segment`]).
//! A kill at any instant therefore leaves an index whose rows and checkpoint
//! agree: either the segment and the checkpoint that counts it are both there,
//! or neither is.
//!
//! The `.recstate` file beside the index carries the same checkpoint for people
//! and tools to read, written atomically (temporary file, then rename). It can
//! lag the index by one segment if the kill lands between the two writes, so on
//! resume the index's copy is the authority and the file is only the pointer to
//! it.
//!
//! Segments are sized to take about `checkpoint_every` each, so no more than
//! that much work is ever lost, and a stop request is honoured at the next
//! segment boundary.
//!
//! # Refusing the wrong disk
//!
//! Resuming a checkpoint against a different device would silently merge two
//! drives' results. The checkpoint records the device's size, sector size,
//! serial where it has one, and a fingerprint of its first and last MiB, plus a
//! hash of the signature database and scan options; resume refuses on any
//! mismatch. SPEC.md asks for "processed-cluster bitmap" as well: because
//! segments commit strictly in order, everything below `next_offset` is
//! processed and nothing above is, so a single offset *is* that bitmap.

use rc_carve::scan::{scan_segment, Carry, ScanOptions, ScanStats};
use rc_carve::SignatureDb;
use rc_device::ReadOnlyDevice;
use rc_index::CandidateIndex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const RECSTATE_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("{0}")]
    Carve(#[from] rc_carve::CarveError),
    #[error("{0}")]
    Index(#[from] rc_index::IndexError),
    #[error("reading the device: {0}")]
    Device(#[from] rc_device::DeviceError),
    #[error("{path}: {detail}")]
    State { path: String, detail: String },
    #[error("refusing to resume: {0}")]
    Mismatch(String),
}

pub type Result<T> = std::result::Result<T, SessionError>;

/// What identifies the device a checkpoint belongs to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceIdentity {
    pub path: String,
    pub total_bytes: u64,
    pub sector_size: u32,
    pub serial: Option<String>,
    /// SHA-256 of the first and last MiB. A read-only carve never changes
    /// them, and two different disks of one size almost never share them.
    pub fingerprint: String,
}

impl DeviceIdentity {
    pub fn of(dev: &dyn ReadOnlyDevice, path: &Path) -> Result<DeviceIdentity> {
        let total = dev.total_bytes();
        let span = (1u64 << 20).min(total);
        let mut h = Sha256::new();
        let mut buf = vec![0u8; span as usize];
        let n = dev.read_bytes_at(0, &mut buf)?;
        h.update(&buf[..n]);
        let n = dev.read_bytes_at(total - span, &mut buf)?;
        h.update(&buf[..n]);
        Ok(DeviceIdentity {
            path: path.display().to_string(),
            total_bytes: total,
            sector_size: dev.sector_size().get(),
            serial: dev.serial().map(str::to_string),
            fingerprint: hex::encode(h.finalize()),
        })
    }

    /// Why `other` is not this device, if it is not. The path is not compared:
    /// the same disk can be reached by several names, and an image can move.
    pub fn mismatch(&self, other: &DeviceIdentity) -> Option<String> {
        if self.total_bytes != other.total_bytes {
            return Some(format!(
                "the checkpoint is for a {}-byte device and this one is {} bytes",
                self.total_bytes, other.total_bytes
            ));
        }
        if self.sector_size != other.sector_size {
            return Some("the sector size differs".into());
        }
        if let (Some(a), Some(b)) = (&self.serial, &other.serial) {
            if a != b {
                return Some(format!("serial {a} was checkpointed; this device is {b}"));
            }
        }
        if self.fingerprint != other.fingerprint {
            return Some("the first and last MiB differ from the checkpointed device's".into());
        }
        None
    }
}

/// Everything a resume needs, committed with every segment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecState {
    pub version: u32,
    pub device: DeviceIdentity,
    pub range_start: u64,
    pub range_end: u64,
    /// Hash of the signature database and every option that changes results.
    pub config_hash: String,
    pub index: PathBuf,
    /// Everything below this offset has been scanned and committed.
    pub next_offset: u64,
    pub candidates_committed: u64,
    pub carry_cover_end: u64,
    pub carry_candidates: u64,
    pub complete: bool,
    pub updated_unix: u64,
}

impl RecState {
    pub fn load(path: &Path) -> Result<RecState> {
        let text = std::fs::read_to_string(path).map_err(|e| SessionError::State {
            path: path.display().to_string(),
            detail: e.to_string(),
        })?;
        serde_json::from_str(&text).map_err(|e| SessionError::State {
            path: path.display().to_string(),
            detail: format!("not a checkpoint: {e}"),
        })
    }

    /// Write atomically: a temporary file, then a rename over the old one.
    pub fn save(&self, path: &Path) -> Result<()> {
        let tmp = path.with_extension("recstate.tmp");
        let err = |e: std::io::Error| SessionError::State {
            path: path.display().to_string(),
            detail: e.to_string(),
        };
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).expect("serialises")).map_err(err)?;
        std::fs::rename(&tmp, path).map_err(err)
    }
}

/// Where the checkpoint file for an index lives.
pub fn recstate_path(index: &Path) -> PathBuf {
    let mut name = index.as_os_str().to_owned();
    name.push(".recstate");
    PathBuf::from(name)
}

/// Hash of what decides the results: the signature database and scan options.
pub fn config_hash(signatures_json: &str, opts: &ScanOptions) -> String {
    let mut h = Sha256::new();
    h.update(signatures_json.as_bytes());
    h.update(
        format!(
            "block={} validate={} window={} only={:?} suppress={} max={}",
            opts.block_bytes,
            opts.validate,
            opts.validation_window,
            opts.only,
            opts.suppress_contained,
            opts.max_candidates
        )
        .as_bytes(),
    );
    hex::encode(h.finalize())
}

pub struct Settings {
    pub opts: ScanOptions,
    /// The signature database's JSON, for the config hash.
    pub signatures_json: String,
    /// Target time between checkpoints.
    pub checkpoint_every: Duration,
    /// First segment's size; later ones adapt to `checkpoint_every`.
    pub first_segment: u64,
    /// Cap on read rate, in bytes per second. Gentle on a failing drive, and
    /// how the kill test makes a 512 MiB carve last long enough to kill.
    pub max_rate: Option<u64>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            opts: ScanOptions::default(),
            signatures_json: rc_carve::signature::BUILTIN_JSON.to_string(),
            checkpoint_every: Duration::from_secs(5),
            first_segment: 64 << 20,
            max_rate: None,
        }
    }
}

/// Counters summed over every segment of this run (not the earlier runs a
/// resume continues).
#[derive(Clone, Debug, Default)]
pub struct Totals {
    pub stats: ScanStats,
    pub segments: u64,
    pub resumed_from: Option<u64>,
}

#[derive(Debug)]
pub enum Outcome {
    Complete { candidates: u64 },
    Stopped { next_offset: u64, recstate: PathBuf },
}

/// Progress, reported after every committed segment.
#[derive(Clone, Copy, Debug)]
pub struct Progress {
    pub next_offset: u64,
    pub range_start: u64,
    pub range_end: u64,
    pub candidates: u64,
}

/// Start a fresh carve.
#[allow(clippy::too_many_arguments)]
pub fn start(
    device: Arc<dyn ReadOnlyDevice>,
    device_path: &Path,
    db: &SignatureDb,
    settings: &Settings,
    range: (u64, u64),
    index_path: &Path,
    stop: &AtomicBool,
    progress: &mut dyn FnMut(Progress),
) -> Result<(Outcome, Totals)> {
    let identity = DeviceIdentity::of(device.as_ref(), device_path)?;
    let state = RecState {
        version: RECSTATE_VERSION,
        device: identity,
        range_start: range.0,
        range_end: range.1,
        config_hash: config_hash(&settings.signatures_json, &settings.opts),
        index: index_path.to_path_buf(),
        next_offset: range.0,
        candidates_committed: 0,
        carry_cover_end: 0,
        carry_candidates: 0,
        complete: false,
        updated_unix: now(),
    };
    let mut index = CandidateIndex::create(index_path)?;
    index.commit_segment(&[], &serde_json::to_string(&state).expect("serialises"))?;
    state.save(&recstate_path(index_path))?;
    run(device, db, settings, state, index, stop, progress, None)
}

/// Continue a carve from its checkpoint, after checking it is the same device
/// and the same configuration.
pub fn resume(
    device: Arc<dyn ReadOnlyDevice>,
    device_path: &Path,
    db: &SignatureDb,
    settings: &Settings,
    recstate: &Path,
    stop: &AtomicBool,
    progress: &mut dyn FnMut(Progress),
) -> Result<(Outcome, Totals)> {
    let file_state = RecState::load(recstate)?;
    let index = CandidateIndex::open(&file_state.index)?;
    // The index's checkpoint was committed with the rows; it is the authority.
    let state: RecState = match index.checkpoint()? {
        Some(json) => serde_json::from_str(&json).map_err(|e| SessionError::State {
            path: file_state.index.display().to_string(),
            detail: format!("the index's checkpoint does not parse: {e}"),
        })?,
        None => {
            return Err(SessionError::State {
                path: file_state.index.display().to_string(),
                detail: "the index holds no checkpoint".into(),
            })
        }
    };
    if state.version != RECSTATE_VERSION {
        return Err(SessionError::Mismatch(format!(
            "checkpoint version {} is not {RECSTATE_VERSION}",
            state.version
        )));
    }
    let here = DeviceIdentity::of(device.as_ref(), device_path)?;
    if let Some(why) = state.device.mismatch(&here) {
        return Err(SessionError::Mismatch(why));
    }
    let hash = config_hash(&settings.signatures_json, &settings.opts);
    if state.config_hash != hash {
        return Err(SessionError::Mismatch(
            "the signature database or scan options differ from the checkpointed carve's; \
             resuming would mix two configurations in one index"
                .into(),
        ));
    }
    let rows = index.count()?;
    if rows != state.candidates_committed {
        return Err(SessionError::State {
            path: state.index.display().to_string(),
            detail: format!(
                "the index holds {rows} candidates but its checkpoint accounts for {}",
                state.candidates_committed
            ),
        });
    }
    if state.complete {
        let n = state.candidates_committed;
        return Ok((Outcome::Complete { candidates: n }, Totals::default()));
    }
    let from = state.next_offset;
    run(
        device,
        db,
        settings,
        state,
        index,
        stop,
        progress,
        Some(from),
    )
}

#[allow(clippy::too_many_arguments)]
fn run(
    device: Arc<dyn ReadOnlyDevice>,
    db: &SignatureDb,
    settings: &Settings,
    mut state: RecState,
    mut index: CandidateIndex,
    stop: &AtomicBool,
    progress: &mut dyn FnMut(Progress),
    resumed_from: Option<u64>,
) -> Result<(Outcome, Totals)> {
    let recstate = recstate_path(&state.index);
    let mut totals = Totals {
        resumed_from,
        ..Default::default()
    };
    let mut carry = Carry {
        cover_end: state.carry_cover_end,
        candidates: state.carry_candidates,
    };
    let block = settings.opts.block_bytes.max(1) as u64;
    let mut segment = settings.first_segment.max(block);
    let end = state.range_end;

    while state.next_offset < end {
        if stop.load(Ordering::SeqCst) {
            return Ok((
                Outcome::Stopped {
                    next_offset: state.next_offset,
                    recstate,
                },
                totals,
            ));
        }
        let began = Instant::now();
        let at = state.next_offset;
        let stop_at = at.saturating_add(segment).min(end);
        let r = scan_segment(
            Arc::clone(&device),
            db,
            at,
            stop_at,
            end,
            &settings.opts,
            &mut carry,
        )?;
        if let Some(rate) = settings.max_rate.filter(|r| *r > 0) {
            let want = Duration::from_secs_f64((stop_at - at) as f64 / rate as f64);
            if let Some(rest) = want.checked_sub(began.elapsed()) {
                std::thread::sleep(rest);
            }
        }

        state.next_offset = stop_at;
        state.candidates_committed += r.candidates.len() as u64;
        state.carry_cover_end = carry.cover_end;
        state.carry_candidates = carry.candidates;
        state.updated_unix = now();
        index.commit_segment(
            &r.candidates,
            &serde_json::to_string(&state).expect("serialises"),
        )?;
        state.save(&recstate)?;

        add(&mut totals.stats, &r.stats);
        totals.segments += 1;
        progress(Progress {
            next_offset: state.next_offset,
            range_start: state.range_start,
            range_end: end,
            candidates: state.candidates_committed,
        });

        // Aim each segment at the checkpoint interval, in whole blocks.
        let took = began.elapsed().as_secs_f64().max(0.001);
        let target = settings.checkpoint_every.as_secs_f64();
        let next = (segment as f64 * target / took).clamp(4.0 * 1048576.0, 1024.0 * 1048576.0);
        segment = ((next as u64) / block).max(1) * block;
    }

    index.finish()?;
    state.complete = true;
    state.updated_unix = now();
    index.commit_segment(&[], &serde_json::to_string(&state).expect("serialises"))?;
    state.save(&recstate)?;
    Ok((
        Outcome::Complete {
            candidates: state.candidates_committed,
        },
        totals,
    ))
}

fn add(t: &mut ScanStats, s: &ScanStats) {
    t.bytes_scanned += s.bytes_scanned;
    t.elapsed += s.elapsed;
    t.prefilter_hits += s.prefilter_hits;
    t.header_matches += s.header_matches;
    t.validated += s.validated;
    t.rejected += s.rejected;
    t.suppressed_contained += s.suppressed_contained;
    t.window_capped += s.window_capped;
    t.window_artifact_lengths += s.window_artifact_lengths;
    t.truncated |= s.truncated;
    t.strategy = s.strategy;
    t.threads = s.threads;
    t.signatures = s.signatures;
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> DeviceIdentity {
        DeviceIdentity {
            path: "a.img".into(),
            total_bytes: 1 << 29,
            sector_size: 512,
            serial: None,
            fingerprint: "ab".into(),
        }
    }

    #[test]
    fn the_same_device_by_another_name_matches() {
        let mut b = identity();
        b.path = r"\\?\D:\images\a.img".into();
        assert_eq!(identity().mismatch(&b), None);
    }

    #[test]
    fn a_different_device_is_refused_with_a_reason() {
        let mut b = identity();
        b.fingerprint = "cd".into();
        assert!(identity().mismatch(&b).unwrap().contains("MiB"));
        let mut c = identity();
        c.total_bytes += 512;
        assert!(identity().mismatch(&c).unwrap().contains("byte"));
    }

    #[test]
    fn config_hash_changes_with_anything_that_changes_results() {
        let base = ScanOptions::default();
        let h = config_hash("{}", &base);
        assert_eq!(h, config_hash("{}", &base));
        assert_ne!(h, config_hash("{ }", &base));
        let other = ScanOptions {
            suppress_contained: false,
            ..ScanOptions::default()
        };
        assert_ne!(h, config_hash("{}", &other));
    }

    #[test]
    fn recstate_path_sits_beside_the_index() {
        assert_eq!(
            recstate_path(Path::new("x/y.rcindex")),
            PathBuf::from("x/y.rcindex.recstate")
        );
    }
}
