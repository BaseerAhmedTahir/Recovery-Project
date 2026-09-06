#!/usr/bin/env python3
"""Deterministic test-corpus generator for RECOVERY-CORE.

Writes a fixed set of structurally valid files into OUTDIR and prints a JSON
manifest of {relpath: {sha256, size, kind}} to stdout.

Everything here is built from the Python standard library on purpose:

  * The WSL2 distro backing fixture generation is stored on this machine's C:
    drive, which is effectively full, so installing ffmpeg or Pillow is not
    safe.
  * Hand-building the container formats gives exact control over internal
    structure (JPEG restart intervals, MP4 chunk offsets, ZIP central
    directory), which is what the Milestone 3 validators and the Milestone 4
    fragment-reassembly heuristics actually need to be tested against.

Reproducibility: every source of entropy is a seeded PRNG and every embedded
timestamp is pinned, so re-running this on the same machine reproduces every
file byte for byte. The one exception is the SQLite database, whose page layout
depends on the SQLite library compiled into the running Python -- the same
script under Windows Python and under WSL Python produces different bytes. That
is harmless, because the fixture's expected.json records hashes taken from the
bytes actually written during that build, so ground truth is always
self-consistent. Do not compare hashes across machines.
"""

import binascii
import hashlib
import io
import json
import math
import os
import random
import sqlite3
import struct
import sys
import tempfile
import unicodedata
import zipfile
import zlib

# Pinned so archive members and database pages do not drift between runs.
DOS_EPOCH = (2024, 1, 1, 0, 0, 0)


# ---------------------------------------------------------------------------
# PNG
# ---------------------------------------------------------------------------

def _png_chunk(tag, payload):
    return (struct.pack(">I", len(payload)) + tag + payload
            + struct.pack(">I", binascii.crc32(tag + payload) & 0xFFFFFFFF))


