#!/usr/bin/env python3
"""One file per carvable format, for the format-coverage fixture.

Milestone 3's acceptance criterion is "carve 20+ formats from a quick-formatted
fixture". The main corpus holds six carvable formats, chosen to be *hard* -
JPEG with restart markers, MP4 with real chunk offsets - rather than broad. This
module is the other axis: one straightforward file of every format the signature
database claims to know, so that a signature which cannot fire is a failing test
rather than a line of JSON nobody reads.

That failure mode is not hypothetical. The `vhd` entry read "connecti" for a
cookie that is actually "conectix" and could never have matched anything; it was
found by eye, not by a test. A corpus with a file per format is the guard.

Where the bytes come from
-------------------------

**Real implementations, via the standard library.** gzip, bzip2 and xz come from
zlib, libbzip2 and liblzma; WAV from `wave`; ZIP from `zipfile`; SQLite's WAL
from SQLite itself. These are foreign code and their output is evidence.

**Hand-written, from the format specs.** Everything else. These are honest about
what they prove: that the *signature* matches something shaped like the format.
For the twelve formats that also have a validator, correctness is established
separately by `make_independent.py`, whose files come from ImageMagick and
ffmpeg. A hand-written sample here plus a foreign-encoded sample there is a
better pair than either alone.

Two constraints on every sample
-------------------------------

1. **At least 8 KiB.** NTFS keeps a small file's data *inside* its MFT record
   rather than in a cluster, and a quick format discards the MFT. A resident
   file is unrecoverable by carving no matter how good the carver is, so a
   corpus of 200-byte samples would measure nothing.
2. **Deterministic.** Same bytes on every run, so the fixture's ground truth
   stays valid. No timestamps, no randomness that is not seeded.
"""

import bz2
import gzip
import io
import lzma
import os
import sqlite3
import struct
import tempfile
import wave
import zipfile
import zlib

# Every sample is padded to at least this, so NTFS stores it in clusters rather
# than resident in the MFT record. See the module docstring.
MIN_BYTES = 8192


def _filler(seed, n):
    """Deterministic pseudo-random bytes.

    Compressible-looking data would make the archive formats collapse to a few
    hundred bytes and land back inside the MFT, so this is high entropy.
    """
    out = bytearray()
    state = seed & 0xFFFFFFFF or 1
    while len(out) < n:
        state ^= (state << 13) & 0xFFFFFFFF
        state ^= state >> 17
        state ^= (state << 5) & 0xFFFFFFFF
        out += struct.pack("<I", state)
    return bytes(out[:n])


def _pad(data, seed, minimum=MIN_BYTES):
    """Grow `data` past the resident-file threshold without breaking it.

    Only safe for formats whose parsers stop at a declared length or a footer
    and ignore what follows. Formats that would be changed by trailing bytes
    build their own padding into a payload field instead.
    """
    if len(data) >= minimum:
        return data
    return data + _filler(seed, minimum - len(data))


# ---------------------------------------------------------------------------
# real implementations
# ---------------------------------------------------------------------------


def make_gzip(seed):
    # mtime=0 so the output does not change between runs.
    buf = io.BytesIO()
    with gzip.GzipFile(fileobj=buf, mode="wb", mtime=0) as fh:
        fh.write(_filler(seed, 12000))
    return buf.getvalue()


def make_bzip2(seed):
    return bz2.compress(_filler(seed, 12000))


def make_xz(seed):
    return lzma.compress(_filler(seed, 12000))


def make_wav(seed):
    """PCM WAV from the standard library, so the RIFF chunk layout is not mine.

    Must satisfy the riff_wav validator, which requires a `fmt ` and a `data`
    chunk and that the chunk sizes tile the declared RIFF size exactly.
    """
    buf = io.BytesIO()
    with wave.open(buf, "wb") as w:
        w.setnchannels(2)
        w.setsampwidth(2)
        w.setframerate(44100)
        # A simple triangle so the samples are not all identical.
        frames = bytearray()
        for i in range(6000):
            v = (i * 137) % 32767 - 16383
            frames += struct.pack("<hh", v, -v)
        w.writeframes(bytes(frames))
    return buf.getvalue()


