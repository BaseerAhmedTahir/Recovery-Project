#!/usr/bin/env python3
"""Corpus files written by encoders this project had no hand in.

Why this exists
---------------

``make_corpus.py`` builds its JPEG, PNG, PDF and MP4 samples with encoders
written by hand, in that file, by the same person who wrote the validators
they are used to test. That is a correlated-error risk and not a small one: a
misreading of a spec produces an encoder and a validator that share the
misreading, and the test then reports a clean 100% that means nothing. Four
more formats - BMP, WAV, AVI, WebP - had no corpus file at all and were graded
only against hand-built byte vectors.

So the samples here come from ImageMagick and ffmpeg. Neither knows this
project exists. When a validator accepts one of these files, that is evidence
about the format rather than about one reading of it.

What this does NOT do
---------------------

These files are not written into the disk-image fixtures. Validator grading
needs bytes, not a filesystem, so folding them into ``make_corpus.py`` would
force a rebuild of all eight images for no gain at this stage. The Milestone 3
*carving* acceptance test is a different matter - carving 20+ formats out of a
quick-formatted image needs those formats to actually be in the image - and
that is recorded as a prerequisite in ``docs/PROGRESS.md``.

Missing tools are reported, never silently skipped. A generator that quietly
produces fewer files would turn a coverage gap into a passing test, which is
the exact failure this file exists to prevent.

Usage
-----

    make_independent.py OUTDIR      write the samples, print a JSON manifest
    make_independent.py --probe     report which encoders are available
"""

import hashlib
import json
import os
import re
import shutil
import subprocess
import sys

# --------------------------------------------------------------------------
# tool discovery
# --------------------------------------------------------------------------


def _run(args, **kw):
    return subprocess.run(
        args, capture_output=True, timeout=120, check=False, **kw
    )


def _search_dirs():
    """Places a package manager may have put a binary that PATH does not yet
    mention.

    A freshly installed tool is not always on the PATH of the process that
    installed it - chocolatey and winget both update the environment for
    *subsequent* shells - and on CI the install and the test can share one. An
    encoder that is present but unfindable would fail the corpus test as though
    it were absent, which is a confusing way to learn about a PATH refresh.
    """
    out = []
    program_data = os.environ.get("ProgramData")
    if program_data:
        out.append(os.path.join(program_data, "chocolatey", "bin"))
    for var in ("ProgramFiles", "ProgramFiles(x86)"):
        root = os.environ.get(var)
        if root and os.path.isdir(root):
            try:
                for name in os.listdir(root):
                    if name.lower().startswith("imagemagick"):
                        out.append(os.path.join(root, name))
            except OSError:
                pass
    return out


def _find_exe(name, extra_roots=()):
    """PATH first, then package-manager locations, then a bounded walk."""
    exe = shutil.which(name)
    if exe:
        return exe
    stem = name + (".exe" if os.name == "nt" else "")
    for d in _search_dirs():
        cand = os.path.join(d, stem)
        if os.path.isfile(cand):
            return cand
    for root in extra_roots:
        if not root or not os.path.isdir(root):
            continue
        for dirpath, _dirs, files in os.walk(root):
            if stem in files:
                return os.path.join(dirpath, stem)
    return None


def _version_of(exe):
    if not exe:
        return None
    out = _run([exe, "-version"])
    if out.returncode != 0:
        return None
    lines = out.stdout.decode("utf-8", "replace").splitlines()
    return {"exe": exe, "version": lines[0].strip() if lines else "unknown"}


def find_magick():
    """ImageMagick 7 is `magick`; 6 is `convert`, which collides with a Windows
    system binary of the same name, so only v7 is accepted."""
    return _version_of(_find_exe("magick"))


def find_ffmpeg():
    local = os.environ.get("LOCALAPPDATA")
    # winget installs into a versioned package directory under here.
    roots = [os.path.join(local, "Microsoft", "WinGet", "Packages")] if local else []
    return _version_of(_find_exe("ffmpeg", roots))


# --------------------------------------------------------------------------
# sample definitions
# --------------------------------------------------------------------------

# Each entry: (relative path, kind, tool, argv builder).
#
# The variants are chosen to move the structures the validators actually walk,
# not just to produce more files: progressive vs baseline JPEG changes the
# marker sequence, an interlaced PNG changes the chunk set, faststart moves an
# MP4's moov box in front of its mdat, and BMP bit depths change the DIB header
# and the row padding arithmetic.


