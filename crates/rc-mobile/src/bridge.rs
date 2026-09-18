//! The desktop end of the companion-app bridge (SPEC.md 6.4), the one socket
//! this project is allowed (SPEC.md 4.4). Compiled only with the `bridge`
//! feature.
//!
//! The listener binds to 127.0.0.1 and nothing else, and drops any connection
//! whose peer is not loopback. The phone reaches it through
//! `adb reverse tcp:PORT tcp:PORT`, which carries the connection over the USB
//! cable; nothing is reachable from the network. Wi-Fi mode (TLS with a pinned
//! per-session certificate) is **not implemented**: it would need a TLS stack,
//! which the offline audit forbids, and a network listener, which this module
//! refuses to create.
//!
//! Protocol (`docs/BRIDGE.md`), all lines UTF-8 and `\n`-terminated:
//!
//! ```text
//! phone:   RCB1 <pairing code>
//! desktop: OK | NO
//! phone:   {"path":"/storage/...","size":N,"sha256":"...","category":"trashed"}
//! phone:   <N bytes>
//! desktop: ACK <sha256> | NAK <reason>
//! ...
//! phone:   {"end":true}
//! desktop: BYE <files received>
//! ```
//!
//! The pairing code is shown on the computer and typed into the app by the
//! person holding the phone, so a session cannot start without them.

use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const PROTOCOL: &str = "RCB1";
const MAX_LINE: usize = 64 * 1024;
const MAX_FILE: u64 = 64 << 30;

#[derive(Debug, Deserialize)]
struct Header {
    #[serde(default)]
    end: bool,
    #[serde(default)]
    path: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    sha256: String,
    #[serde(default)]
    category: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Received {
    pub remote_path: String,
    pub category: String,
    pub local: PathBuf,
    pub size: u64,
    pub sha256: String,
}

pub struct Receiver {
    listener: TcpListener,
    code: String,
    out: PathBuf,
}

/// A six-digit code from the OS-seeded hasher. Not a cryptographic secret: the
/// socket is loopback-only, and the code exists so that a session needs the
/// phone's holder to type what this computer shows.
pub fn pairing_code() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    h.write_u32(std::process::id());
    format!("{:06}", h.finish() % 1_000_000)
}

impl Receiver {
    /// Listen on 127.0.0.1:`port` (0 picks a free port). `out` must be empty or
    /// not exist.
    pub fn bind(port: u16, out: &Path) -> Result<Receiver> {
        Self::bind_with_code(port, out, &pairing_code())
    }

