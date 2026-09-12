#!/usr/bin/env bash
#
# RECOVERY-CORE fixture generator (SPEC.md section 7).
#
# Produces synthetic disk images with recorded ground truth so the engine can be
# tested without touching real hardware. Each fixture emits two files:
#
#     <name>.img            the disk image
#     <name>.expected.json  ground truth: per-file sha256, size, deleted state,
#                           plus the image's own sha256 for the immutability test
#
# Must run inside Linux. On this machine that means WSL2; the images are written
# straight to the Windows D: drive because the WSL distro itself lives on a
# nearly-full C:. Loop devices work fine against /mnt/d.
#
# Usage:
#     ./build_fixtures.sh                 # build everything
#     ./build_fixtures.sh ntfs-basic      # build selected fixtures
#     ./build_fixtures.sh --list
#     RC_FIXTURE_SIZE=256M ./build_fixtures.sh

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib/common.sh
source "$HERE/lib/common.sh"

OUT_DIR="${RC_FIXTURE_DIR:-$HERE/fixtures}"
CORPUS_PY="$HERE/corpus/make_corpus.py"

# The toolchain that builds the corpus is part of the ground truth. The
# generator is byte-for-byte deterministic on one toolchain and not across
# toolchains: SQLite stamps its library version into every database header and
# DEFLATE output varies between zlib versions, which between them makes 58 of
# the 315 files differ in content at identical size. Recording this means a
# manifest paired with an image built elsewhere is a visible mismatch rather
# than 58 silently wrong hashes. See docs/LIMITATIONS.md section 2.7.
RC_PROVENANCE="$(python3 "$CORPUS_PY" --provenance)"
export RC_PROVENANCE
GENERATOR_VERSION=1

# Files deleted from every basic filesystem fixture, and therefore the set the
# Milestone 2 name-accuracy bar is measured against.
#
# Populated from `make_corpus.py --deleted-set` in main(), not restated here:
# the list contains non-ASCII names and a 250-character name, and maintaining
# those in two places is a typo waiting to happen.
DELETED_SET=()

# The corpus file deliberately fragmented before deletion. Also from Python.
FRAG_TARGET=""

# Fragmentation shape for that file. The volume is filled so the only free
# space left is a field of FRAG_HOLE_KIB holes, which forces the allocator to
# split the target across several of them.
FRAG_HOLE_KIB="${RC_FRAG_HOLE_KIB:-32}"
FRAG_SMALL_TOTAL_MIB="${RC_FRAG_SMALL_TOTAL_MIB:-48}"

ALL_FIXTURES=(
    ntfs-basic
    fat32-basic
    exfat-basic
    ext4-basic
    fragmented-jpeg
    fragmented-hard
    quickformat
    formats
    overwritten
    nopart
)

trap rc_cleanup EXIT

# ---------------------------------------------------------------------------
# helpers
# ---------------------------------------------------------------------------

in_deleted_set() {
    local needle="$1" x
    for x in "${DELETED_SET[@]}"; do [[ "$x" == "$needle" ]] && return 0; done
    return 1
}

# populate_corpus MOUNTPOINT SRCDIR [EXCLUDE_REL]
# Copies every corpus file in, syncing at the end so the allocator lays them
# down in a predictable order. An optional relative path is skipped, used to
# hold back the file that gets fragmented separately.
populate_corpus() {
    local mp="$1" src="$2" exclude="${3:-}"
    # -print0/read -d '' because the corpus contains names with spaces.
    (cd "$src" && find . -type f -print0 | sort -z | while IFS= read -r -d '' f; do
        rel="${f#./}"
        [[ -n "$exclude" && "$rel" == "$exclude" ]] && continue
        mkdir -p "$mp/$(dirname "$rel")"
        cp "$f" "$mp/$rel"
    done)
    sync
}

# fragment_one MOUNTPOINT SRCDIR REL
#
# Forces REL to be laid down in several non-adjacent runs.
#
# Simply writing a file onto a fresh volume gives a contiguous one, and merely
# deleting some filler is not enough either: allocators happily use the large
# contiguous tail instead of the holes. So the volume is filled almost
# completely, the remainder is filled with small files, and every other one is
# deleted. After that the *only* free space is a field of FRAG_HOLE_KIB holes,
# and the target cannot be stored contiguously.
fragment_one() {
    local mp="$1" src="$2" rel="$3" img="$4"
    local size_kib
    size_kib=$(( ( $(stat -c %s "$src/$rel") + 1023 ) / 1024 ))

    # Attempt with progressively harsher hole fields, verifying each one.
    #
    # The allocator has the last word here. ntfs-3g does not necessarily lay
    # filler files down in creation order, so freeing "every other file by
    # name" can coalesce into large contiguous runs rather than scattered
    # holes, and the target then lands contiguously - which is exactly what
    # happened once the corpus grew to 315 files. Rather than assume the
    # technique works, each attempt is checked against the image and retried.
    local attempt hole frac layout
    layout="unknown"
    for attempt in 1 2 3; do
        case "$attempt" in
            1) hole="$FRAG_HOLE_KIB"; frac=2 ;;   # free 1 file in 2
            2) hole=16;               frac=3 ;;   # smaller holes, free 2 in 3
            3) hole=8;                frac=4 ;;   # smaller still, free 3 in 4
        esac
        rc_log "fragmenting $rel (${size_kib}KiB): attempt $attempt, ${hole}KiB holes, freeing $((frac-1)) in $frac"

        rm -f "$mp/$rel"
        rm -rf "$mp/.smallfill" "$mp/.bigfill"
        sync

        FRAG_MP="$mp" FRAG_HOLE_KIB="$hole" FRAG_FRAC="$frac" \
        FRAG_SMALL_TOTAL_MIB="$FRAG_SMALL_TOTAL_MIB" python3 - <<'FRAGPY'
import os, random, sys

mp = os.environ["FRAG_MP"]
hole_kib = int(os.environ["FRAG_HOLE_KIB"])
frac = int(os.environ["FRAG_FRAC"])
small_total = int(os.environ["FRAG_SMALL_TOTAL_MIB"]) * 1024 * 1024

def free_bytes():
    st = os.statvfs(mp)
    return st.f_bavail * st.f_frsize