def magick_samples(magick):
    e = magick["exe"]

    def m(*args):
        return [e] + list(args)

    src = ["-size", "96x64", "gradient:red-blue"]
    # plasma is random unless seeded, which made every run grade different
    # images - so a failure need not reproduce. Seeded since Milestone 4.
    photo = ["-seed", "1", "-size", "128x96", "plasma:fractal"]
    # Neither dimension a multiple of 16, so a subsampled image ends in partial
    # MCUs on both edges - the case MCU-count arithmetic gets wrong.
    odd = ["-seed", "2", "-size", "131x97", "plasma:fractal"]
    return [
        # --- JPEG ----------------------------------------------------------
        ("independent/im_baseline.jpg", "jpeg", m(*photo, "-quality", "82", "{out}")),
        (
            "independent/im_progressive.jpg",
            "jpeg",
            m(*photo, "-interlace", "Plane", "-quality", "75", "{out}"),
        ),
        # Restart intervals. Until Milestone 4 the corpus had one JPEG named for
        # them, im_restart.jpg, and its arguments never asked for any: no JPEG
        # from a foreign encoder had a single restart marker, so the RST cycle
        # check and the DRI bound had only ever met this project's own encoder.
        # The variants move what that arithmetic depends on - subsampling (the
        # blocks in an MCU), progressive scans (each restarts the RST cycle),
        # optimised Huffman tables, and edges of partial MCUs. PROPERTIES below
        # checks that each file really has what it is here for.
        (
            "independent/im_restart.jpg",
            "jpeg",
            m(*photo, "-define", "jpeg:dct-method=float",
              "-define", "jpeg:restart-interval=4", "-quality", "90", "{out}"),
        ),
        (
            "independent/im_restart_420.jpg",
            "jpeg",
            m(*photo, "-sampling-factor", "2x2,1x1,1x1",
              "-define", "jpeg:restart-interval=3", "-quality", "85", "{out}"),
        ),
        (
            "independent/im_restart_422.jpg",
            "jpeg",
            m(*photo, "-sampling-factor", "2x1,1x1,1x1",
              "-define", "jpeg:restart-interval=5", "{out}"),
        ),
        (
            "independent/im_restart_444.jpg",
            "jpeg",
            m(*photo, "-sampling-factor", "1x1,1x1,1x1",
              "-define", "jpeg:restart-interval=7", "-quality", "95", "{out}"),
        ),
        (
            "independent/im_restart_grey.jpg",
            "jpeg",
            m(*photo, "-colorspace", "Gray", "-define", "jpeg:restart-interval=1", "{out}"),
        ),
        (
            "independent/im_restart_odd.jpg",
            "jpeg",
            m(*odd, "-sampling-factor", "2x2,1x1,1x1",
              "-define", "jpeg:restart-interval=5", "{out}"),
        ),
        (
            "independent/im_restart_progressive.jpg",
            "jpeg",
            m(*photo, "-interlace", "Plane", "-define", "jpeg:restart-interval=2", "{out}"),
        ),
        (
            "independent/im_restart_standard_tables.jpg",
            "jpeg",
            m(*photo, "-sampling-factor", "2x2,1x1,1x1",
              "-define", "jpeg:optimize-coding=false",
              "-define", "jpeg:restart-interval=4", "{out}"),
        ),
        ("independent/im_grey.jpg", "jpeg", m(*photo, "-colorspace", "Gray", "{out}")),
        # --- PNG -----------------------------------------------------------
        # A 64-row gradient has few enough colours that ImageMagick quietly
        # writes a palette PNG, which this file was until PROPERTIES checked
        # (im_palette.png already covers palettes). PNG24: forces true colour.
        ("independent/im_rgb8.png", "png", m(*src, "-depth", "8", "PNG24:{out}")),
        ("independent/im_rgb16.png", "png", m(*src, "-depth", "16", "{out}")),
        (
            "independent/im_palette.png",
            "png",
            m(*src, "-colors", "16", "-type", "Palette", "{out}"),
        ),
        (
            "independent/im_interlaced.png",
            "png",
            m(*src, "-interlace", "PNG", "{out}"),
        ),
        (
            "independent/im_alpha.png",
            "png",
            m(*src, "-alpha", "set", "-channel", "A", "-evaluate", "set", "50%", "{out}"),
        ),
        # --- BMP: bit depth drives the DIB header and the row padding -------
        ("independent/im_24bit.bmp", "bmp", m(*src, "-type", "TrueColor", "{out}")),
        (
            "independent/im_8bit.bmp",
            "bmp",
            m(*src, "-colors", "256", "-type", "Palette", "{out}"),
        ),
        (
            "independent/im_32bit.bmp",
            "bmp",
            m(*src, "-alpha", "set", "-define", "bmp:format=bmp4", "{out}"),
        ),
        # --- WebP ----------------------------------------------------------
        ("independent/im_lossy.webp", "webp", m(*photo, "-quality", "80", "{out}")),
        (
            "independent/im_lossless.webp",
            "webp",
            m(*src, "-define", "webp:lossless=true", "{out}"),
        ),
        # --- GIF, TIFF and PSD: three more formats whose validators would
        # otherwise be graded only against my own reading of the spec ---------
        ("independent/im_gif89.gif", "gif", m(*src, "{out}")),
        (
            "independent/im_interlaced.gif",
            "gif",
            m(*src, "-interlace", "GIF", "{out}"),
        ),
        # `-endian LSB` is silently ignored by ImageMagick 7's TIFF writer: both
        # of these were big-endian until PROPERTIES checked, so the
        # little-endian TIFF validator had never seen a foreign file. The
        # define is what the TIFF coder reads.
        ("independent/im_le.tif", "tiff_le", m(*src, "-define", "tiff:endian=lsb", "{out}")),
        ("independent/im_be.tif", "tiff_be", m(*src, "-define", "tiff:endian=msb", "{out}")),
        ("independent/im_layers.psd", "psd", m(*src, "{out}")),
        # --- ICO: the directory-plus-images format whose signature is
        # three-quarters zeros, so real ones matter for grading the validator --
        ("independent/im_icon.ico", "ico", m(*src, "-resize", "32x32", "{out}")),
        (
            "independent/im_multisize.ico",
            "ico",
            m(*src, "-resize", "16x16", "-define", "icon:auto-resize=16,32,48", "{out}"),
        ),
        # --- PDF: ImageMagick writes PDF natively, no Ghostscript needed ----
        ("independent/im_onepage.pdf", "pdf", m(*src, "{out}")),
        (
            "independent/im_multipage.pdf",
            "pdf",
            m(*src, *src, *src, "{out}"),
        ),
    ]


