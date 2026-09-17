//! The bridge receiver, with a client playing the companion app over loopback.
//! The Kotlin side of the same protocol is checked in companion-android.

#![cfg(feature = "bridge")]

use rc_mobile::bridge::Receiver;
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

fn send(w: &mut TcpStream, path: &str, bytes: &[u8], claimed_sha: Option<&str>) {
    let sha = claimed_sha
        .map(str::to_string)
        .unwrap_or_else(|| hex::encode(Sha256::digest(bytes)));
    let header = serde_json::json!({"path": path, "size": bytes.len(), "sha256": sha, "category": "trashed"});
    w.write_all(format!("{header}\n").as_bytes()).unwrap();
    w.write_all(bytes).unwrap();
}

#[test]
fn a_paired_session_transfers_verified_files_only() {
    let out = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("bridge-out");
    let _ = std::fs::remove_dir_all(&out);
    let rx = Receiver::bind(0, &out).unwrap();
    let port = rx.port();
    let code = rx.code().to_string();

    let client = std::thread::spawn(move || {
        // Wrong code first: refused.
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.write_all(b"RCB1 000000x\n").unwrap();
        let mut r = BufReader::new(s.try_clone().unwrap());
        let mut line = String::new();
        r.read_line(&mut line).unwrap();
        assert_eq!(line, "NO\n");

        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.write_all(format!("RCB1 {code}\n").as_bytes()).unwrap();
        let mut r = BufReader::new(s.try_clone().unwrap());
        let mut replies = Vec::new();
        let next = |r: &mut BufReader<TcpStream>| {
            let mut l = String::new();
            r.read_line(&mut l).unwrap();
            l
        };
        replies.push(next(&mut r));
        let photo: Vec<u8> = (0..200_000u32).map(|i| (i % 253) as u8).collect();
        send(
            &mut s,
            "/storage/emulated/0/DCIM/.trashed-1726000000-a.jpg",
            &photo,
            None,
        );
        replies.push(next(&mut r));
        send(
            &mut s,
            "/storage/emulated/0/../../etc/evil",
            b"corrupt",
            Some("00"),
        );
        replies.push(next(&mut r));
        send(&mut s, "/storage/emulated/0/Movies/b.mp4", b"movie", None);
        replies.push(next(&mut r));
        s.write_all(b"{\"end\":true}\n").unwrap();
        replies.push(next(&mut r));
        replies
    });

    let files = rx.serve(Duration::from_secs(20)).unwrap();
    let replies = client.join().unwrap();
    assert_eq!(replies[0], "OK\n");
    assert!(replies[1].starts_with("ACK "));
    assert!(replies[2].starts_with("NAK sha256"), "{}", replies[2]);
    assert!(replies[3].starts_with("ACK "));
    assert_eq!(replies[4], "BYE 2\n");

    assert_eq!(files.len(), 2);
    for f in &files {
        assert!(
            f.local.starts_with(&out),
            "{:?} escaped the output",
            f.local
        );
        let got = hex::encode(Sha256::digest(std::fs::read(&f.local).unwrap()));
        assert_eq!(got, f.sha256);
    }
    // The corrupt file was removed, and nothing climbed out of the directory.
    assert!(!out.join("trashed/etc/evil").exists());
    assert!(out.join("manifest.json").is_file());
}

#[test]
fn the_receiver_listens_on_loopback_only() {
    let out = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("bridge-bind");
    let _ = std::fs::remove_dir_all(&out);
    let rx = Receiver::bind(0, &out).unwrap();
    let addr = rx.local_addr().unwrap();
    assert!(addr.ip().is_loopback(), "bound to {addr}");
    assert_eq!(addr.ip().to_string(), "127.0.0.1");
    assert!(TcpStream::connect(("127.0.0.1", rx.port())).is_ok());
}