# 1. Consume the bulk of the volume, leaving only the future hole field.
big = os.path.join(mp, ".bigfill")
chunk = b"\xC3" * (4 * 1024 * 1024)
with open(big, "wb") as fh:
    while free_bytes() > small_total:
        want = min(len(chunk), max(0, free_bytes() - small_total))
        if want <= 0:
            break
        try:
            fh.write(chunk[:want])
            fh.flush()
        except OSError:
            break
os.sync()

# 2. Fill the remainder with small files until genuinely out of space.
small_dir = os.path.join(mp, ".smallfill")
os.makedirs(small_dir, exist_ok=True)
blob = b"\x5A" * (hole_kib * 1024)
written = 0
while True:
    try:
        with open(os.path.join(small_dir, "s%06d" % written), "wb") as fh:
            fh.write(blob)
        written += 1
    except OSError:
        break
os.sync()

# 3. Free a seeded-random subset rather than every Nth file by name.
#
#    Deleting by name parity assumes the filesystem allocated the files in
#    creation order. ntfs-3g does not guarantee that, and when the assumption
#    fails the freed space coalesces into large contiguous runs - the opposite
#    of what this fixture needs. A random subset scatters the holes whatever
#    the allocation order was. Seeded, so it stays reproducible.
rng = random.Random(0xF3A6 + hole_kib * 131 + frac)
victims = [i for i in range(written) if rng.randrange(frac) != 0]
holes = 0
for i in victims:
    try:
        os.unlink(os.path.join(small_dir, "s%06d" % i))
        holes += 1
    except OSError:
        pass
os.sync()
print("    filler=%d holes=%d free=%.1fMiB"
      % (written, holes, free_bytes() / 1048576.0), file=sys.stderr)
FRAGPY

        # Write the target into the hole field and see what the allocator did.
        mkdir -p "$mp/$(dirname "$rel")"
        cp "$src/$rel" "$mp/$rel"
        sync

        layout="$(fragment_check "$img" "$src/$rel")"
        rc_log "    attempt $attempt produced: $layout"
        if [[ "$layout" == "fragmented" ]]; then
            break
        fi
    done

    # Remove the filler. The target stays where it was allocated.
    rm -rf "$mp/.smallfill" "$mp/.bigfill"
    sync
}

# fragment_check IMAGE SRCFILE -> echoes "contiguous", "fragmented" or "absent"
#
# Answers the question directly from the finished image rather than asking the
# filesystem driver, because the driver cannot be asked reliably: filefrag
# falls back to FIBMAP on vfat and exfat (root only) and neither ntfs-3g nor
# exfat-fuse implement FIEMAP at all. Only ext4 answers unprivileged.
#
# Searching the image is also the more useful question. "Do this file's bytes
# appear as one contiguous run on the disk?" is exactly what decides whether
# contiguous carving can recover it, which is what Milestones 3 and 4 are
# graded on. A filesystem extent count is a proxy for that; this is the thing
# itself.
fragment_check() {
    local img="$1" src="$2"
    RC_IMG="$img" RC_SRC="$src" python3 - <<'PY'
import os, sys

img = os.environ["RC_IMG"]
src = os.environ["RC_SRC"]
data = open(src, "rb").read()
if len(data) < 8192:
    print("absent")
    sys.exit(0)

needle = data[:4096]
with open(img, "rb") as fh:
    blob = fh.read()

# Every place the file's first 4 KiB appears. Random-ish content makes a
# spurious hit vanishingly unlikely, but check them all rather than assume.
pos = blob.find(needle)
while pos != -1:
    if blob[pos:pos + len(data)] == data:
        print("contiguous")
        sys.exit(0)
    pos = blob.find(needle, pos + 1)

# The head is present but the whole file is not stored in one run.
print("fragmented" if blob.find(needle) != -1 else "absent")
PY
}

# verify_populated MOUNTPOINT SRCDIR
#
# Confirm every corpus file is present on the volume under exactly the name it
# was given, before anything is deleted.
#
# Without this the ground truth could be a lie: expected.json records the
# corpus's paths, but a filesystem that mangles a name on the way in (vfat
# without utf8=1 turning non-ASCII into underscores, a driver truncating a
# 250-character name) would leave the manifest claiming a file that is not
# there under that name. The parser would then be graded against a name no
# recovery tool could ever produce.
verify_populated() {
    local mp="$1" src="$2"
    RC_MP="$mp" RC_SRC="$src" python3 - <<'PY'
import os, sys

mp = os.environ["RC_MP"]
src = os.environ["RC_SRC"]

def walk(root):
    out = set()
    for dirpath, _dirs, files in os.walk(root):
        for f in files:
            rel = os.path.relpath(os.path.join(dirpath, f), root)
            out.add(rel.replace(os.sep, "/"))
    return out

want = walk(src)
got = walk(mp)
missing = sorted(want - got)
extra = sorted(g for g in (got - want) if not g.startswith("."))

if missing:
    print("  MISSING from the volume (%d):" % len(missing), file=sys.stderr)
    for m in missing[:10]:
        print("    %r" % m, file=sys.stderr)
if extra:
    print("  UNEXPECTED on the volume (%d):" % len(extra), file=sys.stderr)
    for e in extra[:10]:
        print("    %r" % e, file=sys.stderr)
if missing or extra:
    sys.exit(1)
print("  verified %d files present under their exact names" % len(want), file=sys.stderr)
PY
}

# delete_subset MOUNTPOINT
delete_subset() {
    local mp="$1" f
    for f in "${DELETED_SET[@]}"; do
        rm -f "$mp/$f"
    done
    sync
}