# --------------------------------------------------------------------------
# what each file is here for, checked
# --------------------------------------------------------------------------
#
# A sample is chosen to exercise one structural variation, and nothing checked
# that the encoder produced it: im_restart.jpg shipped through Milestone 3
# without a restart marker, so everything the suite said about restart markers
# in foreign files was vacuous. Each entry here states the property its file
# exists for, and a file without it is a generator failure - loud, like a
# missing tool - rather than a quiet gap in coverage.

# Annex K's luminance DC code-length counts. A JPEG whose first DHT carries
# these is using the standard tables rather than optimised ones.
_ANNEX_K_DC_LUMA_BITS = bytes([0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0])


def _jpeg_segments(d):
    """Marker segments up to and including the first SOS: [(marker, payload)]."""
    out, i = [], 2
    while i + 4 <= len(d) and d[i] == 0xFF:
        m = d[i + 1]
        if m == 0xFF:
            i += 1
            continue
        n = int.from_bytes(d[i + 2:i + 4], "big")
        out.append((m, d[i + 4:i + 2 + n]))
        if m == 0xDA:
            break
        i += 2 + n
    return out


def _jpeg(d):
    segs = _jpeg_segments(d)
    info = {"sof": None, "sampling": [], "restart": 0, "dims": None, "standard_dc": None}
    for m, pl in segs:
        if m in (0xC0, 0xC1, 0xC2) and info["sof"] is None:
            info["sof"] = m
            info["dims"] = (int.from_bytes(pl[3:5], "big"), int.from_bytes(pl[1:3], "big"))
            info["sampling"] = [(pl[6 + 3 * c + 1] >> 4, pl[6 + 3 * c + 1] & 15)
                                for c in range(pl[5])]
        elif m == 0xDD:
            info["restart"] = int.from_bytes(pl[:2], "big")
        elif m == 0xC4 and info["standard_dc"] is None:
            # The first table in the first DHT; class 0 id 0 is luminance DC.
            info["standard_dc"] = pl[0] == 0x00 and pl[1:17] == _ANNEX_K_DC_LUMA_BITS
    return info


