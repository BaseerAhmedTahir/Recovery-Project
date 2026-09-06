//! NTFS runlist decoding.
//!
//! A non-resident attribute stores its cluster layout as a sequence of runs.
//! Each run begins with a header byte packing two nibbles: the low nibble is
//! the byte-length of the run's *length* field, the high nibble the byte-length
//! of its *offset* field. A header byte of zero ends the list.
//!
//! Two details cause most of the bugs here:
//!
//! * **Offsets are signed and relative to the previous run's start.** They are
//!   little-endian two's-complement integers of 1 to 8 bytes, sign-extended
//!   from their top bit. A file whose second fragment lies earlier on the disk
//!   than its first has a negative offset, and reading it as unsigned produces
//!   a cluster number in the exabytes.
//! * **A zero-length offset field means a sparse run.** The run occupies no
//!   clusters at all and reads as zeroes; the running LCN must not move. Read
//!   naively this looks like a run at the previous position, which silently
//!   duplicates data.

use crate::entry::Extent;
use crate::error::{FsError, Result};

/// Decode a runlist into extents.
///
/// `max_clusters` bounds the volume, so a corrupt runlist cannot produce runs
/// pointing off the end of the disk.
pub fn decode_runlist(data: &[u8], max_clusters: u64) -> Result<Vec<Extent>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    let mut current_lcn: i64 = 0;

    while pos < data.len() {
        let header = data[pos];
        if header == 0 {
            break; // end of list
        }
        pos += 1;

        let len_bytes = (header & 0x0F) as usize;
        let off_bytes = (header >> 4) as usize;

        if len_bytes == 0 {
            return Err(FsError::Runlist {
                detail: format!("run at offset {pos} has a zero-length length field"),
            });
        }
        if len_bytes > 8 || off_bytes > 8 {
            return Err(FsError::Runlist {
                detail: format!(
                    "run at offset {pos} declares {len_bytes}-byte length and \
                     {off_bytes}-byte offset; both must be 8 or fewer"
                ),
            });
        }
        if pos + len_bytes + off_bytes > data.len() {
            return Err(FsError::Runlist {
                detail: format!(
                    "run at offset {pos} needs {} bytes but only {} remain",
                    len_bytes + off_bytes,
                    data.len() - pos
                ),
            });
        }

        // Length is unsigned.
        let length = read_le_unsigned(&data[pos..pos + len_bytes]);
        pos += len_bytes;

        if length == 0 {
            return Err(FsError::Runlist {
                detail: "run declares a length of zero clusters".to_string(),
            });
        }

        if off_bytes == 0 {
            // Sparse run: no clusters allocated, and crucially the running LCN
            // does NOT advance.
            out.push(Extent::sparse(length));
        } else {
            // Offset is signed and relative to the previous run's start.
            let delta = read_le_signed(&data[pos..pos + off_bytes]);
            pos += off_bytes;

            current_lcn = current_lcn
                .checked_add(delta)
                .ok_or_else(|| FsError::Runlist {
                    detail: format!("run offset {delta} overflows the cluster address space"),
                })?;

            if current_lcn < 0 {
                return Err(FsError::Runlist {
                    detail: format!("run resolves to negative cluster {current_lcn}"),
                });
            }
            let start = current_lcn as u64;
            if max_clusters > 0 && start.saturating_add(length) > max_clusters {
                return Err(FsError::Runlist {
                    detail: format!(
                        "run at cluster {start} for {length} clusters runs past the \
                         volume's {max_clusters} clusters"
                    ),
                });
            }
            out.push(Extent::new(start, length));
        }
    }

    Ok(out)
}

/// Little-endian unsigned integer of 1..=8 bytes.
fn read_le_unsigned(bytes: &[u8]) -> u64 {
    let mut v = 0u64;
    for (i, &b) in bytes.iter().enumerate() {
        v |= (b as u64) << (i * 8);
    }
    v
}