# write_expected NAME FS IMG CORPUS_MANIFEST PARTITIONED NOTES [EXTRA_JSON]
#
# Merges the corpus manifest with per-fixture deleted/present state and the
# image's post-build sha256.
write_expected() {
    local name="$1" fs="$2" img="$3" cmanifest="$4" partitioned="$5" notes="$6"
    local extra="${7:-{\}}"
    local deleted_json
    # Read as bytes and split on newline only. `.strip()` here would silently
    # eat leading and trailing spaces, and the corpus contains a filename with
    # a space in it precisely because that is a thing real filesystems allow.
    deleted_json="$(printf '%s\n' "${DELETED_SET[@]}" | python3 -c \
        'import sys, json
raw = sys.stdin.buffer.read().decode("utf-8")
print(json.dumps([l for l in raw.split("\n") if l]))')"

    RC_NAME="$name" RC_FS="$fs" RC_IMG="$img" \
    RC_SHA="$(rc_sha256 "$img")" RC_BYTES="$(stat -c %s "$img")" \
    RC_SECTOR="$RC_SECTOR_BYTES" RC_CLUSTER="$RC_CLUSTER_BYTES" \
    RC_PART="$partitioned" RC_NOTES="$notes" RC_DELETED="$deleted_json" \
    RC_GENVER="$GENERATOR_VERSION" RC_EXTRA="$extra" \
    python3 - "$cmanifest" > "${img%.img}.expected.json" <<'PY'
import json, os, sys, datetime

corpus = json.load(open(sys.argv[1]))
deleted = set(json.loads(os.environ["RC_DELETED"]))

files = {}
for path, meta in corpus.items():
    files[path] = {
        "sha256": meta["sha256"],
        "size": meta["size"],
        "kind": meta["kind"],
        "state": "deleted" if path in deleted else "present",
    }

doc = {
    "fixture": os.environ["RC_NAME"],
    "filesystem": os.environ["RC_FS"],
    "image": os.path.basename(os.environ["RC_IMG"]),
    "image_sha256": os.environ["RC_SHA"],
    "image_bytes": int(os.environ["RC_BYTES"]),
    "sector_bytes": int(os.environ["RC_SECTOR"]),
    "cluster_bytes": int(os.environ["RC_CLUSTER"]),
    "partitioned": os.environ["RC_PART"] == "true",
    "generator": {
        "script": "build_fixtures.sh",
        "version": int(os.environ["RC_GENVER"]),
        "built_utc": datetime.datetime.now(datetime.timezone.utc)
                     .replace(microsecond=0).isoformat(),
        "provenance": json.loads(os.environ["RC_PROVENANCE"]),
    },
    "notes": os.environ["RC_NOTES"],
    "files": files,
    "expect": {
        "deleted_count": sum(1 for f in files.values() if f["state"] == "deleted"),
        "present_count": sum(1 for f in files.values() if f["state"] == "present"),
    },
}
doc.update(json.loads(os.environ["RC_EXTRA"]))
json.dump(doc, sys.stdout, indent=2, sort_keys=True)
sys.stdout.write("\n")
PY
}

# ---------------------------------------------------------------------------
# fixture builders
# ---------------------------------------------------------------------------

# build_basic NAME FSTYPE LABEL
#
# Format, copy the whole corpus in, delete a fixed subset, unmount. The deleted
# files' contents are still on disk, which is what Milestones 2 and 3 recover.
build_basic() {
    local name="$1" fs="$2" label="$3"
    local img="$OUT_DIR/$name.img"
    rc_step "$name ($fs)"

    rc_image_create "$img" >/dev/null
    local dev mp
    dev="$(rc_loop_attach "$img")"
    rc_log "loop $dev"
    rc_mkfs "$fs" "$dev" "$label"
    mp="$(rc_mount "$fs" "$dev")"

    # Everything except the fragmentation target, which is written separately
    # so it can be forced into non-contiguous runs.
    populate_corpus "$mp" "$CORPUS_DIR" "$FRAG_TARGET"
    rc_log "copied $(( $(find "$CORPUS_DIR" -type f | wc -l) - 1 )) files"
    fragment_one "$mp" "$CORPUS_DIR" "$FRAG_TARGET" "$img"

    # Names must survive the write before the ground truth can claim them.
    verify_populated "$mp" "$CORPUS_DIR" \
        || rc_die "$name: the volume does not hold the corpus under its exact names"

    delete_subset "$mp"
    rc_log "deleted ${#DELETED_SET[@]} files"

    rc_umount "$mp"
    rc_loop_detach "$dev"

    # Verified against the finished image, not asserted.
    local layout
    layout="$(fragment_check "$img" "$CORPUS_DIR/$FRAG_TARGET")"
    rc_log "$FRAG_TARGET layout on disk: $layout"
    [[ "$layout" == "fragmented" ]] || rc_log "WARNING: expected a fragmented layout, got '$layout'"

    local extra
    extra="$(RC_LAYOUT="$layout" RC_TGT="$FRAG_TARGET" RC_HOLE="$FRAG_HOLE_KIB" python3 -c '
import json, os
layout = os.environ["RC_LAYOUT"]
print(json.dumps({"fragmented_file": {
    "path": os.environ["RC_TGT"],
    "hole_kib": int(os.environ["RC_HOLE"]),
    "layout": layout,
    "verified": layout in ("contiguous", "fragmented"),
    "note": ("Written into a field of holes so it could not be stored "
             "contiguously, then deleted. Verified by searching the finished "
             "image for the file bytes as one contiguous run, which is "
             "filesystem-independent and is exactly the property contiguous "
             "carving depends on. FAT zeroes the cluster chain on delete, so "
             "assuming contiguity recovers the wrong bytes for this file."),
}}))')"

    write_expected "$name" "$fs" "$img" "$CORPUS_MANIFEST" false \
        "Whole-device $fs filesystem, no partition table. Corpus copied in full, then a fixed subset deleted. One file was deliberately fragmented before deletion." \
        "$extra"
    rc_log "wrote $(basename "${img%.img}.expected.json")"
}