def _restart(n, sampling=None, sof=0xC0):
    def check(d):
        j = _jpeg(d)
        return (j["restart"] == n and j["sof"] == sof
                and (sampling is None or j["sampling"] == sampling))
    return check


def _png_ihdr(d):
    # signature(8) length(4) "IHDR"(4) width(4) height(4) depth colour comp filt interlace
    return {"depth": d[24], "colour": d[25], "interlace": d[28]}


def _bmp_bpp(d):
    return int.from_bytes(d[28:30], "little")


def _riff_fmt(d):
    i = d.find(b"fmt ")
    return {"channels": int.from_bytes(d[i + 10:i + 12], "little"),
            "bits": int.from_bytes(d[i + 22:i + 24], "little")}


def _top_boxes(d):
    out, i = [], 0
    while i + 8 <= len(d):
        n = int.from_bytes(d[i:i + 4], "big")
        t = d[i + 4:i + 8]
        if n == 1:
            n = int.from_bytes(d[i + 8:i + 16], "big")
        if n < 8:
            break
        out.append(t)
        i += n
    return out


def _gif_first_image_interlaced(d):
    i = 13
    if d[10] & 0x80:
        i += 3 * (2 << (d[10] & 7))
    while i < len(d):
        if d[i] == 0x2C:
            return bool(d[i + 9] & 0x40)
        if d[i] == 0x21:
            i += 2
            while d[i]:
                i += d[i] + 1
            i += 1
            continue
        return None
    return None


def _psd_layers(d):
    i = 26
    i += 4 + int.from_bytes(d[i:i + 4], "big")  # colour mode data
    i += 4 + int.from_bytes(d[i:i + 4], "big")  # image resources
    # layer and mask info: total length, then layer info length, then count
    if int.from_bytes(d[i:i + 4], "big") == 0:
        return 0
    return abs(int.from_bytes(d[i + 8:i + 10], "big", signed=True))


def _pdf_pages(d):
    # Page objects, not the /Pages tree node.
    return len(re.findall(rb"/Type\s*/Page(?![a-zA-Z])", d))


def _aiff_bits(d):
    i = d.find(b"COMM")
    # COMM: size(4) channels(2) frames(4) sample size(2)
    return int.from_bytes(d[i + 14:i + 16], "big")


