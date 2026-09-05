//! Streaming hashes computed during a clone (SPEC.md section 5.2).
//!
//! SHA-256 and MD5 are updated as bytes flow past, so imaging a drive does not
//! require a second full read to verify it. The `.hash` manifest that comes out
//! is the artefact you keep: it is what `rc-cli verify` checks a clone against
//! months later.
//!
//! A clone with unreadable regions cannot produce a meaningful whole-device
//! hash, because the bad ranges were never read. That case is recorded
//! explicitly in the manifest rather than silently hashing zeros.

use md5::Md5;
use sha2::{Digest, Sha256};
use std::path::Path;

/// Rolling SHA-256 + MD5 over a byte stream.
#[derive(Default)]
pub struct StreamHasher {
    sha256: Sha256,
    md5: Md5,
    bytes: u64,
}

impl StreamHasher {
    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    pub fn update(&mut self, data: &[u8]) {
        self.sha256.update(data);
        self.md5.update(data);
        self.bytes += data.len() as u64;
    }

    pub fn bytes_hashed(&self) -> u64 {
        self.bytes
    }

    pub fn finish(self) -> Digests {
        Digests {
            sha256: hex::encode(self.sha256.finalize()),
            md5: hex::encode(self.md5.finalize()),
            bytes: self.bytes,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Digests {
    pub sha256: String,
    pub md5: String,
    pub bytes: u64,
}

/// The `.hash` sidecar written next to a clone.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct HashManifest {
    pub source: String,
    pub output: String,
    pub created_utc: String,
    pub total_bytes: u64,
    /// Digests over the bytes actually read from the source.
    pub source_digests: Digests,
    /// Digests over the bytes actually written to the output.
    pub output_digests: Digests,
    /// Bytes that could not be read and were filled in the output.
    pub bad_bytes: u64,
    /// Byte value used to fill unreadable regions.
    pub fill_byte: u8,
    /// False when `bad_bytes > 0`: the clone is not a faithful whole-device
    /// copy, so its hash cannot be compared against the physical device.
    pub complete: bool,
    pub tool: String,
}

impl HashManifest {
    pub fn write_to(&self, path: &Path) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(path, json + "\n")
    }

    pub fn read_from(path: &Path) -> std::io::Result<Self> {
        let data = std::fs::read_to_string(path)?;
        serde_json::from_str(&data)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}

/// Hash an existing file end to end. Used by `rc-cli verify`.
pub fn hash_file(path: &Path) -> std::io::Result<Digests> {
    let mut f = std::fs::File::open(path)?;
    let mut hasher = StreamHasher::new();
    let mut buf = vec![0u8; 4 * 1024 * 1024];
    loop {
        use std::io::Read;
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_known_vectors_for_empty_input() {
        let d = StreamHasher::new().finish();
        assert_eq!(
            d.sha256,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(d.md5, "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(d.bytes, 0);
    }

    #[test]
    fn matches_known_vectors_for_abc() {
        let mut h = StreamHasher::new();
        h.update(b"abc");
        let d = h.finish();
        assert_eq!(
            d.sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(d.md5, "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(d.bytes, 3);
    }

    /// Chunk boundaries must not change the result; a clone hashes in whatever
    /// block size the rescue algorithm happens to be using at the time.
    #[test]
    fn is_independent_of_chunking() {
        let data: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();

        let mut whole = StreamHasher::new();
        whole.update(&data);
        let whole = whole.finish();

        for chunk in [1usize, 7, 512, 4096] {
            let mut h = StreamHasher::new();
            for part in data.chunks(chunk) {
                h.update(part);
            }
            let got = h.finish();
            assert_eq!(got, whole, "chunk size {chunk} changed the digest");
        }
    }

    #[test]
    fn hashes_a_file_from_disk() {
        let dir = std::env::temp_dir().join("rc-image-hash-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("abc.bin");
        std::fs::write(&p, b"abc").unwrap();
        let d = hash_file(&p).unwrap();
        assert_eq!(
            d.sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn manifest_round_trips() {
        let dir = std::env::temp_dir().join("rc-image-hash-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("m.hash");
        let m = HashManifest {
            source: "src.img".into(),
            output: "out.raw".into(),
            created_utc: "2026-09-05T00:00:00Z".into(),
            total_bytes: 4096,
            source_digests: Digests {
                sha256: "a".into(),
                md5: "b".into(),
                bytes: 4096,
            },
            output_digests: Digests {
                sha256: "a".into(),
                md5: "b".into(),
                bytes: 4096,
            },
            bad_bytes: 0,
            fill_byte: 0,
            complete: true,
            tool: "rc-image".into(),
        };
        m.write_to(&p).unwrap();
        let back = HashManifest::read_from(&p).unwrap();
        assert_eq!(back.total_bytes, 4096);
        assert!(back.complete);
        assert_eq!(back.source_digests.sha256, "a");
    }
}
