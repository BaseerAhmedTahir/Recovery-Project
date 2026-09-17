//! `rc-score` - the Green / Yellow / Red confidence rating (SPEC.md 5.7).
//!
//! A rating a user cannot interrogate is a rating a user cannot trust, so every
//! [`Score`] carries the [`Inputs`] it was computed from and the [`Reason`] for
//! each rule that moved it, and every number that decides a band lives in
//! `rules.json` rather than in this file.
//!
//! # What the bands mean
//!
//! The loss policy was agreed for Milestone 5 and the overwritten fixture
//! records its ground truth by the same policy (`testdata/overmap.py`):
//!
//! - **RED** when the file's header cluster is lost, or half or more of its
//!   clusters are - or the device has discarded the data (TRIM and zeros).
//! - **YELLOW** when some clusters are lost but fewer than that, or when the
//!   format's own checks fail somewhere that cannot be pinned to a cluster.
//! - **GREEN** only with nothing lost.
//!
//! A cluster counts as *lost* on evidence, and there are four kinds:
//!
//! 1. **Allocated to a live file.** Its number is in a live entry's runs. The
//!    strongest evidence there is, and content-blind: a reused cluster holds
//!    somebody else's perfectly plausible bytes.
//! 2. **One repeated byte where the format cannot have that.** Compressed image
//!    data never can; databases and containers legitimately hold zero runs, so
//!    for those only other fills count. The lists are in `rules.json`.
//! 3. **Not text, in a text file.** A NUL, or more than a sliver of control
//!    bytes. Random bytes fail this within a few hundred characters; legacy
//!    encodings do not, which is why UTF-8 validity is not part of it.
//! 4. **Where the format's validator breaks.** Exact for formats that stop at
//!    the damage; merely "somewhere" for ones that notice damage but not where
//!    (a PNG counts failing checksums but its structure is intact).
//!
//! # What it cannot see
//!
//! A cluster overwritten with bytes that look like the file's own, in a format
//! with no internal structure, leaves no evidence. Such a file rates GREEN on
//! its metadata and says, in `no-structural-check`, that nothing more could be
//! checked. The overwritten fixture contains one on purpose.

use rc_carve::validate::{self, Status};
use rc_device::{ReadOnlyDevice, TrimSupport};
use rc_fs::{DataLocation, Entry, EntryState, Geometry, ScanResult};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// The rules that ship with the crate.
pub const BUILTIN_RULES: &str = include_str!("../rules.json");

/// Every rule this code can fire. A rules file missing one is refused at load
/// rather than failing silently when the rule would have applied.
pub const RULE_IDS: &[&str] = &[
    "trimmed",
    "header-lost",
    "half-or-more-lost",
    "clusters-lost",
    "damage-unlocated",
    "unverified-after-break",
    "truncated",
    "validated-prefix",
    "location-guessed",
    "location-unknown",
    "path-untrusted",
    "no-structural-check",
];

#[derive(Debug, thiserror::Error)]
pub enum ScoreError {
    #[error("scoring rules: {0}")]
    Rules(String),
    #[error("reading the device: {0}")]
    Device(#[from] rc_device::DeviceError),
}

pub type Result<T> = std::result::Result<T, ScoreError>;

// ---------------------------------------------------------------------------
// rules
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Deserialize)]
struct Bands {
    green_min: u8,
    yellow_min: u8,
}

#[derive(Clone, Debug, Deserialize)]
struct Rule {
    cap: Option<u8>,
    #[serde(default)]
    penalty: u8,
    why: String,
}

/// The data that decides scores, from `rules.json`.
#[derive(Clone, Debug, Deserialize)]
pub struct Rules {
    bands: Bands,
    rules: BTreeMap<String, Rule>,
    validator_for_extension: BTreeMap<String, String>,
    text_extensions: BTreeSet<String>,
    uniform_never_legal: BTreeSet<String>,
    uniform_legal_bytes_otherwise: BTreeSet<u8>,
}

impl Rules {
    pub fn builtin() -> Rules {
        Rules::parse(BUILTIN_RULES).expect("the shipped rules.json parses")
    }