/// Little-endian two's-complement signed integer of 1..=8 bytes, sign-extended
/// from the top bit of the final byte.
fn read_le_signed(bytes: &[u8]) -> i64 {
    if bytes.is_empty() {
        return 0;
    }
    let mut v = 0i64;
    for (i, &b) in bytes.iter().enumerate() {
        v |= (b as i64) << (i * 8);
    }
    // Sign-extend if the most significant byte's top bit is set.
    let bits = bytes.len() * 8;
    if bits < 64 && (bytes[bytes.len() - 1] & 0x80) != 0 {
        v |= -1i64 << bits;
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_a_single_run() {
        // header 0x21: 1-byte length, 2-byte offset
        // length 0x08, offset 0x0100 = 256
        let data = [0x21, 0x08, 0x00, 0x01, 0x00];
        let runs = decode_runlist(&data, 10_000).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].start_cluster, 256);
        assert_eq!(runs[0].cluster_count, 8);
        assert!(!runs[0].sparse);
    }

    #[test]
    fn offsets_are_relative_to_the_previous_run() {
        // Run 1: length 4 at +100. Run 2: length 4 at +50 relative => 150.
        let data = [0x11, 0x04, 100, 0x11, 0x04, 50, 0x00];
        let runs = decode_runlist(&data, 10_000).unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].start_cluster, 100);
        assert_eq!(
            runs[1].start_cluster, 150,
            "the second run is relative to the first, not absolute"
        );
    }

    /// The bug that turns a backwards fragment into an exabyte-sized cluster
    /// number.
    #[test]
    fn negative_offsets_move_backwards() {
        // Run 1 at +100, run 2 at -50 relative => cluster 50.
        //
        // Both offsets must fit in a *signed* byte: 200 in one byte would be
        // -56, not 200, which is the same trap this test exists to catch.
        let data = [0x11, 0x04, 100, 0x11, 0x04, 0xCE, 0x00]; // 0xCE = -50
        let runs = decode_runlist(&data, 10_000).unwrap();
        assert_eq!(runs[0].start_cluster, 100);
        assert_eq!(
            runs[1].start_cluster, 50,
            "0xCE must sign-extend to -50, not read as 206"
        );

        // A two-byte offset is how a genuinely large forward jump is encoded.
        let data = [0x21, 0x04, 0xC8, 0x00, 0x11, 0x04, 0x9C, 0x00];
        let runs = decode_runlist(&data, 10_000).unwrap();
        assert_eq!(runs[0].start_cluster, 200);
        assert_eq!(runs[1].start_cluster, 100, "200 - 100");
    }

    #[test]
    fn sign_extension_across_widths() {
        assert_eq!(read_le_signed(&[0xFF]), -1);
        assert_eq!(read_le_signed(&[0x80]), -128);
        assert_eq!(read_le_signed(&[0x7F]), 127);
        assert_eq!(read_le_signed(&[0x00, 0x80]), -32768);
        assert_eq!(read_le_signed(&[0xFF, 0xFF]), -1);
        assert_eq!(read_le_signed(&[0x00, 0x01]), 256);
        assert_eq!(read_le_signed(&[0x00, 0x00, 0x80]), -8_388_608);
        assert_eq!(read_le_signed(&[0xFF, 0xFF, 0xFF, 0x7F]), 2_147_483_647);
    }

    /// A sparse run must consume no clusters and must not advance the LCN.
    #[test]
    fn sparse_runs_do_not_advance_the_cluster_position() {
        // Run 1: len 4 at +100.
        // Run 2: header 0x01 => 1-byte length, 0-byte offset => sparse, len 8.
        // Run 3: len 4 at +10 relative to run 1 (not run 2) => 110.
        let data = [0x11, 0x04, 100, 0x01, 0x08, 0x11, 0x04, 10, 0x00];
        let runs = decode_runlist(&data, 10_000).unwrap();
        assert_eq!(runs.len(), 3);

        assert_eq!(runs[0].start_cluster, 100);
        assert!(runs[1].sparse, "zero-length offset field means sparse");
        assert_eq!(runs[1].cluster_count, 8);
        assert_eq!(
            runs[2].start_cluster, 110,
            "the sparse run must not have moved the running LCN"
        );
    }

    #[test]
    fn stops_at_the_terminator() {
        let data = [0x11, 0x04, 100, 0x00, 0x11, 0x04, 200];
        let runs = decode_runlist(&data, 10_000).unwrap();
        assert_eq!(runs.len(), 1, "bytes after the 0x00 terminator are ignored");
    }

    #[test]
    fn handles_an_empty_runlist() {
        assert!(decode_runlist(&[], 1000).unwrap().is_empty());
        assert!(decode_runlist(&[0x00], 1000).unwrap().is_empty());
    }

    #[test]
    fn rejects_a_run_past_the_end_of_the_volume() {
        let data = [0x21, 0xFF, 0xFF, 0xFF, 0x00];
        assert!(
            decode_runlist(&data, 100).is_err(),
            "a run beyond the volume must be rejected, not returned"
        );
    }

    #[test]
    fn rejects_a_truncated_run() {
        // Declares a 4-byte offset but only supplies two.
        let data = [0x41, 0x08, 0x00, 0x01];
        assert!(decode_runlist(&data, 10_000).is_err());
    }

    #[test]
    fn rejects_a_zero_length_run() {
        let data = [0x11, 0x00, 100];
        assert!(decode_runlist(&data, 10_000).is_err());
    }

    #[test]
    fn rejects_a_zero_width_length_field() {
        // High nibble set, low nibble zero: no length field at all.
        let data = [0x10, 0x64];
        assert!(decode_runlist(&data, 10_000).is_err());
    }

    #[test]
    fn decodes_a_realistically_fragmented_file() {
        // Seven fragments scattered forwards and backwards, which is what the
        // fragmented fixture produces.
        let data = [
            0x11, 0x08, 0x40, // 8 clusters at 64
            0x11, 0x08, 0x10, // +16 => 80
            0x11, 0x08, 0xF0, // -16 => 64? no: 80-16 = 64 .. overlapping is legal on disk
            0x21, 0x10, 0x00, 0x02, // +512 => 576
            0x00,
        ];
        let runs = decode_runlist(&data, 10_000).unwrap();
        assert_eq!(runs.len(), 4);
        assert_eq!(runs[0].start_cluster, 64);
        assert_eq!(runs[1].start_cluster, 80);
        assert_eq!(runs[2].start_cluster, 64);
        assert_eq!(runs[3].start_cluster, 576);
        assert_eq!(
            runs.iter().map(|r| r.cluster_count).sum::<u64>(),
            8 + 8 + 8 + 16
        );
    }
}