PROPERTIES = {
    "independent/im_baseline.jpg": ("baseline (SOF0)", lambda d: _jpeg(d)["sof"] == 0xC0),
    "independent/im_progressive.jpg": ("progressive (SOF2)", lambda d: _jpeg(d)["sof"] == 0xC2),
    "independent/im_grey.jpg": ("one component", lambda d: len(_jpeg(d)["sampling"]) == 1),
    "independent/im_restart.jpg": ("restart interval 4", _restart(4, None)),
    "independent/im_restart_420.jpg": ("4:2:0, restart interval 3",
                                       _restart(3, [(2, 2), (1, 1), (1, 1)])),
    "independent/im_restart_422.jpg": ("4:2:2, restart interval 5",
                                       _restart(5, [(2, 1), (1, 1), (1, 1)])),
    "independent/im_restart_444.jpg": ("4:4:4, restart interval 7",
                                       _restart(7, [(1, 1), (1, 1), (1, 1)])),
    "independent/im_restart_grey.jpg": ("one component, restart interval 1",
                                        _restart(1, [(1, 1)])),
    "independent/im_restart_odd.jpg": (
        "4:2:0, restart interval 5, partial MCUs on both edges",
        lambda d: _restart(5, [(2, 2), (1, 1), (1, 1)])(d)
        and _jpeg(d)["dims"][0] % 16 != 0 and _jpeg(d)["dims"][1] % 16 != 0),
    "independent/im_restart_progressive.jpg": ("progressive, restart interval 2",
                                               _restart(2, None, sof=0xC2)),
    "independent/im_restart_standard_tables.jpg": (
        "Annex K Huffman tables, 4:2:0, restart interval 4",
        lambda d: _restart(4, [(2, 2), (1, 1), (1, 1)])(d) and _jpeg(d)["standard_dc"]),
    "independent/im_rgb8.png": ("8-bit RGB", lambda d: (_png_ihdr(d)["depth"], _png_ihdr(d)["colour"]) == (8, 2)),
    "independent/im_rgb16.png": ("16-bit samples", lambda d: _png_ihdr(d)["depth"] == 16),
    "independent/im_palette.png": ("palette colour type", lambda d: _png_ihdr(d)["colour"] == 3),
    "independent/im_interlaced.png": ("Adam7 interlace", lambda d: _png_ihdr(d)["interlace"] == 1),
    "independent/im_alpha.png": ("an alpha channel", lambda d: _png_ihdr(d)["colour"] in (4, 6)),
    "independent/im_24bit.bmp": ("24 bits per pixel", lambda d: _bmp_bpp(d) == 24),
    "independent/im_8bit.bmp": ("8 bits per pixel", lambda d: _bmp_bpp(d) == 8),
    "independent/im_32bit.bmp": ("32 bits per pixel", lambda d: _bmp_bpp(d) == 32),
    "independent/im_lossy.webp": ("a lossy VP8 bitstream", lambda d: b"VP8 " in d[12:64]),
    "independent/im_lossless.webp": ("a lossless VP8L bitstream", lambda d: b"VP8L" in d[12:64]),
    "independent/im_gif89.gif": ("GIF89a", lambda d: d[:6] == b"GIF89a"),
    "independent/im_interlaced.gif": ("an interlaced image", _gif_first_image_interlaced),
    "independent/im_le.tif": ("little-endian", lambda d: d[:2] == b"II"),
    "independent/im_be.tif": ("big-endian", lambda d: d[:2] == b"MM"),
    "independent/im_layers.psd": ("at least one layer", lambda d: _psd_layers(d) >= 1),
    "independent/im_multisize.ico": ("three images", lambda d: int.from_bytes(d[4:6], "little") == 3),
    "independent/ff_h264.mp4": ("H.264 with avcC, moov after mdat",
                                lambda d: b"avcC" in d and _top_boxes(d).index(b"mdat")
                                < _top_boxes(d).index(b"moov")),
    "independent/ff_faststart.mp4": ("moov before mdat",
                                     lambda d: _top_boxes(d).index(b"moov")
                                     < _top_boxes(d).index(b"mdat")),
    "independent/ff_mpeg4.mp4": ("MPEG-4 Part 2 video", lambda d: b"mp4v" in d),
    "independent/ff_av.mp4": ("two tracks", lambda d: d.count(b"trak") >= 2),
    "independent/im_onepage.pdf": ("one page", lambda d: _pdf_pages(d) == 1),
    "independent/im_multipage.pdf": ("three pages", lambda d: _pdf_pages(d) == 3),
    "independent/ff_pcm16.wav": ("16-bit samples", lambda d: _riff_fmt(d)["bits"] == 16),
    "independent/ff_pcm16.aiff": ("16-bit samples", lambda d: _aiff_bits(d) == 16),
    "independent/ff_pcm24.wav": ("24-bit samples", lambda d: _riff_fmt(d)["bits"] == 24),
    "independent/ff_stereo8.wav": ("8-bit stereo",
                                   lambda d: _riff_fmt(d) == {"channels": 2, "bits": 8}),
    "independent/ff_mjpeg.avi": ("MJPEG video", lambda d: b"MJPG" in d),
}


def check_property(rel, data):
    """None if the file has what it is here for, else a reason it does not."""
    if rel not in PROPERTIES:
        return None
    what, test = PROPERTIES[rel]
    try:
        ok = test(data)
    except (IndexError, ValueError):
        ok = False
    return None if ok else "expected %s; the encoder did not produce it" % what