    pub fn parse(json: &str) -> Result<Rules> {
        let r: Rules = serde_json::from_str(json).map_err(|e| ScoreError::Rules(e.to_string()))?;
        let missing: Vec<&str> = RULE_IDS
            .iter()
            .copied()
            .filter(|id| !r.rules.contains_key(*id))
            .collect();
        if !missing.is_empty() {
            return Err(ScoreError::Rules(format!("missing rule(s): {missing:?}")));
        }
        if !(r.bands.yellow_min < r.bands.green_min && r.bands.green_min <= 100) {
            return Err(ScoreError::Rules(
                "bands must satisfy yellow_min < green_min <= 100".into(),
            ));
        }
        for id in r.validator_for_extension.values() {
            if !validate::has_validator(id) {
                return Err(ScoreError::Rules(format!("no validator named {id:?}")));
            }
        }
        Ok(r)
    }

    fn rule(&self, id: &str) -> &Rule {
        self.rules.get(id).expect("rule ids are checked at load")
    }

    /// The validator for a file name's extension, if any.
    pub fn validator_for(&self, name: &str) -> Option<&str> {
        extension(name).and_then(|e| self.validator_for_extension.get(&e).map(String::as_str))
    }

    pub fn is_text(&self, name: &str) -> bool {
        extension(name).is_some_and(|e| self.text_extensions.contains(&e))
    }

    /// Whether a cluster of nothing but `byte` is evidence of loss in a file of
    /// this format.
    fn uniform_is_loss(&self, format: Option<&str>, text: bool, byte: u8) -> bool {
        if text {
            // Fixed-width padding is a run of a printable character; a run of
            // anything else is not text.
            return !(byte.is_ascii_graphic() || byte == b' ');
        }
        if format.is_some_and(|f| self.uniform_never_legal.contains(f)) {
            return true;
        }
        !self.uniform_legal_bytes_otherwise.contains(&byte)
    }
}

fn extension(name: &str) -> Option<String> {
    let (stem, ext) = name.rsplit_once('.')?;
    (!stem.is_empty() && !ext.is_empty()).then(|| ext.to_ascii_lowercase())
}

// ---------------------------------------------------------------------------
// what a score is made of
// ---------------------------------------------------------------------------

/// How the validator judged the content.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Validation {
    /// Structurally complete.
    Valid,
    /// No validator exists for this format.
    NoValidator,
    /// Only a prefix could be checked, and it held up.
    ValidatedPrefix { checked: u64 },
    /// The content ends before its structure does.
    Truncated { verified: u64 },
    /// The validator found damage. `at_cluster` is where, when the validator
    /// can say.
    Broken {
        at_cluster: Option<u64>,
        detail: String,
    },
    /// The header itself is not this format.
    HeaderRejected { detail: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Location {
    /// Exact cluster runs.
    Runs,
    /// The bytes live inside the metadata record.
    Resident,
    /// Only the first cluster is known; the rest is assumed contiguous.
    FirstClusterOnly,
    /// Nothing is known.
    Unknown,
}

/// Everything a score was computed from, kept so it can be explained.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Inputs {
    /// Filesystem the entry came from.
    pub filesystem: String,
    /// Validator id used, if any.
    pub format: Option<String>,
    pub text: bool,
    pub size: u64,
    /// Clusters the content occupies. Zero for resident content.
    pub clusters: u64,
    /// Indices of lost clusters within the file, on any evidence.
    pub lost: Vec<u64>,
    pub lost_to_live_file: u64,
    pub uniform: u64,
    pub not_text: u64,
    pub validation: Validation,
    pub location: Location,
    pub path_trusted: bool,
    /// The device reports TRIM support.
    pub trim: bool,
    /// Every cluster of the content reads as zero.
    pub all_zero: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum Band {
    Red,
    Yellow,
    Green,
}

impl std::fmt::Display for Band {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Band::Green => "GREEN",
            Band::Yellow => "YELLOW",
            Band::Red => "RED",
        })
    }
}

/// One rule that moved a score, and what it did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Reason {
    pub rule: String,
    pub cap: Option<u8>,
    pub penalty: u8,
    pub why: String,
    /// The specific facts that made it fire.
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Score {
    pub value: u8,
    pub band: Band,
    pub reasons: Vec<Reason>,
    pub inputs: Inputs,
}

// ---------------------------------------------------------------------------
// classification: inputs -> score
// ---------------------------------------------------------------------------