    /// As [`Receiver::bind`], with the pairing code given rather than
    /// generated. For the test that replays a recorded session.
    pub fn bind_with_code(port: u16, out: &Path, code: &str) -> Result<Receiver> {
        if out.exists() && std::fs::read_dir(out)?.next().is_some() {
            return Err(Error::Destination(format!(
                "{} is not empty; choose a new directory",
                out.display()
            )));
        }
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
            .map_err(|e| Error::Bridge(format!("cannot listen on 127.0.0.1:{port}: {e}")))?;
        Ok(Receiver {
            listener,
            code: code.to_string(),
            out: out.to_path_buf(),
        })
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.listener.local_addr()?)
    }

    pub fn port(&self) -> u16 {
        self.listener.local_addr().map(|a| a.port()).unwrap_or(0)
    }

    pub fn code(&self) -> &str {
        &self.code
    }

    /// Accept connections until one pairs, then receive its files. Connections
    /// that are not loopback, or give the wrong code, are closed.
    pub fn serve(&self, idle_timeout: Duration) -> Result<Vec<Received>> {
        loop {
            let (stream, peer) = self.listener.accept()?;
            if !is_loopback(&peer) {
                continue;
            }
            stream.set_read_timeout(Some(idle_timeout))?;
            match self.session(stream) {
                Ok(Some(files)) => return Ok(files),
                Ok(None) => continue,
                Err(e) => return Err(e),
            }
        }
    }

    fn session(&self, stream: TcpStream) -> Result<Option<Vec<Received>>> {
        let mut w = stream.try_clone()?;
        let mut r = BufReader::new(stream);
        let hello = read_line(&mut r)?;
        let expected = format!("{PROTOCOL} {}", self.code);
        if hello.trim_end() != expected {
            let _ = w.write_all(b"NO\n");
            return Ok(None);
        }
        w.write_all(b"OK\n")?;
        std::fs::create_dir_all(&self.out)?;
        let mut files = Vec::new();
        loop {
            let line = read_line(&mut r)?;
            let h: Header = serde_json::from_str(&line)
                .map_err(|e| Error::Bridge(format!("bad header: {e}")))?;
            if h.end {
                w.write_all(format!("BYE {}\n", files.len()).as_bytes())?;
                std::fs::write(
                    self.out.join("manifest.json"),
                    serde_json::to_vec_pretty(&files).map_err(|e| Error::Other(e.to_string()))?,
                )?;
                return Ok(Some(files));
            }
            if h.size > MAX_FILE {
                w.write_all(b"NAK too large\n")?;
                return Err(Error::Bridge(format!("{} is too large", h.path)));
            }
            let local = self.local_path(&h);
            if let Some(p) = local.parent() {
                std::fs::create_dir_all(p)?;
            }
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&local)?;
            let mut hasher = Sha256::new();
            let mut left = h.size;
            let mut buf = vec![0u8; 1 << 16];
            while left > 0 {
                let n = r.read(&mut buf[..(left.min(1 << 16)) as usize])?;
                if n == 0 {
                    return Err(Error::Bridge(format!(
                        "{}: connection closed mid-file",
                        h.path
                    )));
                }
                hasher.update(&buf[..n]);
                file.write_all(&buf[..n])?;
                left -= n as u64;
            }
            file.flush()?;
            let got = hex::encode(hasher.finalize());
            if !got.eq_ignore_ascii_case(&h.sha256) {
                drop(file);
                std::fs::remove_file(&local)?;
                w.write_all(format!("NAK sha256 {got}\n").as_bytes())?;
                continue;
            }
            w.write_all(format!("ACK {got}\n").as_bytes())?;
            files.push(Received {
                remote_path: h.path.clone(),
                category: h.category.clone(),
                local,
                size: h.size,
                sha256: got,
            });
        }
    }

    fn local_path(&self, h: &Header) -> PathBuf {
        let cat: String = h
            .category
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        let mut p = self
            .out
            .join(if cat.is_empty() { "other".into() } else { cat });
        for part in h.path.split('/') {
            if part.is_empty() || part == "." || part == ".." {
                continue;
            }
            p.push(
                part.chars()
                    .map(|c| match c {
                        '<' | '>' | ':' | '"' | '\\' | '|' | '?' | '*' => '_',
                        c if c.is_control() => '_',
                        c => c,
                    })
                    .collect::<String>(),
            );
        }
        p
    }
}

fn is_loopback(a: &SocketAddr) -> bool {
    a.ip().is_loopback()
}

fn read_line(r: &mut impl BufRead) -> Result<String> {
    let mut line = Vec::new();
    let n = r
        .by_ref()
        .take(MAX_LINE as u64)
        .read_until(b'\n', &mut line)?;
    if n == 0 {
        return Err(Error::Bridge("connection closed".into()));
    }
    if line.last() != Some(&b'\n') {
        return Err(Error::Bridge("line too long or unterminated".into()));
    }
    String::from_utf8(line).map_err(|_| Error::Bridge("line is not UTF-8".into()))
}

/// `adb reverse tcp:PORT tcp:PORT`, so the app's connection to its own
/// localhost arrives here over USB.
pub fn adb_reverse(adb: &crate::adb::Adb, port: u16) -> Result<()> {
    let spec = format!("tcp:{port}");
    adb.run(&["reverse", &spec, &spec])?;
    Ok(())
}