# Fragmentation: fill the volume with fixed-size filler files, punch alternating
# holes by deleting every other one, then write large media into the holes. The
# allocator has no contiguous run big enough, so each large file is split.
# Fragment the media by writing it into a volume full of alternating holes.
#
# MODE decides what sits in the gaps between a file's fragments, and it is the
# difference between a test and a formality:
#
#   constant  every filler file is one repeated byte. The gaps have zero
#             entropy against file data at 7.8-7.96 bits per byte, so "skip the
#             clusters that are all one value" reassembles everything without
#             understanding a single format. Kept as a sanity check, not as
#             evidence.
#   decoy     every filler file is a slice of *other real media* - JPEGs with
#             the same restart interval, H.264 MP4s, PNGs. A gap then looks like
#             the file it interrupts, which is what a real disk looks like, and
#             a reassembler has to use format structure to tell them apart.
#
# SIZES decides how big the holes (where fragments land) and the gaps (the
# filler left between them) are:
#
#   fixed     every filler file is HOLE_KIB. Every gap between two fragments is
#             then the same size, which a search window can be tuned to without
#             anyone noticing - fine for the sanity check, not for a result.
#   varied    holes of 8-64 KiB and gaps of 4 KiB to 1 MiB, drawn from a seeded
#             generator: fragments of different lengths, and gaps from one
#             cluster to 256 of them.
build_fragmented() {
    local name="${1:-fragmented-jpeg}" mode="${2:-constant}" sizes="${3:-fixed}" fs="vfat"
    local img="$OUT_DIR/$name.img"
    rc_step "$name ($fs, $sizes sizes, gap content: $mode)"

    rc_image_create "$img" >/dev/null
    local dev mp
    dev="$(rc_loop_attach "$img")"
    rc_mkfs "$fs" "$dev" RCFRAG
    mp="$(rc_mount "$fs" "$dev")"

    # Fill with filler files, then punch alternating holes. Both loops run
    # inside one Python process: a volume this size needs thousands of files,
    # and spawning dd/rm per file makes the build take tens of minutes.
    local free_kib n
    free_kib="$(df -k --output=avail "$mp" | tail -1)"
    n=$(( free_kib / 4 ))
    rc_log "filling with $sizes-size filler files, then deleting every other one"
    python3 - "$mp/filler" "$n" "$HOLE_KIB" "$mode" "$sizes" "${DECOY_DIR:-}" <<'PY'
import os, random, sys
d, n, kib, mode, sizes, decoy_dir = (sys.argv[1], int(sys.argv[2]), int(sys.argv[3]),
                                     sys.argv[4], sys.argv[5], sys.argv[6])
os.makedirs(d, exist_ok=True)

# Even-numbered filler files are deleted (holes, where fragments land); odd ones
# stay until the media are written (gaps, what lies between two fragments).
if sizes == "fixed":
    def size_for(i):
        return kib * 1024
elif sizes == "varied":
    rng = random.Random(0x9A95)
    HOLES_KIB = [8, 16, 32, 64]
    GAPS_KIB = [4, 16, 64, 256, 1024]
    table = [(rng.choice(HOLES_KIB) if i % 2 == 0 else rng.choice(GAPS_KIB)) * 1024
             for i in range(n)]
    def size_for(i):
        return table[i]
else:
    raise SystemExit("unknown filler sizes: " + sizes)

if mode == "decoy":
    # One stream of real media in a fixed order, sliced into filler files.
    # Sorted so the fixture is reproducible.
    stream = bytearray()
    for root, _dirs, files in sorted(os.walk(decoy_dir)):
        for f in sorted(files):
            with open(os.path.join(root, f), "rb") as fh:
                stream += fh.read()
    stream = bytes(stream)
    cursor = [0]
    def blob_for(i):
        size = size_for(i)
        if len(stream) < size:
            raise SystemExit("decoy pool is smaller than one filler file")
        start = cursor[0] % len(stream)
        cursor[0] += size
        piece = stream[start:start + size]
        return piece + stream[:size - len(piece)]
elif mode == "constant":
    def blob_for(i):
        return b"\xA5" * size_for(i)
else:
    raise SystemExit("unknown filler mode: " + mode)
written = 0
for i in range(n):
    try:
        with open(os.path.join(d, "f%05d" % i), "wb") as fh:
            fh.write(blob_for(i))
        written += 1
    except OSError:
        break            # volume full: stop here, the holes below still work
os.sync()
holes = 0
for i in range(0, written, 2):
    try:
        os.unlink(os.path.join(d, "f%05d" % i))
        holes += 1
    except OSError:
        pass
os.sync()
print("    filler written=%d holes=%d" % (written, holes), file=sys.stderr)
PY
    sync

    # Write the large media into the fragmented free space.
    mkdir -p "$mp/frag"
    cp "$FRAG_DIR"/frag/* "$mp/frag/" 2>/dev/null || cp -r "$FRAG_DIR"/* "$mp/"
    sync
    rc_log "wrote fragmented media"

    # Remove the remaining filler so the deleted media dominate the free space,
    # then delete the media themselves: they are now fragmented AND deleted.
    rm -rf "$mp/filler"
    sync
    local f
    for f in "$mp"/frag/*; do rm -f "$f"; done
    sync

    rc_umount "$mp"
    rc_loop_detach "$dev"

    local extra
    extra="$(RC_MODE="$mode" RC_SIZES="$sizes" python3 -c '
import json, os
mode, sizes = os.environ["RC_MODE"], os.environ["RC_SIZES"]
where = {
    "fixed": "alternating %s KiB holes and gaps" % os.environ["HOLE_KIB"],
    "varied": "holes of 8-64 KiB separated by gaps of 4 KiB to 1 MiB",
}[sizes]
what = {
    "constant": ("every gap is one repeated byte. A sanity check only: skipping "
                 "uniform clusters reassembles it without understanding any format."),
    "decoy": ("every gap holds a slice of other real media from the same encoders "
              "with the same settings - same Huffman tables, restart interval and "
              "sampling, same H.264 profile. Gaps look like the files they "
              "interrupt, so reassembly has to use format structure."),
}[mode]
frag = {"method": "alternating-filler-holes", "sizes": sizes,
        "gap_content": mode, "filesystem": "vfat"}
if sizes == "fixed":
    frag["hole_kib"] = int(os.environ["HOLE_KIB"])
else:
    frag["hole_kib_choices"] = [8, 16, 32, 64]
    frag["gap_kib_choices"] = [4, 16, 64, 256, 1024]
print(json.dumps({
    "fragmentation": frag,
    "notes": "Media fragmented into %s; %s" % (where, what),
}))')"
    RC_DELETED_OVERRIDE=1 write_frag_expected "$name" "$fs" "$img" "$extra"
    embed_layout "$img"
}

# Record where every fragmented file actually landed.
#
# The fragmentation method says where files are *expected* to break. This says
# where they did: every cluster of every file located in the image, grouped
# into runs. Reassembly is graded against it, because a hash match says only
# whether a file came back whole, and the layout says how close a miss was.
embed_layout() {
    local img="$1" json="${1%.img}.expected.json"
    local layout
    layout="$(python3 "$HERE/fragmap.py" "$img" "$json" "$FRAG_DIR")" \
        || rc_die "could not recover the fragment layout of $img"
    RC_LAYOUT="$layout" python3 - "$json" <<'PY'
import json, os, sys
path = sys.argv[1]
doc = json.load(open(path, encoding="utf-8"))
doc["layout"] = json.loads(os.environ["RC_LAYOUT"])
# A file that did not actually fragment would make its row in any reassembly
# result meaningless, so say so rather than letting it pass as a success.
single = [p for p, l in doc["layout"]["files"].items() if l["fragment_count"] < 2]
if single:
    raise SystemExit("these files did not fragment: %s" % ", ".join(single))
json.dump(doc, open(path, "w", encoding="utf-8"), indent=2, sort_keys=True)
PY
    rc_log "recorded the true fragment layout"
}

# The fragmented fixture uses the frag corpus and deletes all of it, so it needs
# its own manifest writer rather than the shared DELETED_SET logic.
write_frag_expected() {
    local name="$1" fs="$2" img="$3" extra="$4"
    RC_NAME="$name" RC_FS="$fs" RC_IMG="$img" \
    RC_SHA="$(rc_sha256 "$img")" RC_BYTES="$(stat -c %s "$img")" \
    RC_SECTOR="$RC_SECTOR_BYTES" RC_CLUSTER="$RC_CLUSTER_BYTES" \
    RC_GENVER="$GENERATOR_VERSION" RC_EXTRA="$extra" RC_HOLE="$HOLE_KIB" \
    python3 - "$FRAG_MANIFEST" > "${img%.img}.expected.json" <<'PY'
import json, os, sys, datetime
corpus = json.load(open(sys.argv[1]))
files = {p: {"sha256": m["sha256"], "size": m["size"], "kind": m["kind"],
             "state": "deleted"} for p, m in corpus.items()}
doc = {
    "fixture": os.environ["RC_NAME"],
    "filesystem": os.environ["RC_FS"],
    "image": os.path.basename(os.environ["RC_IMG"]),
    "image_sha256": os.environ["RC_SHA"],
    "image_bytes": int(os.environ["RC_BYTES"]),
    "sector_bytes": int(os.environ["RC_SECTOR"]),
    "cluster_bytes": int(os.environ["RC_CLUSTER"]),
    "partitioned": False,
    "generator": {"script": "build_fixtures.sh",
                  "version": int(os.environ["RC_GENVER"]),
                  "built_utc": datetime.datetime.now(datetime.timezone.utc)
                               .replace(microsecond=0).isoformat(),
                  "provenance": json.loads(os.environ["RC_PROVENANCE"])},
    "notes": ("Volume filled with filler files, every other one deleted to "
              "punch holes, then large media written into the fragmented free "
              "space and deleted. Every listed file is expected to be "
              "fragmented; contiguous-only carving should recover none of them "
              "byte-exactly."),
    "files": files,
    "expect": {"deleted_count": len(files), "present_count": 0},
}
doc.update(json.loads(os.environ["RC_EXTRA"]))
json.dump(doc, sys.stdout, indent=2, sort_keys=True)
sys.stdout.write("\n")
PY
    rc_log "wrote $(basename "${img%.img}.expected.json")"
}

# Populate, then quick-format. Directory metadata is gone but file content is
# untouched, so signature carving should still recover nearly everything.
build_quickformat() {
    local name="quickformat" fs="ntfs"
    local img="$OUT_DIR/$name.img"
    rc_step "$name (populate $fs, then quick reformat)"

    rc_image_create "$img" >/dev/null
    local dev mp
    dev="$(rc_loop_attach "$img")"
    rc_mkfs "$fs" "$dev" RCORIG
    mp="$(rc_mount "$fs" "$dev")"
    populate_corpus "$mp" "$CORPUS_DIR"
    rc_umount "$mp"

    rc_log "quick-reformatting (metadata discarded, content left in place)"
    rc_mkfs "$fs" "$dev" RCWIPED
    rc_loop_detach "$dev"

    # A "quick format" that zeroed the volume would leave a fixture that tests
    # nothing at all, and it would pass silently. Confirm a known corpus file's
    # bytes are still present before recording the ground truth.
    local survived
    survived="$(fragment_check "$img" "$CORPUS_DIR/$FRAG_TARGET")"
    rc_log "post-format content check: $FRAG_TARGET is $survived"
    [[ "$survived" == "absent" ]] && rc_die         "the quick format destroyed the file content; this fixture would test nothing.
Check that mkfs.ntfs is being invoked with -Q." 

    RC_ALL_DELETED=1 write_all_deleted_expected "$name" "$fs" "$img" \
        "Corpus written to an NTFS volume, then quick-reformatted. All filesystem metadata for the original files is gone; content remains and should be recoverable by signature carving in Milestone 3."
}

# Populate, delete everything, then overwrite part of the free space so some
# deleted files are provably destroyed and others provably intact. This is the
# fixture the Milestone 5 Green/Yellow/Red classifier is graded against.
# One file of every carvable format, then a quick reformat.
#
# The basic corpus is deliberately hard and deliberately narrow: six carvable
# formats chosen for awkward structure. This is the other axis - broad and
# ordinary - and it exists so that a signature which cannot fire is a failing
# test rather than a line of JSON nobody reads. The `vhd` entry read "connecti"
# for a cookie that is "conectix" and could never have matched anything; it was
# found by eye. This fixture is the guard.
#
# Every sample is at least 8 KiB by construction, because NTFS keeps a smaller
# file's data inside its MFT record and a quick format discards the MFT. A
# corpus of small samples would test nothing at all, and would pass while doing
# it.
build_formats() {
    local name="formats" fs="ntfs"
    local img="$OUT_DIR/$name.img"
    rc_step "$name (one file per format on $fs, then quick reformat)"

    rc_image_create "$img" >/dev/null
    local dev mp
    dev="$(rc_loop_attach "$img")"
    rc_mkfs "$fs" "$dev" RCFMTORIG
    mp="$(rc_mount "$fs" "$dev")"
    populate_corpus "$mp" "$FORMATS_DIR"
    verify_populated "$mp" "$FORMATS_DIR"
    rc_umount "$mp"

    rc_log "quick-reformatting (metadata discarded, content left in place)"
    rc_mkfs "$fs" "$dev" RCFMTWIPED
    rc_loop_detach "$dev"

    # Same guard as the quickformat fixture: a format that zeroed the volume
    # would leave a fixture that tests nothing and passes silently.
    local survived
    survived="$(fragment_check "$img" "$FORMATS_DIR/formats/sample.gz")"
    rc_log "post-format content check: formats/sample.gz is $survived"
    [[ "$survived" == "absent" ]] && rc_die \
        "the quick format destroyed the file content; this fixture would test nothing."

    RC_ALL_DELETED=1 write_all_deleted_expected "$name" "$fs" "$img" \
        "One file of every format in the signature database, written to an NTFS volume and then quick-reformatted. All filesystem metadata is gone; content remains. This is the format-coverage half of the Milestone 3 criterion - the basic corpus covers depth, this covers breadth." \
        "$FORMATS_MANIFEST"
}

build_overwritten() {
    local name="overwritten" fs="ntfs"
    local img="$OUT_DIR/$name.img"
    rc_step "$name (populate, delete, overwrite three ways, record what happened)"

    # Ground truth is what happened to each deleted file, not the method: see
    # testdata/overmap.py. Until Milestone 5 this fixture recorded only a rule,
    # and measured on the image it produced, no file was partially overwritten
    # at all - the YELLOW band had no case.
    local tmp
    tmp="$(mktemp -d)"

    rc_image_create "$img" >/dev/null
    local dev mp
    dev="$(rc_loop_attach "$img")"
    rc_mkfs "$fs" "$dev" RCOVER
    mp="$(rc_mount "$fs" "$dev")"
    populate_corpus "$mp" "$CORPUS_DIR"
    delete_subset "$mp"
    rc_umount "$mp"

    # 1. Before anything is overwritten, locate every cluster of every deleted
    #    file by content.
    printf '%s\n' "${DELETED_SET[@]}" | python3 -c \
        'import sys, json
raw = sys.stdin.buffer.read().decode("utf-8")
print(json.dumps([l for l in raw.split("\n") if l]))' > "$tmp/deleted.json"
    python3 "$HERE/overmap.py" map "$img" "$CORPUS_DIR" "$CORPUS_MANIFEST" \
        "$tmp/deleted.json" "$RC_CLUSTER_BYTES" > "$tmp/before.json" \
        || rc_die "could not map the deleted files' clusters"
    rc_log "mapped the clusters of every deleted file"

    mp="$(rc_mount "$fs" "$dev")"
    # 2a. Overwrite roughly a third of the free space with a pattern, then free
    #     it again: clusters overwritten and unallocated, as after any
    #     temporary file.
    local free_kib third
    free_kib="$(df -k --output=avail "$mp" | tail -1)"
    third=$(( free_kib / 3 ))
    rc_log "overwriting ~${third}KiB of free space with 0xDB, then deleting it"
    python3 - "$mp/overwrite.bin" "$third" <<'PY'
import os, sys
path, kib = sys.argv[1], int(sys.argv[2])
chunk = b"\xDB" * (1024 * 1024)
left = kib * 1024
with open(path, "wb") as fh:
    while left > 0:
        try:
            fh.write(chunk[:min(len(chunk), left)])
        except OSError:
            break        # volume full is fine; we only need "a large fraction"
        left -= min(len(chunk), left)
os.sync()
PY
    sync
    rm -f "$mp/overwrite.bin"
    sync

    # 2b. A file written after the deletions and kept: whichever freed clusters
    #     it takes are now allocated to a live file - the RED case SPEC.md
    #     5.7 names first. Random bytes, so only allocation can tell.
    python3 "$HERE/overmap.py" livefile "$mp/live_after.bin" $(( 16 * 1024 * 1024 )) 4242 \
        "$RC_CLUSTER_BYTES" > "$tmp/live.json" \
        || rc_die "could not write the live file"
    sync
    rc_umount "$mp"
    rc_loop_detach "$dev"

    # 2c. Deliberate partial overwrites, straight into chosen files' clusters,
    #     so every band and every kind of evidence has a case.
    python3 "$HERE/overmap.py" damage "$img" "$tmp/before.json" 5151 "$CORPUS_MANIFEST" \
        > "$tmp/damage.json" || rc_die "could not apply the damage plan"
    rc_log "applied $(python3 -c 'import json,sys; print(len(json.load(open(sys.argv[1]))["cases"]))' "$tmp/damage.json") targeted overwrites"

    local extra
    extra='{"overwrite":{"pattern_byte":"0xDB","fraction_of_free_space":0.33,"live_file":"live_after.bin (16 MiB random, kept)","note":"Per-file outcomes are in overwrite_truth, measured after the build. The classification policy is in testdata/overmap.py classify()."}}'
    write_expected "$name" "$fs" "$img" "$CORPUS_MANIFEST" false \
        "Corpus written and a fixed subset deleted; then a third of free space overwritten with 0xDB and freed, a 16 MiB file written and kept, and eleven deliberate partial overwrites applied to chosen deleted files. Grades the Milestone 5 confidence classifier." \
        "$extra"

    # 3. What actually happened, per deleted file.
    python3 "$HERE/overmap.py" truth "$img" "$tmp/before.json" "$tmp/damage.json" "$tmp/live.json" \
        > "$tmp/truth.json" || rc_die "could not compute the overwrite truth"
    python3 - "${img%.img}.expected.json" "$tmp/truth.json" "$tmp/damage.json" "$tmp/live.json" <<'PY'
import json, sys
path, truth, damage = sys.argv[1], json.load(open(sys.argv[2])), json.load(open(sys.argv[3]))
live_file = json.load(open(sys.argv[4]))
doc = json.load(open(path, encoding="utf-8"))
doc["overwrite_truth"] = truth
doc["overwrite_damage"] = damage
# The live file is a real present file on the volume; say so, or the present
# count is wrong by one for anything that lists the volume.
doc["files"]["live_after.bin"] = {"kind": "binary", "sha256": live_file["sha256"],
                                  "size": live_file["bytes"], "state": "present"}
doc["expect"]["present_count"] = sum(1 for f in doc["files"].values() if f["state"] == "present")
counts = truth["counts"]
# A band with no case would grade nothing, which is the failure this fixture
# was rebuilt to fix. Refuse to write one.
missing = [b for b in ("GREEN", "YELLOW", "RED") if not counts.get(b)]
if missing:
    raise SystemExit("overwritten fixture has no %s case: %s" % (missing, counts))
live = sum(1 for f in truth["files"].values() if f["lost_to_live_file"])
if not live:
    raise SystemExit("no deleted file lost a cluster to the live file; allocation is untested")
json.dump(doc, open(path, "w", encoding="utf-8"), indent=2, sort_keys=True)
print("    classes: %s; %d file(s) lost clusters to the live file" % (counts, live), file=sys.stderr)
PY
    rm -rf "$tmp"
    rc_log "wrote $(basename "${img%.img}.expected.json") with per-file overwrite truth"
}

# A partitioned image whose partition table has been destroyed. rc-partition
# must rebuild it in memory by scanning for the NTFS boot sector.
build_nopart() {
    local name="nopart" fs="ntfs"
    local img="$OUT_DIR/$name.img"
    rc_step "$name (GPT + NTFS, then partition table wiped)"

    rc_image_create "$img" >/dev/null

    # The WSL2 loop driver runs with max_part=0, so /dev/loopNpM nodes never
    # appear and --partscan is useless here. Instead the GPT is written straight
    # into the image file (sfdisk on a regular file needs no privileges), and
    # the partition is then reached with a loop device windowed onto its byte
    # range via --offset/--sizelimit. Same on-disk result, no partition scanning.
    rc_log "writing GPT directly to the image file"
    printf 'label: gpt\nstart=%s, type=EBD0A0A2-B9E5-4433-87C0-68B6B72699C7\n' \
        "$PART_START_LBA" | sfdisk --quiet "$img" >/dev/null 2>&1 \
        || rc_die "sfdisk failed to write a GPT to $img"

    local img_bytes offset sizelimit dev mp
    img_bytes="$(stat -c %s "$img")"
    offset=$(( PART_START_LBA * RC_SECTOR_BYTES ))
    # Leave the trailing 33 sectors for the backup GPT.
    sizelimit=$(( img_bytes - offset - 33 * RC_SECTOR_BYTES ))

    dev="$(sudo -n losetup --find --show --offset "$offset" --sizelimit "$sizelimit" "$img")" \
        || rc_die "losetup with offset failed"
    RC_LOOPS+=("$dev")
    rc_log "loop $dev windowed at offset $offset size $sizelimit"

    rc_mkfs "$fs" "$dev" RCNOPART
    mp="$(rc_mount "$fs" "$dev")"
    populate_corpus "$mp" "$CORPUS_DIR"
    delete_subset "$mp"
    rc_umount "$mp"
    rc_loop_detach "$dev"

    # Destroy primary GPT + protective MBR (first 34 sectors) and the backup
    # GPT (last 33 sectors). The NTFS partition itself is left untouched.
    rc_log "wiping primary GPT (first 34 sectors) and backup GPT (last 33)"
    local total_sectors
    total_sectors=$(( $(stat -c %s "$img") / RC_SECTOR_BYTES ))
    dd if=/dev/zero of="$img" bs="$RC_SECTOR_BYTES" count=34 conv=notrunc status=none
    dd if=/dev/zero of="$img" bs="$RC_SECTOR_BYTES" count=33 \
       seek=$(( total_sectors - 33 )) conv=notrunc status=none
    sync

    local extra
    extra='{"partition":{"wiped":true,"scheme":"gpt","data_start_lba":'"$PART_START_LBA"',"note":"Primary and backup GPT destroyed. rc-partition must locate the NTFS boot sector at LBA 2048 and rebuild the table in memory."}}'
    write_expected "$name" "$fs" "$img" "$CORPUS_MANIFEST" true \
        "GPT-partitioned NTFS volume with both copies of the partition table destroyed. Exercises lost-partition rebuild." \
        "$extra"
    rc_log "wrote $(basename "${img%.img}.expected.json")"
}

# Shared writer for fixtures where every corpus file counts as unrecoverable
# through metadata (quick-format).
write_all_deleted_expected() {
    # The manifest defaults to the basic corpus; the formats fixture passes its
    # own, because it holds a different set of files entirely.
    local name="$1" fs="$2" img="$3" notes="$4" manifest="${5:-$CORPUS_MANIFEST}"
    RC_NAME="$name" RC_FS="$fs" RC_IMG="$img" \
    RC_SHA="$(rc_sha256 "$img")" RC_BYTES="$(stat -c %s "$img")" \
    RC_SECTOR="$RC_SECTOR_BYTES" RC_CLUSTER="$RC_CLUSTER_BYTES" \
    RC_GENVER="$GENERATOR_VERSION" RC_NOTES="$notes" \
    python3 - "$manifest" > "${img%.img}.expected.json" <<'PY'
import json, os, sys, datetime
corpus = json.load(open(sys.argv[1]))
files = {p: {"sha256": m["sha256"], "size": m["size"], "kind": m["kind"],
             "state": "deleted"} for p, m in corpus.items()}
doc = {
    "fixture": os.environ["RC_NAME"],
    "filesystem": os.environ["RC_FS"],
    "image": os.path.basename(os.environ["RC_IMG"]),
    "image_sha256": os.environ["RC_SHA"],
    "image_bytes": int(os.environ["RC_BYTES"]),
    "sector_bytes": int(os.environ["RC_SECTOR"]),
    "cluster_bytes": int(os.environ["RC_CLUSTER"]),
    "partitioned": False,
    "generator": {"script": "build_fixtures.sh",
                  "version": int(os.environ["RC_GENVER"]),
                  "built_utc": datetime.datetime.now(datetime.timezone.utc)
                               .replace(microsecond=0).isoformat(),
                  "provenance": json.loads(os.environ["RC_PROVENANCE"])},
    "notes": os.environ["RC_NOTES"],
    "files": files,
    "expect": {"deleted_count": len(files), "present_count": 0},
}
json.dump(doc, sys.stdout, indent=2, sort_keys=True)
sys.stdout.write("\n")
PY
    rc_log "wrote $(basename "${img%.img}.expected.json")"
}

# ---------------------------------------------------------------------------
# main
# ---------------------------------------------------------------------------

# Exported: the fragmented-fixture manifest writer reads it from the
# environment of a python3 child process.
export HOLE_KIB="${RC_HOLE_KIB:-32}"

# First partition starts at 1 MiB, the modern alignment default. The
# lost-partition rebuild in Milestone 3 must rediscover this offset.
PART_START_LBA="${RC_PART_START_LBA:-2048}"

usage() {
    cat >&2 <<EOF
usage: build_fixtures.sh [--list] [--out DIR] [FIXTURE...]

Fixtures: ${ALL_FIXTURES[*]}

Environment:
  RC_FIXTURE_DIR   output directory (default: testdata/fixtures)
  RC_FIXTURE_SIZE  image size       (default: 512M)
  RC_CLUSTER_BYTES cluster size     (default: 4096)
  RC_HOLE_KIB      filler hole size for the fragmented fixture (default: 32)
EOF
}

main() {
    local selected=()
    while (( $# )); do
        case "$1" in
            --list) printf '%s\n' "${ALL_FIXTURES[@]}"; return 0 ;;
            --out)  OUT_DIR="$2"; shift 2 ;;
            -h|--help) usage; return 0 ;;
            -*) usage; return 2 ;;
            *) selected+=("$1"); shift ;;
        esac
    done
    (( ${#selected[@]} )) || selected=("${ALL_FIXTURES[@]}")

    rc_require_tools losetup mount umount mkfs.ntfs mkfs.vfat mkfs.exfat \
                     mkfs.ext4 sfdisk python3 sha256sum dd truncate stat
    rc_require_sudo
    mkdir -p "$OUT_DIR"

    # Take the exclusive side of the lock the Rust tests hold shared.
    #
    # The immutability tests hash a fixture, scan it, and hash it again. A
    # rebuild landing between those two hashes makes the safety test report
    # that scanning modified the source, which is false and trains everyone to
    # dismiss the one test guarding the invariant that matters most. Blocking
    # here removes the race rather than documenting it.
    exec 9>"$OUT_DIR/.lock"
    if command -v flock >/dev/null 2>&1; then
        if ! flock -n 9; then
            rc_log "waiting for the fixtures lock (tests are reading them)..."
            flock 9
        fi
    else
        rc_log "WARNING: flock is unavailable; a concurrent test run could see a half-built fixture"
    fi

    # Single source of truth for both lists; see make_corpus.py. Read as bytes
    # because the deleted set contains non-ASCII names.
    mapfile -t DELETED_SET < <(python3 "$CORPUS_PY" --deleted-set)
    FRAG_TARGET="$(python3 "$CORPUS_PY" --fragment-target)"
    (( ${#DELETED_SET[@]} > 0 )) || rc_die "make_corpus.py returned an empty deleted set"
    [[ -n "$FRAG_TARGET" ]] || rc_die "make_corpus.py returned no fragment target"
    rc_log "deleted set: ${#DELETED_SET[@]} files; fragment target: $FRAG_TARGET"
    mkdir -p "$OUT_DIR"

    # Build the corpora once and reuse across fixtures.
    local work
    work="$(mktemp -d)"
    CORPUS_DIR="$work/corpus"
    FRAG_DIR="$work/frag"
    CORPUS_MANIFEST="$work/corpus.json"
    FRAG_MANIFEST="$work/frag.json"

    rc_step "corpus"
    python3 "$CORPUS_PY" "$CORPUS_DIR" > "$CORPUS_MANIFEST"
    rc_log "basic corpus: $(python3 -c 'import json,sys; print(len(json.load(open(sys.argv[1]))))' "$CORPUS_MANIFEST") files"
    python3 "$CORPUS_PY" --set=frag "$FRAG_DIR" > "$FRAG_MANIFEST"

    # Other real media, used to fill the gaps in fragmented-hard.
    DECOY_DIR="$work/decoy"
    python3 "$CORPUS_PY" --set=decoy "$DECOY_DIR" > /dev/null

    # The format-coverage corpus. Self-checked first: every sample must start
    # with its own signature and clear the resident-file threshold, and a
    # failure there means the fixture would silently prove nothing.
    python3 "$CORPUS_PY" --check-formats >&2 || rc_die "the format samples failed their self-check"
    FORMATS_DIR="$work/formats"
    FORMATS_MANIFEST="$work/formats.json"
    python3 "$CORPUS_PY" --set=formats "$FORMATS_DIR" > "$FORMATS_MANIFEST"
    rc_log "frag corpus:  $(python3 -c 'import json,sys; print(len(json.load(open(sys.argv[1]))))' "$FRAG_MANIFEST") files"

    local f
    for f in "${selected[@]}"; do
        case "$f" in
            ntfs-basic)      build_basic ntfs-basic  ntfs  RCNTFS ;;
            fat32-basic)     build_basic fat32-basic vfat  RCFAT32 ;;
            exfat-basic)     build_basic exfat-basic exfat RCEXFAT ;;
            ext4-basic)      build_basic ext4-basic  ext4  RCEXT4 ;;
            fragmented-jpeg) build_fragmented fragmented-jpeg constant fixed ;;
            fragmented-hard) build_fragmented fragmented-hard decoy varied ;;
            quickformat)     build_quickformat ;;
            formats)         build_formats ;;
            overwritten)     build_overwritten ;;
            nopart)          build_nopart ;;
            *) rc_die "unknown fixture: $f (see --list)" ;;
        esac
    done

    rm -rf "$work"
    rc_step "done"
    ls -la "$OUT_DIR" >&2
    # (.lock is the test-coordination lock, not a fixture)
}

main "$@"