/// Rate a set of inputs. Pure: everything it uses is in `inputs` and `rules`.
pub fn classify(inputs: &Inputs, rules: &Rules) -> Score {
    let mut reasons = Vec::new();
    let mut fire = |id: &str, detail: String| {
        let r = rules.rule(id);
        reasons.push(Reason {
            rule: id.to_string(),
            cap: r.cap,
            penalty: r.penalty,
            why: r.why.clone(),
            detail,
        });
    };

    if inputs.trim && inputs.all_zero && inputs.size > 0 {
        fire(
            "trimmed",
            "TRIM supported and every cluster reads as zero".into(),
        );
    }

    let n = inputs.clusters;
    let lost = inputs.lost.len() as u64;
    let header_lost = matches!(inputs.validation, Validation::HeaderRejected { .. })
        || (n > 0 && inputs.lost.first() == Some(&0));
    if header_lost {
        let why = match &inputs.validation {
            Validation::HeaderRejected { detail } => format!("the validator rejects it: {detail}"),
            _ => "cluster 0 is lost".to_string(),
        };
        fire("header-lost", why);
    } else if n > 0 && lost * 2 >= n {
        fire(
            "half-or-more-lost",
            format!("{lost} of {n} clusters lost {}", evidence_summary(inputs)),
        );
    } else if lost > 0 {
        fire(
            "clusters-lost",
            format!("{lost} of {n} clusters lost {}", evidence_summary(inputs)),
        );
    }

    match &inputs.validation {
        Validation::Broken {
            at_cluster: None,
            detail,
        } => fire("damage-unlocated", detail.clone()),
        Validation::Broken {
            at_cluster: Some(k),
            detail,
        } if k + 1 < n => fire(
            "unverified-after-break",
            format!("break at cluster {k} of {n}: {detail}"),
        ),
        Validation::Truncated { verified } => fire(
            "truncated",
            format!("structure verified to byte {verified} of {}", inputs.size),
        ),
        Validation::ValidatedPrefix { checked } => fire(
            "validated-prefix",
            format!("the first {checked} of {} bytes validate", inputs.size),
        ),
        Validation::NoValidator => fire(
            "no-structural-check",
            match &inputs.format {
                Some(f) => format!("format {f}"),
                None => "no validator for this extension".into(),
            },
        ),
        _ => {}
    }

    match inputs.location {
        Location::FirstClusterOnly => fire(
            "location-guessed",
            "only the first cluster is recorded".into(),
        ),
        Location::Unknown => fire("location-unknown", "no layout recorded".into()),
        Location::Runs | Location::Resident => {}
    }
    if !inputs.path_trusted {
        fire("path-untrusted", "original folder not established".into());
    }

    let penalty: u32 = reasons.iter().map(|r| r.penalty as u32).sum();
    let mut value = 100u32.saturating_sub(penalty);
    for r in &reasons {
        if let Some(cap) = r.cap {
            value = value.min(cap as u32);
        }
    }
    let value = value as u8;
    let band = if value >= rules.bands.green_min {
        Band::Green
    } else if value >= rules.bands.yellow_min {
        Band::Yellow
    } else {
        Band::Red
    };
    Score {
        value,
        band,
        reasons,
        inputs: inputs.clone(),
    }
}

fn evidence_summary(i: &Inputs) -> String {
    let mut parts = Vec::new();
    if i.lost_to_live_file > 0 {
        parts.push(format!("{} allocated to live files", i.lost_to_live_file));
    }
    if i.uniform > 0 {
        parts.push(format!("{} filled with one repeated byte", i.uniform));
    }
    if i.not_text > 0 {
        parts.push(format!("{} no longer text", i.not_text));
    }
    if matches!(
        i.validation,
        Validation::Broken {
            at_cluster: Some(_),
            ..
        }
    ) {
        parts.push("one where the format's structure breaks".into());
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("({})", parts.join("; "))
    }
}

// ---------------------------------------------------------------------------
// evidence: an entry on a device -> inputs
// ---------------------------------------------------------------------------

/// Clusters that live files occupy, from every allocated entry's runs.
///
/// SPEC.md 5.4 enumerates live entries for exactly this.
#[derive(Clone, Debug, Default)]
pub struct Occupancy {
    /// Merged, sorted, half-open cluster ranges.
    ranges: Vec<(u64, u64)>,
}

impl Occupancy {
    pub fn from_scan(scan: &ScanResult) -> Occupancy {
        let mut ranges: Vec<(u64, u64)> = Vec::new();
        for e in scan
            .entries
            .iter()
            .filter(|e| e.state == EntryState::Allocated)
        {
            if let DataLocation::Runs(runs) = &e.location {
                for r in runs.iter().filter(|r| !r.sparse && r.cluster_count > 0) {
                    ranges.push((r.start_cluster, r.start_cluster + r.cluster_count));
                }
            }
        }
        ranges.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
        for (s, e) in ranges {
            match merged.last_mut() {
                Some(last) if s <= last.1 => last.1 = last.1.max(e),
                _ => merged.push((s, e)),
            }
        }
        Occupancy { ranges: merged }
    }

