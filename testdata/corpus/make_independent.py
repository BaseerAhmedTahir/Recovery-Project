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
    photo = ["-size", "128x96", "plasma:fractal"]
    return [
        # --- JPEG ----------------------------------------------------------
        ("independent/im_baseline.jpg", "jpeg", m(*photo, "-quality", "82", "{out}")),
        (
            "independent/im_progressive.jpg",
            "jpeg",
            m(*photo, "-interlace", "Plane", "-quality", "75", "{out}"),
        ),
        (
            "independent/im_restart.jpg",
            "jpeg",
            m(*photo, "-define", "jpeg:dct-method=float", "-quality", "90", "{out}"),
        ),
        ("independent/im_grey.jpg", "jpeg", m(*photo, "-colorspace", "Gray", "{out}")),
        # --- PNG -----------------------------------------------------------
        ("independent/im_rgb8.png", "png", m(*src, "-depth", "8", "{out}")),
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