def ffmpeg_samples(ffmpeg):
    e = ffmpeg["exe"]

    def f(*args):
        # -nostdin so a failure cannot block; -y to overwrite; errors only.
        return [e, "-nostdin", "-y", "-loglevel", "error"] + list(args)

    vid = ["-f", "lavfi", "-i", "testsrc=size=128x96:rate=10:duration=1"]
    aud = ["-f", "lavfi", "-i", "sine=frequency=440:duration=1"]
    return [
        # --- MP4: faststart moves moov ahead of mdat, which is the single
        # most consequential structural variation for the box walk ----------
        (
            "independent/ff_h264.mp4",
            "mp4",
            f(*vid, "-c:v", "libx264", "-pix_fmt", "yuv420p", "{out}"),
        ),
        (
            "independent/ff_faststart.mp4",
            "mp4",
            f(
                *vid, "-c:v", "libx264", "-pix_fmt", "yuv420p",
                "-movflags", "+faststart", "{out}",
            ),
        ),
        (
            "independent/ff_mpeg4.mp4",
            "mp4",
            f(*vid, "-c:v", "mpeg4", "{out}"),
        ),
        (
            "independent/ff_av.mp4",
            "mp4",
            f(*vid, *aud, "-c:v", "libx264", "-c:a", "aac",
              "-pix_fmt", "yuv420p", "-shortest", "{out}"),
        ),
        # --- WAV: sample width and channel count change the fmt chunk ------
        ("independent/ff_pcm16.wav", "wav", f(*aud, "-c:a", "pcm_s16le", "{out}")),
        (
            "independent/ff_pcm24.wav",
            "wav",
            f(*aud, "-c:a", "pcm_s24le", "{out}"),
        ),
        (
            "independent/ff_stereo8.wav",
            "wav",
            f(*aud, "-ac", "2", "-c:a", "pcm_u8", "{out}"),
        ),
        # --- AVI: RIFF with LIST hdrl and LIST movi ------------------------
        ("independent/ff_mjpeg.avi", "avi", f(*vid, "-c:v", "mjpeg", "{out}")),
        (
            "independent/ff_rawvideo.avi",
            "avi",
            f(*vid, "-c:v", "rawvideo", "{out}"),
        ),
        # --- AIFF: the big-endian mirror of WAV, and the format most likely
        # to expose a byte-order slip in a shared chunk walk ------------------
        ("independent/ff_pcm16.aiff", "aiff", f(*aud, "-c:a", "pcm_s16be", "{out}")),
        # --- a second, independent encoder for three formats ImageMagick
        # also covers, so neither tool is a single point of agreement -------
        (
            "independent/ff_frame.png",
            "png",
            f(*vid, "-frames:v", "1", "{out}"),
        ),
        (
            "independent/ff_frame.jpg",
            "jpeg",
            f(*vid, "-frames:v", "1", "-q:v", "3", "{out}"),
        ),
        (
            "independent/ff_frame.webp",
            "webp",
            f(*vid, "-frames:v", "1", "{out}"),
        ),
        (
            "independent/ff_frame.bmp",
            "bmp",
            f(*vid, "-frames:v", "1", "{out}"),
        ),
    ]


# --------------------------------------------------------------------------


def _copy_if_magic(src, magic):
    """Read `src` and return its bytes only if they start with `magic`.

    Used for samples taken from files the operating system happens to ship. A
    path that exists is not evidence the file is what its extension claims, so
    the magic is checked before the sample is accepted rather than after a test
    fails confusingly.
    """
    try:
        with open(src, "rb") as fh:
            data = fh.read()
    except OSError:
        return None
    return data if data.startswith(magic) else None