    pub fn contains(&self, cluster: u64) -> bool {
        let i = self.ranges.partition_point(|&(s, _)| s <= cluster);
        i > 0 && cluster < self.ranges[i - 1].1
    }

    /// Clusters occupied.
    pub fn clusters(&self) -> u64 {
        self.ranges.iter().map(|(s, e)| e - s).sum()
    }
}

/// What scoring an entry needs besides the entry.
pub struct Context<'a> {
    pub device: &'a dyn ReadOnlyDevice,
    pub geometry: Geometry,
    pub occupancy: &'a Occupancy,
    pub rules: &'a Rules,
    pub filesystem: String,
    /// The most content handed to a validator. Larger files are validated on a
    /// prefix, and say so.
    pub validation_limit: usize,
}

impl<'a> Context<'a> {
    pub fn new(
        device: &'a dyn ReadOnlyDevice,
        scan: &ScanResult,
        occupancy: &'a Occupancy,
        rules: &'a Rules,
        filesystem: impl Into<String>,
    ) -> Context<'a> {
        Context {
            device,
            geometry: scan.geometry,
            occupancy,
            rules,
            filesystem: filesystem.into(),
            validation_limit: 64 << 20,
        }
    }
}

pub fn score_entry(ctx: &Context, entry: &Entry) -> Result<Score> {
    Ok(classify(&inputs_for_entry(ctx, entry)?, ctx.rules))
}

/// Text, by the one test random bytes fail and real text in any encoding
/// passes: no NUL, and control bytes other than whitespace under 2%.
pub fn looks_like_text(b: &[u8]) -> bool {
    if b.contains(&0) {
        return false;
    }
    let control = b
        .iter()
        .filter(|&&x| (x < 0x20 && !matches!(x, b'\t' | b'\n' | b'\r' | 0x0C)) || x == 0x7F)
        .count();
    control * 50 <= b.len()
}

/// A cluster tail shorter than this is not judged uniform: a few repeated
/// bytes at the end of a file are ordinary.
const MIN_UNIFORM_BYTES: usize = 256;

