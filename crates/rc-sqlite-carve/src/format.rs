//! The parts of the SQLite file format carving needs, read from bytes only
//! (sqlite.org/fileformat2.html). Nothing here opens a database through SQLite:
//! opening a WAL-mode database with the library creates `-shm` and can
//! checkpoint the WAL into the main file, which destroys exactly the older page
//! versions that hold deleted rows.

use serde::Serialize;

pub const HEADER_MAGIC: &[u8; 16] = b"SQLite format 3\0";

pub fn be16(d: &[u8], at: usize) -> Option<usize> {
    Some(u16::from_be_bytes([*d.get(at)?, *d.get(at + 1)?]) as usize)
}

pub fn be32(d: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(d.get(at..at + 4)?.try_into().ok()?))
}

/// A SQLite varint: value and length in bytes.
pub fn varint(d: &[u8], at: usize) -> Option<(u64, usize)> {
    let mut v = 0u64;
    for i in 0..9 {
        let b = *d.get(at + i)?;
        if i == 8 {
            return Some(((v << 8) | b as u64, 9));
        }
        v = (v << 7) | (b & 0x7F) as u64;
        if b & 0x80 == 0 {
            return Some((v, i + 1));
        }
    }
    None
}

#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum Value {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(#[serde(serialize_with = "hex_blob")] Vec<u8>),
}

fn hex_blob<S: serde::Serializer>(b: &[u8], s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&format!("x'{}'", hex::encode(b)))
}

impl Value {
    /// A comparable key: equal rows have equal keys, floats compared by bits.
    pub fn key(&self) -> String {
        match self {
            Value::Null => "n".into(),
            Value::Integer(i) => format!("i{i}"),
            Value::Real(f) => format!("r{}", f.to_bits()),
            Value::Text(t) => format!("t{}:{t}", t.len()),
            Value::Blob(b) => format!("b{}", hex::encode(b)),
        }
    }
}

/// Bytes of content a serial type occupies, or `None` for the reserved types.
pub fn serial_len(t: u64) -> Option<usize> {
    Some(match t {
        0 | 8 | 9 => 0,
        1 => 1,
        2 => 2,
        3 => 3,
        4 => 4,
        5 => 6,
        6 | 7 => 8,
        10 | 11 => return None,
        n => ((n - 12) / 2) as usize,
    })
}

pub fn decode_value(t: u64, b: &[u8], encoding: u32) -> Option<Value> {
    let int = |n: usize| -> i64 {
        let mut v: i64 = if b[0] & 0x80 != 0 { -1 } else { 0 };
        for &x in &b[..n] {
            v = (v << 8) | x as i64;
        }
        v
    };
    Some(match t {
        0 => Value::Null,
        1 => Value::Integer(int(1)),
        2 => Value::Integer(int(2)),
        3 => Value::Integer(int(3)),
        4 => Value::Integer(int(4)),
        5 => Value::Integer(int(6)),
        6 => Value::Integer(int(8)),
        7 => Value::Real(f64::from_bits(u64::from_be_bytes(b[..8].try_into().ok()?))),
        8 => Value::Integer(0),
        9 => Value::Integer(1),
        10 | 11 => return None,
        n if n % 2 == 0 => Value::Blob(b.to_vec()),
        _ => Value::Text(decode_text(b, encoding)?),
    })
}

fn decode_text(b: &[u8], encoding: u32) -> Option<String> {
    match encoding {
        2 | 3 => {
            if b.len() % 2 != 0 {
                return None;
            }
            let units: Vec<u16> = b
                .chunks(2)
                .map(|c| {
                    if encoding == 2 {
                        u16::from_le_bytes([c[0], c[1]])
                    } else {
                        u16::from_be_bytes([c[0], c[1]])
                    }
                })
                .collect();
            String::from_utf16(&units).ok()
        }
        _ => String::from_utf8(b.to_vec()).ok(),
    }
}

/// A record decoded from a complete, in-bounds payload.
pub fn decode_record(payload: &[u8], encoding: u32) -> Option<Vec<Value>> {
    let (h, hl) = varint(payload, 0)?;
    let h = h as usize;
    if h < hl || h > payload.len() {
        return None;
    }
    let mut types = Vec::new();
    let mut at = hl;
    while at < h {
        let (t, l) = varint(payload, at)?;
        types.push(t);
        at += l;
    }
    if at != h {
        return None;
    }
    let mut body = h;
    let mut out = Vec::with_capacity(types.len());
    for t in types {
        let n = serial_len(t)?;
        let b = payload.get(body..body + n)?;
        out.push(decode_value(t, b, encoding)?);
        body += n;
    }
    Some(out)
}

/// How much of a table b-tree leaf cell's payload is stored on the page.
pub fn local_payload(p: usize, usable: usize) -> usize {
    let x = usable - 35;
    if p <= x {
        return p;
    }
    let m = ((usable - 12) * 32 / 255) - 23;
    let k = m + (p - m) % (usable - 4);
    if k <= x {
        k
    } else {
        m
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_of_every_length() {
        assert_eq!(varint(&[0x05], 0), Some((5, 1)));
        assert_eq!(varint(&[0x81, 0x00], 0), Some((128, 2)));
        assert_eq!(varint(&[0xFF; 9], 0), Some((u64::MAX, 9)));
    }

    #[test]
    fn record_with_every_integer_width_and_text() {
        // header: size 8, types 1,2,4,6,8,19(text 3 bytes),0
        let mut p = vec![8u8, 1, 2, 4, 6, 8, 19, 0];
        p.push(0xFF); // -1
        p.extend_from_slice(&300i16.to_be_bytes());
        p.extend_from_slice(&(-70000i32).to_be_bytes());
        p.extend_from_slice(&(1i64 << 40).to_be_bytes());
        p.extend_from_slice(b"abc");
        let v = decode_record(&p, 1).unwrap();
        let keys: Vec<String> = v.iter().map(Value::key).collect();
        assert_eq!(
            keys,
            vec![
                "i-1",
                "i300",
                "i-70000",
                "i1099511627776",
                "i0",
                "t3:abc",
                "n"
            ]
        );
    }

    #[test]
    fn local_payload_matches_the_documented_thresholds() {
        // 4096-byte pages: everything up to 4061 bytes is stored locally.
        assert_eq!(local_payload(4061, 4096), 4061);
        assert!(local_payload(4062, 4096) < 4062);
        assert_eq!(local_payload(100, 4096), 100);
    }
}