def make_png(width, height, seed):
    """8-bit RGB PNG with correct per-chunk CRCs (exercises the PNG validator)."""
    rng = random.Random(seed)
    raw = bytearray()
    for y in range(height):
        raw.append(0)  # filter type 0 (None) keeps the validator's job simple
        for x in range(width):
            raw.append((x * 7 + seed) & 0xFF)
            raw.append((y * 5 + seed) & 0xFF)
            raw.append(rng.randrange(256))
    return (b"\x89PNG\r\n\x1a\n"
            + _png_chunk(b"IHDR",
                         struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
            + _png_chunk(b"IDAT", zlib.compress(bytes(raw), 6))
            + _png_chunk(b"IEND", b""))


# ---------------------------------------------------------------------------
# JPEG - baseline, grayscale, standard Annex K tables, restart markers
# ---------------------------------------------------------------------------

ZIGZAG = [
    0, 1, 8, 16, 9, 2, 3, 10,
    17, 24, 32, 25, 18, 11, 4, 5,
    12, 19, 26, 33, 40, 48, 41, 34,
    27, 20, 13, 6, 7, 14, 21, 28,
    35, 42, 49, 56, 57, 50, 43, 36,
    29, 22, 15, 23, 30, 37, 44, 51,
    58, 59, 52, 45, 38, 31, 39, 46,
    53, 60, 61, 54, 47, 55, 62, 63,
]

QUANT_LUMA = [
    16, 11, 10, 16, 24, 40, 51, 61,
    12, 12, 14, 19, 26, 58, 60, 55,
    14, 13, 16, 24, 40, 57, 69, 56,
    14, 17, 22, 29, 51, 87, 80, 62,
    18, 22, 37, 56, 68, 109, 103, 77,
    24, 35, 55, 64, 81, 104, 113, 92,
    49, 64, 78, 87, 103, 121, 120, 101,
    72, 92, 95, 98, 112, 100, 103, 99,
]

DC_BITS = [0, 0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0]
DC_VALS = list(range(12))

AC_BITS = [0, 0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7D]
AC_VALS = [
    0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12,
    0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07,
    0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xA1, 0x08,
    0x23, 0x42, 0xB1, 0xC1, 0x15, 0x52, 0xD1, 0xF0,
    0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0A, 0x16,
    0x17, 0x18, 0x19, 0x1A, 0x25, 0x26, 0x27, 0x28,
    0x29, 0x2A, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39,
    0x3A, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49,
    0x4A, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59,
    0x5A, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69,
    0x6A, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79,
    0x7A, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89,
    0x8A, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98,
    0x99, 0x9A, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7,
    0xA8, 0xA9, 0xAA, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6,
    0xB7, 0xB8, 0xB9, 0xBA, 0xC2, 0xC3, 0xC4, 0xC5,
    0xC6, 0xC7, 0xC8, 0xC9, 0xCA, 0xD2, 0xD3, 0xD4,
    0xD5, 0xD6, 0xD7, 0xD8, 0xD9, 0xDA, 0xE1, 0xE2,
    0xE3, 0xE4, 0xE5, 0xE6, 0xE7, 0xE8, 0xE9, 0xEA,
    0xF1, 0xF2, 0xF3, 0xF4, 0xF5, 0xF6, 0xF7, 0xF8,
    0xF9, 0xFA,
]


def _huff_table(bits, vals):
    """Expand a JPEG BITS/HUFFVAL pair into {symbol: (code, length)}."""
    table = {}
    code = 0
    k = 0
    for length in range(1, 17):
        for _ in range(bits[length]):
            table[vals[k]] = (code, length)
            code += 1
            k += 1
        code <<= 1
    return table


DC_TABLE = _huff_table(DC_BITS, DC_VALS)
AC_TABLE = _huff_table(AC_BITS, AC_VALS)

# Separable 1-D DCT basis. The corpus is small enough that a naive per-block
# transform is not worth optimising.
_COS = [[math.cos((2 * x + 1) * u * math.pi / 16)
         * (math.sqrt(0.5) if u == 0 else 1.0)
         for u in range(8)] for x in range(8)]


def _fdct(block):
    tmp = [[0.0] * 8 for _ in range(8)]
    for y in range(8):
        row = block[y]
        for u in range(8):
            tmp[y][u] = sum(row[x] * _COS[x][u] for x in range(8))
    out = [0.0] * 64
    for u in range(8):
        for v in range(8):
            out[v * 8 + u] = 0.25 * sum(tmp[y][u] * _COS[y][v] for y in range(8))
    return out


class _BitWriter:
    """MSB-first bit packer with JPEG 0xFF byte stuffing."""

    def __init__(self):
        self.buf = bytearray()
        self.acc = 0
        self.n = 0

    def write(self, code, length):
        for i in range(length - 1, -1, -1):
            self.acc = (self.acc << 1) | ((code >> i) & 1)
            self.n += 1
            if self.n == 8:
                self.buf.append(self.acc)
                if self.acc == 0xFF:
                    self.buf.append(0x00)
                self.acc = 0
                self.n = 0

    def flush_to_byte(self):
        """Pad with 1-bits to the next byte boundary (required before RSTn)."""
        while self.n:
            self.write(1, 1)


def _category(v):
    a = abs(v)
    c = 0
    while a:
        c += 1
        a >>= 1
    return c


def make_jpeg(width, height, seed, restart_interval=4):
    """Baseline grayscale JPEG.

    A non-zero restart_interval emits DRI plus RST0..RST7 markers, which the
    Milestone 4 decode-until-failure heuristic relies on to resynchronise
    after a fragment boundary.
    """
    rng = random.Random(seed)
    mcux = (width + 7) // 8
    mcuy = (height + 7) // 8

    # Structured gradient plus noise: compresses realistically rather than
    # collapsing to a near-empty entropy stream.
    pix = [[(x * 3 + y * 2 + rng.randrange(64)) & 0xFF
            for x in range(mcux * 8)] for y in range(mcuy * 8)]

    out = bytearray(b"\xFF\xD8")
    out += (b"\xFF\xE0" + struct.pack(">H", 16) + b"JFIF\x00\x01\x01\x00"
            + struct.pack(">HH", 1, 1) + b"\x00\x00")
    out += (b"\xFF\xDB" + struct.pack(">H", 67) + b"\x00"
            + bytes(QUANT_LUMA[ZIGZAG[i]] for i in range(64)))
    out += (b"\xFF\xC0" + struct.pack(">H", 11) + b"\x08"
            + struct.pack(">HH", height, width) + b"\x01\x01\x11\x00")
    out += (b"\xFF\xC4" + struct.pack(">H", 2 + 1 + 16 + len(DC_VALS)) + b"\x00"
            + bytes(DC_BITS[1:]) + bytes(DC_VALS))
    out += (b"\xFF\xC4" + struct.pack(">H", 2 + 1 + 16 + len(AC_VALS)) + b"\x10"
            + bytes(AC_BITS[1:]) + bytes(AC_VALS))
    if restart_interval:
        out += b"\xFF\xDD" + struct.pack(">HH", 4, restart_interval)
    out += b"\xFF\xDA" + struct.pack(">H", 8) + b"\x01\x01\x00\x00\x3F\x00"

    bw = _BitWriter()
    prev_dc = 0
    rst = 0
    mcu_count = 0

    for by in range(mcuy):
        for bx in range(mcux):
            if restart_interval and mcu_count and mcu_count % restart_interval == 0:
                bw.flush_to_byte()
                out += bw.buf
                bw = _BitWriter()
                out += bytes((0xFF, 0xD0 + rst))
                rst = (rst + 1) & 7
                prev_dc = 0  # DC predictor resets at every restart marker

            block = [[pix[by * 8 + y][bx * 8 + x] - 128 for x in range(8)]
                     for y in range(8)]
            coef = _fdct(block)
            q = [int(round(coef[ZIGZAG[i]] / QUANT_LUMA[ZIGZAG[i]]))
                 for i in range(64)]

            diff = q[0] - prev_dc
            prev_dc = q[0]
            cat = _category(diff)
            code, length = DC_TABLE[cat]
            bw.write(code, length)
            if cat:
                bw.write(diff - 1 if diff < 0 else diff, cat)

            last = 0
            for i in range(63, 0, -1):
                if q[i]:
                    last = i
                    break
            run = 0
            for i in range(1, last + 1):
                if q[i] == 0:
                    run += 1
                    continue
                while run > 15:
                    c, l = AC_TABLE[0xF0]  # ZRL
                    bw.write(c, l)
                    run -= 16
                cat = _category(q[i])
                c, l = AC_TABLE[(run << 4) | cat]
                bw.write(c, l)
                bw.write(q[i] - 1 if q[i] < 0 else q[i], cat)
                run = 0
            if last < 63:
                c, l = AC_TABLE[0x00]  # EOB
                bw.write(c, l)

            mcu_count += 1

    bw.flush_to_byte()
    out += bw.buf
    out += b"\xFF\xD9"
    return bytes(out)


# ---------------------------------------------------------------------------
# PDF
# ---------------------------------------------------------------------------

def make_pdf(seed, pages=3):
    """Minimal but structurally complete PDF: object table, xref, trailer, EOF."""
    rng = random.Random(seed)
    objects = []

    kids = " ".join("%d 0 R" % (3 + i * 2) for i in range(pages))
    objects.append(b"<< /Type /Catalog /Pages 2 0 R >>")
    objects.append(("<< /Type /Pages /Count %d /Kids [%s] >>"
                    % (pages, kids)).encode())

    for i in range(pages):
        content_obj = 4 + i * 2
        objects.append(("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] "
                        "/Contents %d 0 R /Resources << /Font << /F1 %d 0 R >> "
                        ">> >>" % (content_obj, 3 + pages * 2)).encode())
        text = "Page %d of the RECOVERY-CORE corpus. %s" % (
            i + 1, "".join(rng.choice("abcdefghijklmnopqrstuvwxyz ")
                           for _ in range(400)))
        stream = ("BT /F1 12 Tf 72 720 Td (%s) Tj ET" % text).encode()
        objects.append(b"<< /Length " + str(len(stream)).encode() + b" >>\nstream\n"
                       + stream + b"\nendstream")

    objects.append(b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>")

    out = bytearray(b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n")
    offsets = []
    for i, body in enumerate(objects, start=1):
        offsets.append(len(out))
        out += ("%d 0 obj\n" % i).encode() + body + b"\nendobj\n"

    xref_pos = len(out)
    n = len(objects) + 1
    out += ("xref\n0 %d\n" % n).encode()
    out += b"0000000000 65535 f \n"
    for off in offsets:
        out += ("%010d 00000 n \n" % off).encode()
    out += ("trailer\n<< /Size %d /Root 1 0 R >>\nstartxref\n%d\n"
            % (n, xref_pos)).encode()
    out += b"%%EOF\n"
    return bytes(out)


# ---------------------------------------------------------------------------
# MP4 - real atom tree with a populated sample table
# ---------------------------------------------------------------------------

def _box(tag, payload):
    return struct.pack(">I", len(payload) + 8) + tag + payload


def _full_box(tag, version, flags, payload):
    return _box(tag, struct.pack(">B", version) + flags + payload)


def make_mp4(seed, chunks=12, samples_per_chunk=4, sample_size=1024):
    """MP4 with a coherent moov/mdat pair.

    The stco chunk offsets are patched to point at the real byte positions of
    each chunk inside mdat, so the Milestone 4 format-driven pointer-following
    reassembler has genuine offsets to chase rather than a placeholder tree.
    """
    rng = random.Random(seed)
    n_samples = chunks * samples_per_chunk
    timescale = 1000
    duration = n_samples * 40  # 25 fps at a 1 ms timescale

    media = bytearray()
    chunk_rel_offsets = []
    for _ in range(chunks):
        chunk_rel_offsets.append(len(media))
        for _ in range(samples_per_chunk):
            media += bytes(rng.randrange(256) for _ in range(sample_size))

    ftyp = _box(b"ftyp", b"isom" + struct.pack(">I", 0x200)
                + b"isom" + b"iso2" + b"avc1" + b"mp41")

    mvhd = _full_box(b"mvhd", 0, b"\x00\x00\x00",
                     struct.pack(">IIII", 0, 0, timescale, duration)
                     + struct.pack(">i", 0x00010000)
                     + struct.pack(">h", 0x0100)
                     + b"\x00" * 10
                     + struct.pack(">9i", 0x00010000, 0, 0, 0, 0x00010000, 0,
                                   0, 0, 0x40000000)
                     + b"\x00" * 24
                     + struct.pack(">I", 2))

    tkhd = _full_box(b"tkhd", 0, b"\x00\x00\x07",
                     struct.pack(">IIIII", 0, 0, 1, 0, duration)
                     + b"\x00" * 8
                     + struct.pack(">hhhh", 0, 0, 0, 0)
                     + struct.pack(">9i", 0x00010000, 0, 0, 0, 0x00010000, 0,
                                   0, 0, 0x40000000)
                     + struct.pack(">II", 320 << 16, 240 << 16))

    mdhd = _full_box(b"mdhd", 0, b"\x00\x00\x00",
                     struct.pack(">IIII", 0, 0, timescale, duration)
                     + struct.pack(">HH", 0x55C4, 0))

    hdlr = _full_box(b"hdlr", 0, b"\x00\x00\x00",
                     struct.pack(">I", 0) + b"vide" + b"\x00" * 12
                     + b"RecoveryCoreFixture\x00")

    vmhd = _full_box(b"vmhd", 0, b"\x00\x00\x01",
                     struct.pack(">HHHH", 0, 0, 0, 0))
    dref = _full_box(b"dref", 0, b"\x00\x00\x00",
                     struct.pack(">I", 1)
                     + _full_box(b"url ", 0, b"\x00\x00\x01", b""))
    dinf = _box(b"dinf", dref)

    avc_entry = _box(b"avc1",
                     b"\x00" * 6 + struct.pack(">H", 1)
                     + b"\x00" * 16
                     + struct.pack(">HH", 320, 240)
                     + struct.pack(">II", 0x00480000, 0x00480000)
                     + struct.pack(">I", 0)
                     + struct.pack(">H", 1)
                     + b"\x00" * 32
                     + struct.pack(">Hh", 24, -1))
    stsd = _full_box(b"stsd", 0, b"\x00\x00\x00",
                     struct.pack(">I", 1) + avc_entry)
    stts = _full_box(b"stts", 0, b"\x00\x00\x00",
                     struct.pack(">I", 1) + struct.pack(">II", n_samples, 40))
    stsc = _full_box(b"stsc", 0, b"\x00\x00\x00",
                     struct.pack(">I", 1)
                     + struct.pack(">III", 1, samples_per_chunk, 1))
    stsz = _full_box(b"stsz", 0, b"\x00\x00\x00",
                     struct.pack(">II", sample_size, n_samples))

    # Placeholder stco, replaced below once the moov size is known.
    stco_payload = struct.pack(">I", chunks) + b"\x00" * (4 * chunks)
    stco = _full_box(b"stco", 0, b"\x00\x00\x00", stco_payload)

    def assemble(stco_box):
        stbl = _box(b"stbl", stsd + stts + stsc + stsz + stco_box)
        minf = _box(b"minf", vmhd + dinf + stbl)
        mdia = _box(b"mdia", mdhd + hdlr + minf)
        trak = _box(b"trak", tkhd + mdia)
        return _box(b"moov", mvhd + trak)

    moov = assemble(stco)
    # mdat payload begins 8 bytes into the mdat box, which follows ftyp + moov.
    mdat_data_start = len(ftyp) + len(moov) + 8
    real = struct.pack(">I", chunks) + b"".join(
        struct.pack(">I", mdat_data_start + off) for off in chunk_rel_offsets)
    moov = assemble(_full_box(b"stco", 0, b"\x00\x00\x00", real))

    return ftyp + moov + _box(b"mdat", bytes(media))


# ---------------------------------------------------------------------------
# DOCX / ZIP and SQLite
# ---------------------------------------------------------------------------

def make_docx(seed, paragraphs=40):
    """Valid OOXML package: real ZIP local headers and central directory."""
    rng = random.Random(seed)
    body = []
    for i in range(paragraphs):
        words = " ".join(
            "".join(rng.choice("abcdefghijklmnopqrstuvwxyz")
                    for _ in range(rng.randrange(3, 11)))
            for _ in range(12))
        body.append("<w:p><w:r><w:t>Paragraph %d: %s</w:t></w:r></w:p>"
                    % (i, words))

    document = (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<w:document xmlns:w="http://schemas.openxmlformats.org/'
        'wordprocessingml/2006/main"><w:body>' + "".join(body)
        + "</w:body></w:document>")
    content_types = (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<Types xmlns="http://schemas.openxmlformats.org/package/2006/'
        'content-types">'
        '<Default Extension="rels" ContentType="application/vnd.openxmlformats-'
        'package.relationships+xml"/>'
        '<Default Extension="xml" ContentType="application/xml"/>'
        '<Override PartName="/word/document.xml" ContentType="application/vnd.'
        'openxmlformats-officedocument.wordprocessingml.document.main+xml"/>'
        "</Types>")
    rels = (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/'
        'relationships"><Relationship Id="rId1" Type="http://schemas.'
        'openxmlformats.org/officeDocument/2006/relationships/officeDocument" '
        'Target="word/document.xml"/></Relationships>')

    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as z:
        for name, data in (("[Content_Types].xml", content_types),
                           ("_rels/.rels", rels),
                           ("word/document.xml", document)):
            info = zipfile.ZipInfo(name, date_time=DOS_EPOCH)
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = 0o600 << 16
            z.writestr(info, data)
    return buf.getvalue()


def make_sqlite(seed, rows=300):
    """SQLite database with committed content plus a populated freelist.

    Rows are inserted then deleted so that free pages carry recoverable record
    payloads, which is exactly what rc-sqlite-carve targets in Milestone 7.
    """
    rng = random.Random(seed)
    fd, path = tempfile.mkstemp(suffix=".sqlite")
    os.close(fd)
    os.unlink(path)
    conn = sqlite3.connect(path)
    conn.execute("PRAGMA journal_mode=DELETE")
    conn.execute("PRAGMA secure_delete=OFF")  # leave deleted payloads on the page
    conn.execute("CREATE TABLE messages (id INTEGER PRIMARY KEY, sender TEXT, "
                 "body TEXT, ts INTEGER)")
    for i in range(rows):
        conn.execute("INSERT INTO messages VALUES (?,?,?,?)",
                     (i, "sender%03d" % (i % 17),
                      "message body %d %s" % (i, "x" * rng.randrange(20, 120)),
                      1700000000 + i * 61))
    conn.commit()
    conn.execute("DELETE FROM messages WHERE id % 3 = 0")
    conn.commit()
    conn.close()
    with open(path, "rb") as fh:
        data = fh.read()
    os.unlink(path)
    return data


def make_text(seed, approx_bytes):
    rng = random.Random(seed)
    words = ["recovery", "cluster", "sector", "carve", "inode", "journal",
             "extent", "bitmap", "fragment", "signature", "entropy", "runlist"]
    out = []
    total = 0
    line = 0
    while total < approx_bytes:
        s = "%05d %s\n" % (line, " ".join(rng.choice(words)
                                          for _ in range(12)))
        out.append(s)
        total += len(s)
        line += 1
    return "".join(out).encode("ascii")


def make_binary(seed, size, entropy="high"):
    """Filler with a controlled entropy profile.

    The 'low' variant is highly repetitive and the 'high' variant is
    incompressible; Milestone 4's entropy-boundary detector is tested against
    the transition between them.
    """
    rng = random.Random(seed)
    if entropy == "low":
        pattern = bytes(rng.randrange(256) for _ in range(16))
        return (pattern * (size // 16 + 1))[:size]
    return bytes(rng.randrange(256) for _ in range(size))


# ---------------------------------------------------------------------------
# Driver
# ---------------------------------------------------------------------------

CORPUS = [
    # (relpath, kind, builder)
    ("photos/img_0001.jpg", "jpeg", lambda: make_jpeg(320, 240, 1001)),
    ("photos/img_0002.jpg", "jpeg", lambda: make_jpeg(256, 192, 1002)),
    ("photos/img_0003.jpg", "jpeg", lambda: make_jpeg(400, 300, 1003)),
    ("photos/nested/img_0004.jpg", "jpeg", lambda: make_jpeg(200, 200, 1004)),
    ("photos/screenshot.png", "png", lambda: make_png(240, 180, 2001)),
    ("photos/diagram.png", "png", lambda: make_png(320, 240, 2002)),
    ("docs/report.pdf", "pdf", lambda: make_pdf(3001, pages=4)),
    ("docs/notes.pdf", "pdf", lambda: make_pdf(3002, pages=2)),
    ("docs/letter.docx", "docx", lambda: make_docx(4001)),
    ("docs/minutes.docx", "docx", lambda: make_docx(4002, paragraphs=80)),
    ("video/clip_a.mp4", "mp4", lambda: make_mp4(5001, chunks=10)),
    ("video/clip_b.mp4", "mp4", lambda: make_mp4(5002, chunks=20)),
    ("data/messages.sqlite", "sqlite", lambda: make_sqlite(6001)),
    ("data/readme.txt", "text", lambda: make_text(7001, 4096)),
    ("data/manifest.txt", "text", lambda: make_text(7002, 32768)),
    ("data/blob_high.bin", "binary", lambda: make_binary(8001, 65536, "high")),
    ("data/blob_low.bin", "binary", lambda: make_binary(8002, 65536, "low")),
]

# ---------------------------------------------------------------------------
# Adversarial names and shapes
# ---------------------------------------------------------------------------
#
# A recovery score measured against a flat directory of short ASCII names does
# not tell you much. These entries target the specific places filesystem
# parsers actually break:
#
#   * Deep nesting exercises directory-tree reconstruction -- NTFS rebuilds a
#     path by following parent references through $FILE_NAME, and FAT/exFAT by
#     recursive descent. Both go wrong at depth.
#   * Non-ASCII names exercise UTF-16 decoding in three different encodings:
#     NTFS $FILE_NAME, FAT long-filename entry chains (UCS-2 split across three
#     disjoint field ranges per entry) and exFAT name entries (15 units each).
#     The emoji entries matter most: they are surrogate pairs, so a parser that
#     decodes UTF-16 one code unit at a time produces mojibake.
#   * Names near the 255-character limit force the longest possible entry
#     chains: 20 LFN entries on FAT (the maximum the format allows) and 17 name
#     entries on exFAT. Off-by-one chain handling shows up here and nowhere
#     else.
#   * A file fragmented before deletion breaks the assumption FAT recovery
#     leans on, because FAT zeroes the cluster chain on delete and contiguity
#     is only a guess. See FRAGMENT_TARGET below.

# Eight levels below the root.
_DEEP = "deep/lvl1/lvl2/lvl3/lvl4/lvl5/lvl6/lvl7/lvl8"

# 250 and 200 characters: just inside the 255-unit component limit that NTFS,
# exFAT and FAT long names all share.
_LONG_250 = ("long_" + "n" * 240 + "_end")[:250 - len(".txt")] + ".txt"
_LONG_200 = ("wide_" + "w" * 190 + "_end")[:200 - len(".jpg")] + ".jpg"

# The file the fixture generator deliberately fragments before deleting it.
# A PNG because it is large enough to span several holes and carries per-chunk
# CRCs, so Milestone 3 can validate the reassembly rather than just its length.
FRAGMENT_TARGET = "photos/fragmented_photo.png"

# Unicode paths are named constants rather than repeated literals, because a
# string typed twice can differ in *normalisation form* while looking
# identical: "é" is either U+00E9 (NFC) or "e" + U+0301 (NFD). Repeating these
# literals silently desynchronised CORPUS from DELETED_SET once already.
#
# The two café entries are deliberately built in opposite forms, so the
# fixtures genuinely test that names round-trip byte-exactly. NTFS, FAT and
# exFAT all store the bytes they are given without normalising; a parser that
# normalises on the way out would fail one of these two and pass the other.
_CAFE_STEM = "café_niño_αβγ"
PATH_CAFE_NFD = "unicode/" + unicodedata.normalize("NFD", _CAFE_STEM) + ".txt"
PATH_CAFE_NFC = "unicode/" + unicodedata.normalize("NFC", _CAFE_STEM) + "_precomposed.txt"
PATH_CJK = "unicode/日本語のファイル.jpg"
PATH_CYRILLIC_GREEK = "unicode/Кириллица_Ωμέγα.png"
PATH_EMOJI = "unicode/emoji_\U0001F389\U0001F525_test.txt"
PATH_ARABIC = "unicode/ملف_عربي.txt"
PATH_SPACES = "unicode/spaces and.dots.in.name .txt"
PATH_DEEP_JPG = _DEEP + "/buried.jpg"

ADVERSARIAL = [
    # --- deep nesting ---
    (PATH_DEEP_JPG, "jpeg", lambda: make_jpeg(160, 120, 1101)),
    (_DEEP + "/buried.txt", "text", lambda: make_text(1102, 2048)),
    ("deep/lvl1/lvl2/midway.txt", "text", lambda: make_text(1103, 1024)),

    # --- non-ASCII, spanning several scripts and normalisation traps ---
    # These use the module constants rather than repeating the literals: two
    # spellings of the same name in different normalisation forms look
    # identical in a diff but are different byte sequences, which silently
    # desynchronised this list from DELETED_SET once already.
    (PATH_CAFE_NFD, "text", lambda: make_text(1201, 1500)),   # decomposed
    (PATH_CAFE_NFC, "text", lambda: make_text(1202, 1500)),   # precomposed
    (PATH_CJK, "jpeg", lambda: make_jpeg(128, 96, 1203)),
    (PATH_CYRILLIC_GREEK, "png", lambda: make_png(64, 48, 1204)),
    # Emoji: surrogate pairs in UTF-16. A parser that treats code units as
    # characters mangles these.
    (PATH_EMOJI, "text", lambda: make_text(1205, 900)),
    (PATH_ARABIC, "text", lambda: make_text(1206, 900)),      # RTL
    (PATH_SPACES, "text", lambda: make_text(1207, 700)),      # space in the name

    # --- long names ---
    ("longnames/" + _LONG_250, "text", lambda: make_text(1301, 3000)),
    ("longnames/" + _LONG_200, "jpeg", lambda: make_jpeg(96, 96, 1302)),

    # --- the fragmentation target ---
    (FRAGMENT_TARGET, "png", lambda: make_png(320, 240, 1401)),
]

CORPUS = CORPUS + ADVERSARIAL

# The files build_fixtures.sh deletes from every basic filesystem fixture, and
# therefore the set Milestone 2's >=95% name-accuracy bar is measured against.
#
# Defined here rather than in the shell script so the two cannot drift: a
# 250-character name written out twice is a typo waiting to happen. The shell
# reads this list via `make_corpus.py --deleted-set`.
#
# Deliberately weighted towards the hard cases. Recovering nine short ASCII
# names in a flat directory would say nothing about whether the parsers work.
DELETED_SET = [
    # ordinary files, spanning container formats
    "photos/img_0001.jpg",
    "photos/img_0003.jpg",
    "photos/nested/img_0004.jpg",
    "photos/screenshot.png",
    "docs/report.pdf",
    "docs/letter.docx",
    "video/clip_a.mp4",
    "data/messages.sqlite",
    "data/readme.txt",
    # nine levels deep: exercises path reconstruction, which on NTFS means
    # following parent references and on FAT/exFAT means recursive descent
    PATH_DEEP_JPG,
    # non-ASCII across scripts, including surrogate pairs and a decomposed
    # form, all referenced by constant so they cannot drift from CORPUS
    PATH_CJK,
    PATH_EMOJI,
    PATH_CAFE_NFD,
    PATH_CYRILLIC_GREEK,
    # 250 characters: 20 LFN entries on FAT, the format maximum
    "longnames/" + _LONG_250,
    # deliberately fragmented before deletion
    FRAGMENT_TARGET,
]


# Larger payloads used only by the fragmented-* fixtures. Each file must be
# comfortably bigger than the filler hole size (see build_fixtures.sh) so the
# allocator is forced to split it across several non-adjacent runs.
FRAG_CORPUS = [
    ("frag/large_a.jpg", "jpeg", lambda: make_jpeg(640, 480, 9001)),
    ("frag/large_b.jpg", "jpeg", lambda: make_jpeg(560, 420, 9002)),
    ("frag/large_c.mp4", "mp4", lambda: make_mp4(9003, chunks=120)),
    ("frag/large_d.png", "png", lambda: make_png(640, 480, 9004)),
]

SETS = {"basic": CORPUS, "frag": FRAG_CORPUS}

# Every deleted path must be a real corpus path. Without this a typo, or two
# spellings of the same name in different Unicode normalisation forms, silently
# shrinks the graded set and weakens the accuracy bar instead of failing.
_CORPUS_PATHS = {p for p, _k, _b in CORPUS}
_ORPHANS = [p for p in DELETED_SET if p not in _CORPUS_PATHS]
if _ORPHANS:
    raise SystemExit(
        "DELETED_SET contains %d path(s) that are not in CORPUS: %r. "
        "If a name looks identical to a corpus entry, compare their Unicode "
        "normalisation forms -- NFC and NFD render the same but are different "
        "byte sequences." % (len(_ORPHANS), _ORPHANS)
    )
if FRAGMENT_TARGET not in _CORPUS_PATHS:
    raise SystemExit("FRAGMENT_TARGET %r is not in CORPUS" % FRAGMENT_TARGET)


def _long_path(p):
    """Make a path usable past Windows' 260-character MAX_PATH.

    The corpus deliberately contains 250-character filenames, so a moderately
    deep output directory pushes the full path over the limit. Windows accepts
    longer paths only through the \\\\?\\ extended-length prefix, which also
    requires a backslash-separated absolute path. No-op everywhere else.
    """
    if os.name != "nt":
        return p
    p = os.path.abspath(p)
    if p.startswith("\\\\?\\"):
        return p
    if p.startswith("\\\\"):
        return "\\\\?\\UNC\\" + p[2:]
    return "\\\\?\\" + p.replace("/", "\\")


def build(outdir, items):
    manifest = {}
    for relpath, kind, builder in items:
        data = builder()
        dest = _long_path(os.path.join(outdir, relpath))
        os.makedirs(os.path.dirname(dest), exist_ok=True)
        with open(dest, "wb") as fh:
            fh.write(data)
        manifest[relpath] = {
            "sha256": hashlib.sha256(data).hexdigest(),
            "size": len(data),
            "kind": kind,
        }
    return manifest


def main(argv):
    args = [a for a in argv[1:] if not a.startswith("--")]
    flags = [a for a in argv[1:] if a.startswith("--")]

    # Queries used by build_fixtures.sh so the shell never restates these.
    # Written as raw bytes on purpose. These paths are not ASCII, and
    # sys.stdout's encoding follows the caller's locale -- cp1252 on a default
    # Windows console, which cannot represent them and raises. Going through
    # the buffer also avoids newline translation, so the shell reading this
    # list never sees a stray CR in a filename.
    if "--deleted-set" in flags:
        for p in DELETED_SET:
            sys.stdout.buffer.write(p.encode("utf-8") + b"\n")
        return 0
    if "--fragment-target" in flags:
        sys.stdout.buffer.write(FRAGMENT_TARGET.encode("utf-8") + b"\n")
        return 0

    which = "basic"
    for a in flags:
        if a.startswith("--set="):
            which = a.split("=", 1)[1]
    if len(args) != 1 or which not in SETS:
        sys.stderr.write(
            "usage: make_corpus.py [--set=basic|frag] OUTDIR\n"
            "       make_corpus.py --deleted-set\n"
            "       make_corpus.py --fragment-target\n"
        )
        return 2
    outdir = args[0]
    os.makedirs(outdir, exist_ok=True)
    manifest = build(outdir, SETS[which])
    json.dump(manifest, sys.stdout, indent=2, sort_keys=True)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