pub fn inputs_for_entry(ctx: &Context, entry: &Entry) -> Result<Inputs> {
    let rules = ctx.rules;
    let format = rules.validator_for(&entry.name).map(str::to_string);
    let text = rules.is_text(&entry.name);
    let c = ctx.geometry.cluster_bytes.max(1);
    let size = entry.size;
    let needed = size.div_ceil(c);

    // The clusters the content lives in, in file order.
    let (location, clusters): (Location, Vec<Option<u64>>) = match &entry.location {
        DataLocation::Resident(_) => (Location::Resident, Vec::new()),
        DataLocation::Runs(runs) => {
            let mut v = Vec::new();
            'runs: for r in runs {
                for k in 0..r.cluster_count {
                    if v.len() as u64 >= needed {
                        break 'runs;
                    }
                    // A sparse run has no cluster: it reads as zeros and cannot
                    // be lost.
                    v.push((!r.sparse).then_some(r.start_cluster + k));
                }
            }
            (Location::Runs, v)
        }
        DataLocation::FirstClusterOnly(first) => (
            Location::FirstClusterOnly,
            (0..needed).map(|k| Some(first + k)).collect(),
        ),
        DataLocation::Unknown => (Location::Unknown, Vec::new()),
    };

    let mut lost: BTreeSet<u64> = BTreeSet::new();
    let (mut to_live, mut uniform, mut not_text) = (0u64, 0u64, 0u64);
    let mut all_zero = !clusters.is_empty();
    let mut content: Vec<u8> = Vec::new();
    let limit = ctx.validation_limit;

    if let DataLocation::Resident(bytes) = &entry.location {
        content.extend_from_slice(&bytes[..(size as usize).min(bytes.len())]);
        all_zero = false;
    }

    let mut buf = vec![0u8; c as usize];
    for (i, cl) in clusters.iter().enumerate() {
        let i = i as u64;
        // Bytes of the file in this cluster: all of it but the last.
        let len = if i + 1 == needed {
            (size - c * i) as usize
        } else {
            c as usize
        };
        let Some(cluster) = cl else {
            // Sparse: zeros by definition.
            if content.len() < limit {
                content.resize((content.len() + len).min(limit), 0);
            }
            continue;
        };
        let Some(at) = ctx.geometry.cluster_offset(*cluster) else {
            // A run pointing outside the volume: whatever was there, it is not
            // readable as this file.
            lost.insert(i);
            all_zero = false;
            continue;
        };
        let n = ctx.device.read_bytes_at(at, &mut buf[..len])?;
        let bytes = &buf[..n];

        if bytes.iter().any(|&b| b != 0) {
            all_zero = false;
        }
        // A live file's clusters are its own; only a deleted file can lose one
        // to a live file.
        if entry.state != EntryState::Allocated && ctx.occupancy.contains(*cluster) {
            to_live += 1;
            lost.insert(i);
        }
        if bytes.len() >= MIN_UNIFORM_BYTES
            && bytes.iter().all(|&b| b == bytes[0])
            && rules.uniform_is_loss(format.as_deref(), text, bytes[0])
        {
            uniform += 1;
            lost.insert(i);
        }
        if text && !looks_like_text(bytes) {
            not_text += 1;
            lost.insert(i);
        }
        if content.len() < limit {
            let take = bytes.len().min(limit - content.len());
            content.extend_from_slice(&bytes[..take]);
        }
    }

    let validation = match (&format, &location) {
        (None, _) => Validation::NoValidator,
        (Some(_), Location::Unknown) => Validation::NoValidator,
        (Some(id), _) => {
            let out = validate::validate(id, &content).expect("ids are checked at load");
            let prefix = (content.len() as u64) < size;
            match out.status {
                Status::Valid => Validation::Valid,
                _ if prefix && out.ran_out => Validation::ValidatedPrefix {
                    checked: content.len() as u64,
                },
                Status::Rejected if !out.ran_out => {
                    Validation::HeaderRejected { detail: out.detail }
                }
                _ if out.ran_out => Validation::Truncated {
                    verified: out.length,
                },
                _ => {
                    // Damage found. The validator's length is the last byte it
                    // vouches for; the damage is at or after the cluster holding
                    // it. When that length is the whole file the damage is real
                    // but unplaced - a PNG with failing checksums knows exactly
                    // how long it is.
                    // Validators whose length stays the file's own - SQLite
                    // pages, PNG chunks - say where the damage begins instead.
                    let at = match out
                        .evidence_of("damage_at")
                        .and_then(|v| v.parse::<u64>().ok())
                    {
                        Some(off) if off < size => Some(off / c),
                        _ => (out.length < size).then(|| out.length / c),
                    };
                    Validation::Broken {
                        at_cluster: at,
                        detail: out.detail,
                    }
                }
            }
        }
    };

    // A break counts as one lost cluster where it happened, unless content
    // evidence already places a loss at or beyond it. It never counts as the
    // header: a decoder stops at the last whole unit it read, which can sit in
    // the cluster before the damage, so a break in cluster 0 does not prove
    // cluster 0 is gone. Only the validator rejecting the header does.
    if let Validation::Broken {
        at_cluster: Some(k),
        ..
    } = &validation
    {
        if needed > 0 && !lost.iter().any(|&i| i >= *k) {
            lost.insert((*k).clamp(1.min(needed - 1), needed - 1));
        }
    }

    Ok(Inputs {
        filesystem: ctx.filesystem.clone(),
        format,
        text,
        size,
        clusters: clusters.len() as u64,
        lost: lost.into_iter().collect(),
        lost_to_live_file: to_live,
        uniform,
        not_text,
        validation,
        location,
        path_trusted: entry.path.is_some() && entry.path_confidence.is_trustworthy(),
        trim: ctx.device.trim_supported() == TrimSupport::Yes,
        all_zero,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean(clusters: u64) -> Inputs {
        Inputs {
            filesystem: "ntfs".into(),
            format: Some("png".into()),
            text: false,
            size: clusters * 4096,
            clusters,
            lost: vec![],
            lost_to_live_file: 0,
            uniform: 0,
            not_text: 0,
            validation: Validation::Valid,
            location: Location::Runs,
            path_trusted: true,
            trim: false,
            all_zero: false,
        }
    }

    #[test]
    fn the_shipped_rules_load_and_name_every_rule_the_code_fires() {
        let r = Rules::builtin();
        for id in RULE_IDS {
            assert!(r.rules.contains_key(*id), "{id}");
        }
    }

    #[test]
    fn a_rules_file_missing_a_rule_is_refused() {
        let mut v: serde_json::Value = serde_json::from_str(BUILTIN_RULES).unwrap();
        v["rules"].as_object_mut().unwrap().remove("header-lost");
        assert!(Rules::parse(&v.to_string()).is_err());
    }

    #[test]
    fn nothing_lost_and_structure_valid_is_green() {
        let s = classify(&clean(5), &Rules::builtin());
        assert_eq!(s.band, Band::Green);
        assert_eq!(s.value, 100);
        assert!(s.reasons.is_empty());
    }

    #[test]
    fn the_agreed_loss_policy() {
        let r = Rules::builtin();
        let with = |lost: Vec<u64>, n| {
            let mut i = clean(n);
            i.lost = lost;
            classify(&i, &r).band
        };
        assert_eq!(
            with(vec![2], 5),
            Band::Yellow,
            "one of five, not the header"
        );
        assert_eq!(with(vec![0], 9), Band::Red, "the header");
        assert_eq!(with(vec![1, 2], 4), Band::Red, "exactly half");
        assert_eq!(with(vec![1, 2], 5), Band::Yellow, "under half");
        assert_eq!(with(vec![1, 2, 3], 5), Band::Red, "over half");
    }

    #[test]
    fn a_rejected_header_is_red_even_with_no_cluster_evidence() {
        let mut i = clean(9);
        i.validation = Validation::HeaderRejected {
            detail: "does not begin with SOI".into(),
        };
        let s = classify(&i, &Rules::builtin());
        assert_eq!(s.band, Band::Red);
        assert_eq!(s.reasons[0].rule, "header-lost");
    }

    #[test]
    fn unplaced_damage_is_yellow() {
        let mut i = clean(5);
        i.validation = Validation::Broken {
            at_cluster: None,
            detail: "1 chunk checksum fails".into(),
        };
        assert_eq!(classify(&i, &Rules::builtin()).band, Band::Yellow);
    }

    #[test]
    fn no_validator_is_green_on_evidence_and_says_so() {
        let mut i = clean(4);
        i.format = None;
        i.validation = Validation::NoValidator;
        let s = classify(&i, &Rules::builtin());
        assert_eq!(s.band, Band::Green);
        assert!(s.reasons.iter().any(|r| r.rule == "no-structural-check"));
    }

    #[test]
    fn trim_and_zeros_is_red_with_reason_trimmed() {
        let mut i = clean(4);
        i.trim = true;
        i.all_zero = true;
        let s = classify(&i, &Rules::builtin());
        assert_eq!(s.band, Band::Red);
        assert_eq!(s.value, 0);
        assert_eq!(s.reasons[0].rule, "trimmed");
    }

    #[test]
    fn zeros_without_trim_are_not_trimmed() {
        let mut i = clean(4);
        i.all_zero = true;
        assert!(!classify(&i, &Rules::builtin())
            .reasons
            .iter()
            .any(|r| r.rule == "trimmed"));
    }

    #[test]
    fn uniform_evidence_follows_the_format() {
        let r = Rules::builtin();
        assert!(r.uniform_is_loss(Some("png"), false, 0), "zeros in a PNG");
        assert!(
            !r.uniform_is_loss(Some("sqlite"), false, 0),
            "zeros in SQLite are ordinary"
        );
        assert!(r.uniform_is_loss(Some("sqlite"), false, 0xDB));
        assert!(!r.uniform_is_loss(None, false, 0xFF), "0xFF in a blob");
        assert!(!r.uniform_is_loss(None, true, b' '), "padding in text");
        assert!(r.uniform_is_loss(None, true, 0xDB));
    }

    #[test]
    fn text_survives_any_encoding_and_random_bytes_do_not() {
        assert!(looks_like_text("plain ascii\nlines\tand tabs".as_bytes()));
        assert!(looks_like_text("日本語のテキスト 🎉".as_bytes()));
        assert!(
            looks_like_text(&[0xE9, 0x20, 0x61, 0xE8, 0x0A]),
            "Latin-1 is not UTF-8 but is text"
        );
        let mut x = 0x2545F491u32;
        let random: Vec<u8> = (0..4096)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect();
        assert!(!looks_like_text(&random));
    }

    #[test]
    fn occupancy_merges_and_answers_membership() {
        let scan = ScanResult::default();
        assert!(!Occupancy::from_scan(&scan).contains(0));
        let o = Occupancy {
            ranges: vec![(10, 20), (30, 31)],
        };
        assert!(o.contains(10) && o.contains(19) && o.contains(30));
        assert!(!o.contains(9) && !o.contains(20) && !o.contains(31));
        assert_eq!(o.clusters(), 11);
    }
}