def system_samples(outdir):
    """Samples produced or supplied by the operating system.

    Some formats have no free command-line encoder but are already present on a
    normal machine, written by software that certainly did not consult this
    project: Windows ships a licence in RTF, `makecab` is a built-in cabinet
    writer, and any Linux system binary is an ELF. Those count as foreign
    encoders in exactly the sense that matters.

    Returns (written, unavailable) where each written entry is
    (relpath, kind, description, bytes).
    """
    written = []
    unavailable = []

    # --- RTF: Windows ships its own licence as one ------------------------
    rtf = None
    for cand in (
        os.path.join(os.environ.get("SystemRoot", ""), "System32", "license.rtf"),
        "/usr/share/doc/license.rtf",
    ):
        if cand and os.path.isfile(cand):
            rtf = _copy_if_magic(cand, b"{\\rtf")
            if rtf:
                written.append(
                    ("independent/system_license.rtf", "rtf",
                     "shipped by the operating system", rtf)
                )
                break
    if not rtf:
        unavailable.append("rtf (no system-supplied .rtf found and no free CLI writer)")

    # --- CAB: makecab is a Windows built-in --------------------------------
    cab_exe = _find_exe("makecab")
    if cab_exe:
        srcfile = os.path.join(outdir, "_cabsrc.txt")
        os.makedirs(outdir, exist_ok=True)
        with open(srcfile, "wb") as fh:
            # Compressible text, so the cabinet exercises its own compression
            # rather than storing a blob.
            fh.write(b"".join(b"cabinet sample line %04d\r\n" % i for i in range(2000)))
        dest = os.path.join(outdir, "_out.cab")
        r = _run([cab_exe, srcfile, dest])
        data = _copy_if_magic(dest, b"MSCF") if r.returncode == 0 else None
        for tmp in (srcfile, dest, os.path.join(os.getcwd(), "setup.inf"),
                    os.path.join(os.getcwd(), "setup.rpt")):
            try:
                os.remove(tmp)
            except OSError:
                pass
        if data:
            written.append(
                ("independent/system.cab", "cab", "Windows makecab", data)
            )
        else:
            unavailable.append("cab (makecab is present but produced nothing usable)")
    else:
        unavailable.append("cab (no makecab, lcab or gcab on this platform)")

    # --- ELF: any Linux system binary --------------------------------------
    elf = None
    for cand in ("/bin/ls", "/usr/bin/ls", "/bin/true"):
        elf = _copy_if_magic(cand, b"\x7FELF")
        if elf:
            written.append(
                ("independent/system_binary.elf", "elf",
                 "a Linux system binary (%s)" % cand, elf)
            )
            break
    if not elf and os.name == "nt":
        # WSL, if it is installed, is a Linux filesystem this machine can read.
        r = _run(["wsl", "-d", "Ubuntu", "--", "cat", "/bin/ls"])
        if r.returncode == 0 and r.stdout.startswith(b"\x7FELF"):
            elf = r.stdout
            written.append(
                ("independent/system_binary.elf", "elf",
                 "a Linux system binary via WSL", elf)
            )
    if not elf:
        unavailable.append("elf (no Linux binary reachable from this machine)")

    # --- 7z: only if a 7-Zip binary is installed ---------------------------
    sevenzip = _find_exe("7z") or _find_exe("7za")
    if sevenzip:
        srcfile = os.path.join(outdir, "_7zsrc.bin")
        os.makedirs(outdir, exist_ok=True)
        with open(srcfile, "wb") as fh:
            fh.write(b"".join(b"seven zip sample %04d\n" % i for i in range(2000)))
        dest = os.path.join(outdir, "_out.7z")
        try:
            os.remove(dest)
        except OSError:
            pass
        r = _run([sevenzip, "a", "-bso0", "-bsp0", dest, srcfile])
        data = _copy_if_magic(dest, b"7z\xBC\xAF\x27\x1C") if r.returncode == 0 else None
        for tmp in (srcfile, dest):
            try:
                os.remove(tmp)
            except OSError:
                pass
        if data:
            written.append(("independent/system.7z", "7z", "7-Zip", data))
        else:
            unavailable.append("7z (a 7z binary is present but produced nothing usable)")
    else:
        unavailable.append(
            "7z (7-Zip is not installed; its Windows installer requires elevation, "
            "so an unattended session cannot add it)"
        )

    # --- formats with no reachable foreign producer ------------------------
    unavailable.append(
        "rar (no free writer exists; WinRAR is proprietary and trialware)"
    )
    unavailable.append(
        "reg_hive (`reg save` needs SeBackupPrivilege, which an unelevated "
        "session does not hold)"
    )
    unavailable.append(
        "evtx (the system event logs are not readable without elevation, and "
        "nothing else writes the format)"
    )
    return written, unavailable


def native_pe():
    """A Windows executable produced by a real linker.

    There is no way to *generate* a PE without a toolchain, and copying a system
    binary into a committed corpus would be both large and a licensing question.
    But the running Python interpreter is itself a PE on Windows, built by
    MSVC's linker, and copying it into a scratch directory for the length of a
    test raises neither problem. It is exactly what this corpus is for: a file
    this project had no hand in writing.

    On any other platform that interpreter is an ELF, so PE coverage is
    genuinely unavailable rather than merely missing, and the caller is told so
    rather than left to guess.
    """
    if os.name != "nt":
        return None
    exe = sys.executable
    if not exe or not os.path.isfile(exe):
        return None
    return {"exe": exe, "version": "system linker (%s)" % os.path.basename(exe)}


def probe():
    return {
        "imagemagick": find_magick(),
        "ffmpeg": find_ffmpeg(),
        "native_pe": native_pe(),
    }