def make_zip(seed):
    """A plain ZIP - no [Content_Types].xml, so the validator reports `zip`
    rather than refining it to an OOXML type."""
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_STORED) as z:
        for i in range(3):
            info = zipfile.ZipInfo("data/part%02d.bin" % i, date_time=(1980, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_STORED
            z.writestr(info, _filler(seed + i, 4000))
    return buf.getvalue()


def _wal_checksum(data, s0, s1, big_endian):
    """SQLite's WAL checksum, from the file-format spec.

    Two 32-bit accumulators walked over the data as pairs of words, with
    wraparound. The magic's low bit selects the word order, which is the same
    bit that makes the WAL signature need a mask.
    """
    fmt = ">II" if big_endian else "<II"
    for i in range(0, len(data), 8):
        x0, x1 = struct.unpack(fmt, data[i : i + 8])
        s0 = (s0 + x0 + s1) & 0xFFFFFFFF
        s1 = (s1 + x1 + s0) & 0xFFFFFFFF
    return s0, s1


def _pin_wal_salt(wal, salt1=0x5245_434F, salt2=0x5645_5259):
    """Replace the random salt and repair every checksum that depended on it.

    SQLite randomises Salt-1 and Salt-2 in each WAL header, and both the header
    checksum and every frame checksum are computed over them - 207 bytes of a
    49 KiB file change between runs. A corpus that is not reproducible cannot
    have ground truth recorded against it, so the salt is pinned here and the
    checksums are recomputed rather than left stale. The result is still a valid
    WAL; only the salt is ours.
    """
    page_size = struct.unpack(">I", wal[8:12])[0]
    big_endian = (struct.unpack(">I", wal[0:4])[0] & 1) == 0

    out = bytearray(wal)
    out[16:20] = struct.pack(">I", salt1)
    out[20:24] = struct.pack(">I", salt2)
    s0, s1 = _wal_checksum(bytes(out[0:24]), 0, 0, big_endian)
    out[24:28] = struct.pack(">I", s0)
    out[28:32] = struct.pack(">I", s1)

    frame = 32
    stride = 24 + page_size
    while frame + stride <= len(out):
        out[frame + 8 : frame + 12] = struct.pack(">I", salt1)
        out[frame + 12 : frame + 16] = struct.pack(">I", salt2)
        # The frame checksum chains from the previous one and covers the first
        # eight bytes of the frame header plus the whole page.
        s0, s1 = _wal_checksum(bytes(out[frame : frame + 8]), s0, s1, big_endian)
        s0, s1 = _wal_checksum(
            bytes(out[frame + 24 : frame + stride]), s0, s1, big_endian
        )
        out[frame + 16 : frame + 20] = struct.pack(">I", s0)
        out[frame + 20 : frame + 24] = struct.pack(">I", s1)
        frame += stride
    return bytes(out)


def make_sqlite_wal(seed):
    """A real write-ahead log, written by SQLite, with its salt pinned.

    Left uncheckpointed on purpose: the point is the -wal file's own header,
    whose magic selects checksum endianness in its low bit. The salt is
    replaced afterwards and the checksums recomputed - see _pin_wal_salt - so
    that the sample is reproducible, which SQLite's own output is not.
    """
    d = tempfile.mkdtemp()
    path = os.path.join(d, "wal.db")
    conn = sqlite3.connect(path)
    conn.execute("PRAGMA journal_mode=WAL")
    conn.execute("PRAGMA wal_autocheckpoint=0")
    conn.execute("CREATE TABLE messages (id INTEGER PRIMARY KEY, body TEXT)")
    for i in range(400):
        conn.execute("INSERT INTO messages (body) VALUES (?)", ("row %d %s" % (i, "x" * 60),))
    conn.commit()
    with open(path + "-wal", "rb") as fh:
        data = fh.read()
    conn.close()
    for suffix in ("", "-wal", "-shm"):
        try:
            os.remove(path + suffix)
        except OSError:
            pass
    try:
        os.rmdir(d)
    except OSError:
        pass
    return _pin_wal_salt(data)


# ---------------------------------------------------------------------------
# hand-written: images
# ---------------------------------------------------------------------------


def make_gif(seed, version=b"89a"):
    """GIF: header, logical screen descriptor, one image block, trailer 0x3B."""
    w = h = 64
    out = bytearray(b"GIF" + version)
    # Logical screen descriptor: a global colour table of 4 entries.
    out += struct.pack("<HH", w, h) + bytes([0xF1, 0x00, 0x00])
    for rgb in ((0, 0, 0), (255, 0, 0), (0, 255, 0), (0, 0, 255)):
        out += bytes(rgb)
    # Image descriptor.
    out += b"\x2C" + struct.pack("<HHHH", 0, 0, w, h) + b"\x00"
    # Uncompressed-style LZW: clear code, literals, end code. Emitting raw
    # sub-blocks of literal codes at 3 bits is legal and keeps this readable.
    out += bytes([2])  # LZW minimum code size
    payload = _filler(seed, 9000)
    for i in range(0, len(payload), 255):
        chunk = payload[i : i + 255]
        out += bytes([len(chunk)]) + chunk
    out += b"\x00"  # block terminator
    out += b"\x3B"  # trailer, which is also the footer in signatures.json
    return bytes(out)


def make_bmp(seed, width=64, height=48, bpp=24):
    """BMP with a BITMAPINFOHEADER, sized so the bmp validator's geometry check
    passes: rows pad to four bytes and the file size must cover them."""
    row = ((width * bpp + 31) // 32) * 4
    pixels = row * height
    data_offset = 14 + 40
    size = data_offset + pixels

    out = bytearray(b"BM")
    out += struct.pack("<IHHI", size, 0, 0, data_offset)
    out += struct.pack("<IiiHH", 40, width, height, 1, bpp)
    out += struct.pack("<IIiiII", 0, pixels, 2835, 2835, 0, 0)
    out += _filler(seed, pixels)
    assert len(out) == size
    return bytes(out)


def make_ico(seed):
    """ICO: a directory of one entry pointing at a BITMAPINFOHEADER image.

    The ico validator checks that the entry's byte range lies inside the file
    and outside the directory, and that the payload is a PNG or a 40-byte DIB.
    """
    # 64x64 rather than a more typical 32x32: at 32bpp that is 16 KiB of
    # pixels, which clears the resident-file threshold. A real 32x32 icon would
    # live inside the MFT record and be unrecoverable after a quick format.
    w = h = 64
    row = ((w * 32 + 31) // 32) * 4
    # An icon's DIB declares double height: colour rows then the AND mask.
    dib = bytearray(struct.pack("<IiiHH", 40, w, h * 2, 1, 32))
    dib += struct.pack("<IIiiII", 0, 0, 0, 0, 0, 0)
    dib += _filler(seed, row * h)
    dib += b"\x00" * (((w + 31) // 32) * 4 * h)

    header = struct.pack("<HHH", 0, 1, 1)
    entry = struct.pack("<BBBBHHII", w, h, 0, 0, 1, 32, len(dib), 6 + 16)
    return bytes(header + entry + bytes(dib))


def _tiff(seed, byteorder):
    """A minimal but structurally real TIFF: header, one IFD, strip data."""
    e = "<" if byteorder == b"II" else ">"
    # 128x128 at 8 bits is 16 KiB of strip data, past the resident threshold.
    w = h = 128
    strip = _filler(seed, w * h)
    ifd_at = 8
    entries = [
        (256, 3, 1, w),        # ImageWidth
        (257, 3, 1, h),        # ImageLength
        (258, 3, 1, 8),        # BitsPerSample
        (259, 3, 1, 1),        # Compression: none
        (262, 3, 1, 1),        # PhotometricInterpretation: black is zero
        (273, 4, 1, 0),        # StripOffsets - patched below
        (278, 3, 1, h),        # RowsPerStrip
        (279, 4, 1, len(strip)),  # StripByteCounts
    ]
    ifd_size = 2 + 12 * len(entries) + 4
    data_at = ifd_at + ifd_size
    entries = [(t, ty, n, data_at if t == 273 else v) for (t, ty, n, v) in entries]

    out = bytearray(byteorder + struct.pack(e + "HI", 42, ifd_at))
    out += struct.pack(e + "H", len(entries))
    for tag, typ, count, value in entries:
        out += struct.pack(e + "HHI", tag, typ, count)
        # A SHORT is written in the first two bytes of the value field.
        out += struct.pack(e + "H", value) + b"\x00\x00" if typ == 3 else struct.pack(e + "I", value)
    out += struct.pack(e + "I", 0)  # no next IFD
    out += strip
    return bytes(out)


def make_tiff_le(seed):
    return _tiff(seed, b"II")


def make_tiff_be(seed):
    return _tiff(seed, b"MM")


def make_webp(seed):
    """RIFF/WEBP carrying a VP8L chunk, which is one of the three bitstreams
    the riff_webp validator accepts."""
    payload = b"\x2F" + _filler(seed, 9000)
    if len(payload) % 2:
        payload += b"\x00"
    body = b"WEBP" + b"VP8L" + struct.pack("<I", len(payload)) + payload
    return b"RIFF" + struct.pack("<I", len(body)) + body


def make_psd(seed):
    """Photoshop: '8BPS', version 1, then the colour mode, resource and layer
    sections, each length-prefixed."""
    w = h = 64
    out = bytearray(b"8BPS" + struct.pack(">H", 1) + b"\x00" * 6)
    out += struct.pack(">HIIHH", 3, h, w, 8, 3)  # channels, rows, cols, depth, mode
    out += struct.pack(">I", 0)  # colour mode data
    out += struct.pack(">I", 0)  # image resources
    out += struct.pack(">I", 0)  # layer and mask info
    out += struct.pack(">H", 0)  # compression: raw
    out += _filler(seed, w * h * 3)
    return bytes(out)


# ---------------------------------------------------------------------------
# hand-written: audio and video containers
# ---------------------------------------------------------------------------


def make_avi(seed):
    """RIFF/AVI with the LIST hdrl and LIST movi the riff_avi validator wants."""

    def chunk(cid, data):
        out = cid + struct.pack("<I", len(data)) + data
        return out + b"\x00" if len(data) % 2 else out

    avih = struct.pack("<IIIIIIIIIIIIIIII",
                       33333, 1000, 0, 0x10, 30, 0, 1, 0, 64, 48, 0, 0, 0, 0, 0, 0)
    hdrl = b"hdrl" + chunk(b"avih", avih)
    movi = b"movi" + chunk(b"00db", _filler(seed, 9000))
    body = b"AVI " + chunk(b"LIST", hdrl) + chunk(b"LIST", movi)
    return b"RIFF" + struct.pack("<I", len(body)) + body


def make_aiff(seed):
    """FORM/AIFF with COMM and SSND, the mirror of WAV in big-endian."""

    def chunk(cid, data):
        out = cid + struct.pack(">I", len(data)) + data
        return out + b"\x00" if len(data) % 2 else out

    # 80-bit extended float for 44100 Hz.
    rate = b"\x40\x0e\xac\x44\x00\x00\x00\x00\x00\x00"
    frames = _filler(seed, 9000)
    comm = struct.pack(">hIh", 1, len(frames) // 2, 16) + rate
    ssnd = struct.pack(">II", 0, 0) + frames
    body = b"AIFF" + chunk(b"COMM", comm) + chunk(b"SSND", ssnd)
    return b"FORM" + struct.pack(">I", len(body)) + body


def make_flac(seed):
    """'fLaC' then a STREAMINFO metadata block, then one frame's worth of
    padding so the file is not resident."""
    info = bytearray()
    info += struct.pack(">HH", 4096, 4096)          # min/max block size
    info += b"\x00\x10\x00" + b"\x00\x20\x00"        # min/max frame size
    # 20 bits sample rate, 3 bits channels-1, 5 bits bits-per-sample-1, 36 bits
    # total samples, packed together.
    info += bytes([0x0A, 0xC4, 0x42, 0xF0, 0x00, 0x00, 0x27, 0x10])
    info += b"\x00" * 16                             # MD5 of the unencoded audio
    out = bytearray(b"fLaC")
    out += bytes([0x00]) + struct.pack(">I", len(info))[1:]  # last-block=0, type=0
    out += info
    padding = _filler(seed, 9000)
    out += bytes([0x81]) + struct.pack(">I", len(padding))[1:]  # last-block, PADDING
    out += padding
    return bytes(out)


def make_ogg(seed):
    """A sequence of OggS pages. Each page's segment table has to add up, which
    is the part a naive generator gets wrong."""
    out = bytearray()
    payload = _filler(seed, 9000)
    seq = 0
    pos = 0
    while pos < len(payload):
        body = payload[pos : pos + 4096]
        pos += len(body)
        segments = []
        remaining = len(body)
        while remaining >= 255:
            segments.append(255)
            remaining -= 255
        segments.append(remaining)
        header_type = 0x02 if seq == 0 else (0x04 if pos >= len(payload) else 0x00)
        page = bytearray(b"OggS")
        page += bytes([0, header_type])
        page += struct.pack("<q", seq * 1024)   # granule position
        page += struct.pack("<III", 0x5243_4F52, seq, 0)  # serial, seq, crc placeholder
        page += bytes([len(segments)]) + bytes(segments)
        page += body
        # The CRC is computed with the field zeroed, which it already is.
        crc = zlib.crc32(bytes(page)) & 0xFFFFFFFF
        page[22:26] = struct.pack("<I", crc)
        out += page
        seq += 1
    return bytes(out)


def make_mp3_id3(seed):
    """ID3v2 tag followed by MPEG audio frames.

    The tag size is a syncsafe integer - seven bits per byte - which is the
    detail that makes hand-written ID3 tags usually wrong.
    """
    frame_body = b"\x00" + b"RECOVERY-CORE test tone\x00"
    frame = b"TIT2" + struct.pack(">I", len(frame_body)) + b"\x00\x00" + frame_body
    tag_size = len(frame)
    syncsafe = bytes([(tag_size >> 21) & 0x7F, (tag_size >> 14) & 0x7F,
                      (tag_size >> 7) & 0x7F, tag_size & 0x7F])
    out = bytearray(b"ID3" + bytes([3, 0, 0]) + syncsafe + frame)
    # MPEG-1 Layer III, 128 kbps, 44.1 kHz: 417-byte frames.
    for _ in range(24):
        out += b"\xFF\xFB\x90\x00" + _filler(seed, 413)
        seed += 1
    return bytes(out)


def make_mkv(seed):
    """EBML header then a Segment element with unknown size, which is how a
    streamed Matroska file is actually written."""
    def elem(eid, payload):
        n = len(payload)
        if n < 0x7F:
            size = bytes([0x80 | n])
        elif n < 0x3FFF:
            size = struct.pack(">H", 0x4000 | n)
        else:
            size = struct.pack(">I", 0x1000_0000 | n)
        return eid + size + payload

    ebml = (elem(b"\x42\x86", b"\x01")              # EBMLVersion
            + elem(b"\x42\xF7", b"\x01")            # EBMLReadVersion
            + elem(b"\x42\x82", b"matroska")        # DocType
            + elem(b"\x42\x87", b"\x04"))           # DocTypeVersion
    out = bytearray(elem(b"\x1A\x45\xDF\xA3", ebml))
    out += b"\x18\x53\x80\x67" + b"\x01\xFF\xFF\xFF\xFF\xFF\xFF\xFF"  # Segment, unknown size
    out += elem(b"\xA3", _filler(seed, 9000))       # SimpleBlock
    return bytes(out)


def make_mpeg_ps(seed):
    """MPEG program stream: a pack header followed by a PES packet."""
    out = bytearray(b"\x00\x00\x01\xBA")
    out += bytes([0x44, 0x00, 0x04, 0x00, 0x04, 0x01])  # SCR and mux rate
    out += bytes([0x01, 0x89, 0xC3, 0xF8])
    payload = _filler(seed, 2000)
    for i in range(0, len(payload), 1000):
        chunk = payload[i : i + 1000]
        out += b"\x00\x00\x01\xE0" + struct.pack(">H", len(chunk) + 3)
        out += bytes([0x80, 0x00, 0x00]) + chunk
    return _pad(bytes(out), seed)


def make_flv(seed):
    """FLV header plus tags, each followed by its own back-pointer."""
    out = bytearray(b"FLV" + bytes([1, 0x05]) + struct.pack(">I", 9))
    out += struct.pack(">I", 0)  # first back-pointer
    payload = _filler(seed, 9000)
    for i in range(0, len(payload), 3000):
        body = payload[i : i + 3000]
        tag = bytes([9]) + struct.pack(">I", len(body))[1:] + struct.pack(">I", 0)[1:]
        tag += bytes([0, 0, 0, 0]) + body
        out += tag + struct.pack(">I", len(tag))
    return bytes(out)


def make_asf_wmv(seed):
    """ASF header object: a 16-byte GUID, a 64-bit size, then child objects."""
    header_guid = bytes.fromhex("3026B2758E66CF11A6D900AA0062CE6C")
    child_guid = bytes.fromhex("B503BF5F2EA9CF118EE300C00C205365")  # File Properties
    child_body = _filler(seed, 9000)
    child = child_guid + struct.pack("<Q", 24 + len(child_body)) + child_body
    total = 30 + len(child)
    return header_guid + struct.pack("<QI", total, 1) + bytes([1, 2]) + child


def _mp4_like(seed, brand):
    """ISO base media file: ftyp, moov, mdat. Must satisfy the mp4 validator,
    which requires both moov and mdat and bounds the ftyp box."""
    def box(kind, payload):
        return struct.pack(">I", len(payload) + 8) + kind + payload

    ftyp = box(b"ftyp", brand + struct.pack(">I", 512) + brand + b"isom")
    moov = box(b"moov", box(b"mvhd", _filler(seed, 96)))
    mdat = box(b"mdat", _filler(seed + 1, 9000))
    return ftyp + moov + mdat


def make_mp4(seed):
    return _mp4_like(seed, b"isom")


def make_m4a(seed):
    return _mp4_like(seed, b"M4A ")


def make_heic(seed):
    return _mp4_like(seed, b"heic")


# ---------------------------------------------------------------------------
# hand-written: documents, archives, system
# ---------------------------------------------------------------------------


def make_rtf(seed):
    body = ["{\\rtf1\\ansi\\deff0{\\fonttbl{\\f0 Courier;}}"]
    for i in range(220):
        body.append("\\par Recovered paragraph %04d: the quick brown fox." % i)
    body.append("}")
    return "".join(body).encode("ascii")


def make_eml(seed):
    lines = [
        "From: recovery@example.invalid",
        "To: owner@example.invalid",
        "Subject: format coverage sample",
        "Date: Thu, 01 Jan 1970 00:00:00 +0000",
        "MIME-Version: 1.0",
        'Content-Type: text/plain; charset="utf-8"',
        "",
    ]
    for i in range(200):
        lines.append("Line %04d of a message body kept deliberately dull." % i)
    return ("\r\n".join(lines) + "\r\n").encode("utf-8")


def make_ole2(seed):
    """Compound File Binary Format: the header is a fixed 512-byte structure
    with a well-known 8-byte magic."""
    out = bytearray(bytes.fromhex("D0CF11E0A1B11AE1"))
    out += b"\x00" * 16                       # CLSID
    out += struct.pack("<HHHH", 0x3E, 3, 0xFFFE, 9)   # versions, byte order, sector shift
    out += struct.pack("<HHI", 6, 0, 0)       # mini sector shift, reserved
    out += struct.pack("<IIII", 0, 1, 0xFFFFFFFE, 0)  # directory sectors, FAT count
    out += struct.pack("<IIII", 0, 0x1000, 0xFFFFFFFE, 0)
    out += b"\xFF\xFF\xFF\xFF" * 109          # DIFAT
    out += _filler(seed, 512 * 20)
    return bytes(out)


def make_rar(seed):
    """RAR 4.x: the 7-byte marker block, then a padded body."""
    return _pad(bytes.fromhex("526172211A0700") + _filler(seed, 9000), seed)


def make_rar5(seed):
    return _pad(bytes.fromhex("526172211A070100") + _filler(seed, 9000), seed)


def make_7z(seed):
    """7z: signature, version, a CRC over the start header, then the header."""
    start = struct.pack("<QQI", 32, 9000, 0)
    crc = zlib.crc32(start) & 0xFFFFFFFF
    out = bytes.fromhex("377ABCAF271C") + bytes([0, 4])
    out += struct.pack("<I", crc) + start + _filler(seed, 9000)
    return out


def make_cab(seed):
    """MSCF header: signature, reserved, cabinet size, offset of the first
    CFFILE, versions and counts."""
    body = _filler(seed, 9000)
    total = 36 + len(body)
    out = bytearray(b"MSCF" + struct.pack("<I", 0))
    out += struct.pack("<II", total, 0)
    out += struct.pack("<II", 36, 0)
    out += bytes([3, 1])                       # version 1.3
    out += struct.pack("<HHHH", 1, 1, 0, 0)    # folders, files, flags, set id
    out += struct.pack("<H", 0)
    out += body
    return bytes(out)


def make_elf(seed):
    """ELF64 little-endian header plus a program header and payload."""
    out = bytearray(b"\x7FELF")
    out += bytes([2, 1, 1, 0]) + b"\x00" * 8   # 64-bit, LE, version 1, System V
    out += struct.pack("<HHI", 2, 0x3E, 1)     # ET_EXEC, x86-64, version
    out += struct.pack("<QQQ", 0x400000, 64, 0)
    out += struct.pack("<IHHHHHH", 0, 64, 56, 1, 64, 0, 0)
    out += struct.pack("<IIQQQQQQ", 1, 5, 0, 0x400000, 0x400000, 9000, 9000, 0x1000)
    return _pad(bytes(out), seed)


def make_exe_pe(seed):
    """A PE the `pe` validator accepts: DOS stub, e_lfanew, PE signature, COFF
    header, optional header and a section table whose ranges exist."""
    lfanew = 0x80
    opt_size = 240                      # PE32+
    sections = 2
    table = lfanew + 4 + 20 + opt_size
    data_at = table + 40 * sections

    out = bytearray(b"\x00" * data_at)
    out[0:2] = b"MZ"
    out[0x3C:0x40] = struct.pack("<I", lfanew)
    out[lfanew : lfanew + 4] = b"PE\x00\x00"
    coff = lfanew + 4
    out[coff : coff + 2] = struct.pack("<H", 0x8664)
    out[coff + 2 : coff + 4] = struct.pack("<H", sections)
    out[coff + 16 : coff + 18] = struct.pack("<H", opt_size)
    opt = coff + 20
    out[opt : opt + 2] = struct.pack("<H", 0x20B)
    out[opt + 108 : opt + 112] = struct.pack("<I", 16)   # NumberOfRvaAndSizes

    offset = data_at
    for i in range(sections):
        at = table + i * 40
        name = (b".text\x00\x00\x00" if i == 0 else b".rdata\x00\x00")
        out[at : at + 8] = name
        size = 4608
        out[at + 16 : at + 20] = struct.pack("<I", size)
        out[at + 20 : at + 24] = struct.pack("<I", offset)
        offset += size
    out += _filler(seed, offset - len(out))
    return bytes(out)


def make_evtx(seed):
    """Windows event log: 'ElfFile\\0' then the file header's counters."""
    out = bytearray(b"ElfFile\x00")
    out += struct.pack("<QQQ", 0, 1, 1)       # first/last chunk, next record id
    out += struct.pack("<IHHHH", 128, 1, 3, 1, 1)
    out += b"\x00" * 76
    out += struct.pack("<I", 0)               # checksum placeholder
    return _pad(bytes(out), seed)


def make_reg_hive(seed):
    """Registry hive: 'regf', sequence numbers, then the base block."""
    out = bytearray(b"regf")
    out += struct.pack("<II", 1, 1)           # primary and secondary sequence
    out += struct.pack("<Q", 0)               # last written timestamp
    out += struct.pack("<IIII", 1, 3, 0, 1)   # major, minor, file type, format
    out += struct.pack("<II", 0x20, 0x1000)   # root cell offset, hive bins size
    out += b"\x00" * 64
    return _pad(bytes(out), seed)


def make_vhd(seed):
    """A dynamic VHD, which is the only kind a header-anchored signature can
    find: it keeps a copy of the footer at offset 0. A fixed VHD carries the
    'conectix' cookie in its last 512 bytes only. See the note on the `vhd`
    entry in signatures.json."""
    footer = bytearray(b"conectix")
    footer += struct.pack(">II", 2, 0x00010000)      # features, format version
    footer += struct.pack(">Q", 512)                 # data offset: dynamic
    footer += struct.pack(">I", 0)                   # timestamp, fixed at the epoch
    footer += b"win " + struct.pack(">I", 0x00050003)
    footer += b"Wi2k"
    footer += struct.pack(">QQ", 16 << 20, 16 << 20)
    footer += struct.pack(">HBB", 512, 16, 63)       # cylinders, heads, sectors
    footer += struct.pack(">I", 3)                   # disk type: dynamic
    footer += struct.pack(">I", 0)                   # checksum placeholder
    footer += b"\x00" * 16 + b"\x00"
    footer += b"\x00" * (512 - len(footer))
    # A dynamic VHD is footer-copy, sparse header, then the BAT.
    return bytes(footer) + b"cxsparse" + _filler(seed, 9000)


# ---------------------------------------------------------------------------
# the set
# ---------------------------------------------------------------------------

# (relative path, kind, builder). `kind` matches the signature id so a test can
# line the two up without a second mapping to keep in step.
FORMAT_ITEMS = [
    ("formats/sample.gz", "gzip", lambda: make_gzip(11)),
    ("formats/sample.bz2", "bzip2", lambda: make_bzip2(12)),
    ("formats/sample.xz", "xz", lambda: make_xz(13)),
    ("formats/sample.wav", "wav", lambda: make_wav(14)),
    ("formats/sample.zip", "zip", lambda: make_zip(15)),
    ("formats/sample.db-wal", "sqlite_wal", lambda: make_sqlite_wal(16)),
    ("formats/sample89.gif", "gif", lambda: make_gif(17, b"89a")),
    ("formats/sample87.gif", "gif87", lambda: make_gif(18, b"87a")),
    ("formats/sample.bmp", "bmp", lambda: make_bmp(19)),
    ("formats/sample.ico", "ico", lambda: make_ico(20)),
    ("formats/sample_le.tif", "tiff_le", lambda: make_tiff_le(21)),
    ("formats/sample_be.tif", "tiff_be", lambda: make_tiff_be(22)),
    ("formats/sample.webp", "webp", lambda: make_webp(23)),
    ("formats/sample.psd", "psd", lambda: make_psd(24)),
    ("formats/sample.avi", "avi", lambda: make_avi(25)),
    ("formats/sample.aiff", "aiff", lambda: make_aiff(26)),
    ("formats/sample.flac", "flac", lambda: make_flac(27)),
    ("formats/sample.ogg", "ogg", lambda: make_ogg(28)),
    ("formats/sample.mp3", "mp3_id3", lambda: make_mp3_id3(29)),
    ("formats/sample.mkv", "mkv", lambda: make_mkv(30)),
    ("formats/sample.mpg", "mpeg_ps", lambda: make_mpeg_ps(31)),
    ("formats/sample.flv", "flv", lambda: make_flv(32)),
    ("formats/sample.wmv", "asf_wmv", lambda: make_asf_wmv(33)),
    ("formats/sample.mp4", "mp4", lambda: make_mp4(34)),
    ("formats/sample.m4a", "m4a", lambda: make_m4a(35)),
    ("formats/sample.heic", "heic", lambda: make_heic(36)),
    ("formats/sample.rtf", "rtf", lambda: make_rtf(37)),
    ("formats/sample.eml", "eml", lambda: make_eml(38)),
    ("formats/sample.doc", "ole2", lambda: make_ole2(39)),
    ("formats/sample4.rar", "rar", lambda: make_rar(40)),
    ("formats/sample5.rar", "rar5", lambda: make_rar5(41)),
    ("formats/sample.7z", "7z", lambda: make_7z(42)),
    ("formats/sample.cab", "cab", lambda: make_cab(43)),
    ("formats/sample.elf", "elf", lambda: make_elf(44)),
    ("formats/sample.exe", "exe_pe", lambda: make_exe_pe(45)),
    ("formats/sample.evtx", "evtx", lambda: make_evtx(46)),
    ("formats/sample.hiv", "reg_hive", lambda: make_reg_hive(47)),
    ("formats/sample.vhd", "vhd", lambda: make_vhd(48)),
]


def self_check():
    """Every sample must be non-resident and start with its signature.

    Run by `make_corpus.py --check-formats`. A sample that is too small is
    unrecoverable after a quick format however good the carver is, and a sample
    that does not start with its own signature makes the fixture prove nothing
    about the format it is named for.
    """
    import json

    sigs = {}
    here = os.path.dirname(os.path.abspath(__file__))
    db = os.path.join(here, "..", "..", "crates", "rc-carve", "signatures.json")
    with open(db, encoding="utf-8") as fh:
        for s in json.load(fh)["signatures"]:
            sigs[s["id"]] = (
                bytes.fromhex(s["header"]),
                s.get("header_off", 0),
                bytes.fromhex(s["header_mask"]) if s.get("header_mask") else None,
            )

    problems = []
    for path, kind, build in FORMAT_ITEMS:
        data = build()
        # Build it twice. A sample that differs between runs cannot have ground
        # truth recorded against it, and the failure is silent: the fixture is
        # written from one run and every later check compares against another.
        # SQLite's WAL was exactly this - a random salt, and 207 bytes of a
        # 49 KiB file moving underneath it.
        if build() != data:
            problems.append("%s is not reproducible: two builds differ" % path)
        if len(data) < MIN_BYTES:
            problems.append(
                "%s is %d bytes, under the %d-byte resident-file threshold"
                % (path, len(data), MIN_BYTES)
            )
        if kind not in sigs:
            problems.append("%s: no signature with id %r" % (path, kind))
            continue
        header, off, mask = sigs[kind]
        got = data[off : off + len(header)]
        if mask:
            ok = all((g & m) == (h & m) for g, h, m in zip(got, header, mask))
        else:
            ok = got == header
        if not ok:
            problems.append(
                "%s does not start with the %s signature: got %s, want %s at offset %d"
                % (path, kind, got.hex(), header.hex(), off)
            )
    return problems