def build(outdir):
    tools = probe()
    planned = {p for p, _, _ in magick_samples({"exe": "magick"})} | {
        p for p, _, _ in ffmpeg_samples({"exe": "ffmpeg"})}
    unknown = sorted(set(PROPERTIES) - planned)
    assert not unknown, "PROPERTIES names files nothing generates: %s" % unknown
    magick, ffmpeg = tools["imagemagick"], tools["ffmpeg"]

    plan = []
    missing = []
    unavailable = []
    if magick:
        plan += [(p, k, a, "imagemagick") for p, k, a in magick_samples(magick)]
    else:
        missing.append("ImageMagick (jpeg, png, bmp, webp, pdf, ico)")
    if ffmpeg:
        plan += [(p, k, a, "ffmpeg") for p, k, a in ffmpeg_samples(ffmpeg)]
    else:
        missing.append("ffmpeg (mp4, wav, avi)")

    manifest = {}

    # A PE is copied rather than generated; see native_pe().
    pe = tools["native_pe"]
    if pe:
        dest = os.path.join(outdir, "independent", "system_binary.exe")
        os.makedirs(os.path.dirname(dest), exist_ok=True)
        shutil.copyfile(pe["exe"], dest)
        with open(dest, "rb") as fh:
            data = fh.read()
        manifest["independent/system_binary.exe"] = {
            "sha256": hashlib.sha256(data).hexdigest(),
            "size": len(data),
            "kind": "pe",
            "generator": "system-linker",
            "generator_version": pe["version"],
        }
    else:
        unavailable.append(
            "pe (a Windows executable cannot be produced on %s; the PE validator "
            "is graded by hand-built vectors here)" % os.name
        )

    # Samples the operating system produces or already holds.
    system_written, system_unavailable = system_samples(outdir)
    unavailable.extend(system_unavailable)
    for rel, kind, description, data in system_written:
        dest = os.path.join(outdir, rel.replace("/", os.sep))
        os.makedirs(os.path.dirname(dest), exist_ok=True)
        with open(dest, "wb") as fh:
            fh.write(data)
        manifest[rel] = {
            "sha256": hashlib.sha256(data).hexdigest(),
            "size": len(data),
            "kind": kind,
            "generator": "system",
            "generator_version": description,
        }

    failures = []
    for rel, kind, argv, tool in plan:
        dest = os.path.join(outdir, rel.replace("/", os.sep))
        os.makedirs(os.path.dirname(dest), exist_ok=True)
        cmd = [a.replace("{out}", dest) for a in argv]
        r = _run(cmd)
        if r.returncode != 0 or not os.path.exists(dest) or os.path.getsize(dest) == 0:
            failures.append(
                {
                    "path": rel,
                    "kind": kind,
                    "tool": tool,
                    "exit": r.returncode,
                    "stderr": r.stderr.decode("utf-8", "replace")[-400:].strip(),
                }
            )
            continue
        with open(dest, "rb") as fh:
            data = fh.read()
        missing_property = check_property(rel, data)
        if missing_property:
            failures.append(
                {"path": rel, "kind": kind, "tool": tool, "exit": 0,
                 "stderr": missing_property}
            )
            continue
        manifest[rel] = {
            "sha256": hashlib.sha256(data).hexdigest(),
            "size": len(data),
            "kind": kind,
            "generator": tool,
            "generator_version": (magick if tool == "imagemagick" else ffmpeg)["version"],
        }

    return {
        "files": manifest,
        "tools": {
            k: (v["version"] if v else None) for k, v in tools.items()
        },
        # Encoders that could be installed and are not. A hard failure.
        "missing_tools": missing,
        # Coverage this platform cannot provide at all, which is a different
        # thing from a missing install and is reported as such rather than
        # quietly reducing what the suite claims to check.
        "unavailable_here": unavailable,
        "failures": failures,
    }


def main(argv):
    args = [a for a in argv[1:] if not a.startswith("--")]
    flags = [a for a in argv[1:] if a.startswith("--")]

    if "--probe" in flags:
        json.dump(probe(), sys.stdout, indent=2, sort_keys=True)
        sys.stdout.write("\n")
        return 0

    if len(args) != 1:
        sys.stderr.write(
            "usage: make_independent.py OUTDIR\n"
            "       make_independent.py --probe\n"
        )
        return 2

    outdir = args[0]
    os.makedirs(outdir, exist_ok=True)
    doc = build(outdir)
    json.dump(doc, sys.stdout, indent=2, sort_keys=True)
    sys.stdout.write("\n")

    # Loud on stderr, so a partial run is visible even when stdout is piped
    # into a parser that only looks at "files".
    for m in doc["missing_tools"]:
        sys.stderr.write("MISSING ENCODER: %s\n" % m)
    for m in doc["unavailable_here"]:
        sys.stderr.write("UNAVAILABLE ON THIS PLATFORM: %s\n" % m)
    for f in doc["failures"]:
        sys.stderr.write(
            "FAILED: %s (%s via %s, exit %s)\n  %s\n"
            % (f["path"], f["kind"], f["tool"], f["exit"], f["stderr"])
        )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
