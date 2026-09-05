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
GENERATOR_VERSION=1

# Files deleted from every basic filesystem fixture. Chosen to span container
# formats (jpeg/png/pdf/docx/mp4/sqlite/text) and to include a nested path so
# directory-tree reconstruction is exercised in Milestone 2.
DELETED_SET=(
    "photos/img_0001.jpg"
    "photos/img_0003.jpg"
    "photos/nested/img_0004.jpg"
    "photos/screenshot.png"
    "docs/report.pdf"
    "docs/letter.docx"
    "video/clip_a.mp4"
    "data/messages.sqlite"
    "data/readme.txt"
)

ALL_FIXTURES=(
    ntfs-basic
    fat32-basic
    exfat-basic
    ext4-basic
    fragmented-jpeg
    quickformat
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

# populate_corpus MOUNTPOINT MANIFEST_JSON
# Copies every corpus file in, syncing between files so the allocator lays them
# down in a predictable order.
populate_corpus() {
    local mp="$1" src="$2"
    (cd "$src" && find . -type f | sort | while read -r f; do
        mkdir -p "$mp/$(dirname "${f#./}")"
        cp "$f" "$mp/${f#./}"
    done)
    sync
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
    deleted_json="$(printf '%s\n' "${DELETED_SET[@]}" | python3 -c \
        'import sys,json; print(json.dumps([l.strip() for l in sys.stdin if l.strip()]))')"

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

    populate_corpus "$mp" "$CORPUS_DIR"
    rc_log "copied $(find "$CORPUS_DIR" -type f | wc -l) files"
    delete_subset "$mp"
    rc_log "deleted ${#DELETED_SET[@]} files"

    rc_umount "$mp"
    rc_loop_detach "$dev"

    write_expected "$name" "$fs" "$img" "$CORPUS_MANIFEST" false \
        "Whole-device $fs filesystem, no partition table. Corpus copied in full, then a fixed subset deleted."
    rc_log "wrote $(basename "${img%.img}.expected.json")"
}

# Fragmentation: fill the volume with fixed-size filler files, punch alternating
# holes by deleting every other one, then write large media into the holes. The
# allocator has no contiguous run big enough, so each large file is split.
build_fragmented() {
    local name="fragmented-jpeg" fs="vfat"
    local img="$OUT_DIR/$name.img"
    rc_step "$name ($fs, hole size ${HOLE_KIB}KiB)"

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
    n=$(( free_kib / HOLE_KIB - 4 ))
    rc_log "writing $n filler files of ${HOLE_KIB}KiB, then deleting every other one"
    python3 - "$mp/filler" "$n" "$HOLE_KIB" <<'PY'
import os, sys
d, n, kib = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
os.makedirs(d, exist_ok=True)
blob = b"\xA5" * (kib * 1024)
written = 0
for i in range(n):
    try:
        with open(os.path.join(d, "f%05d" % i), "wb") as fh:
            fh.write(blob)
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
    extra="$(python3 -c 'import json,os; print(json.dumps({"fragmentation":{"hole_kib":int(os.environ["HOLE_KIB"]),"method":"alternating-filler-holes","filesystem":"vfat"}}))')"
    RC_DELETED_OVERRIDE=1 write_frag_expected "$name" "$fs" "$img" "$extra"
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
                               .replace(microsecond=0).isoformat()},
    "notes": ("Volume filled with %s KiB filler files, every other one deleted "
              "to punch holes, then large media written into the fragmented "
              "free space and deleted. Every listed file is expected to be "
              "fragmented; contiguous-only carving should recover none of them "
              "byte-exactly." % os.environ["RC_HOLE"]),
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

    RC_ALL_DELETED=1 write_all_deleted_expected "$name" "$fs" "$img" \
        "Corpus written to an NTFS volume, then quick-reformatted. All filesystem metadata for the original files is gone; content remains and should be recoverable by signature carving in Milestone 3."
}

# Populate, delete everything, then overwrite part of the free space so some
# deleted files are provably destroyed and others provably intact. This is the
# fixture the Milestone 5 Green/Yellow/Red classifier is graded against.
build_overwritten() {
    local name="overwritten" fs="ntfs"
    local img="$OUT_DIR/$name.img"
    rc_step "$name (populate, delete, partially overwrite)"

    rc_image_create "$img" >/dev/null
    local dev mp
    dev="$(rc_loop_attach "$img")"
    rc_mkfs "$fs" "$dev" RCOVER
    mp="$(rc_mount "$fs" "$dev")"
    populate_corpus "$mp" "$CORPUS_DIR"
    delete_subset "$mp"

    # Overwrite roughly the first third of the remaining free space with a
    # recognisable pattern, then remove the overwriting file itself.
    local free_kib third
    free_kib="$(df -k --output=avail "$mp" | tail -1)"
    third=$(( free_kib / 3 ))
    rc_log "overwriting ~${third}KiB of free space with 0xDB pattern"
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

    rc_umount "$mp"
    rc_loop_detach "$dev"

    local extra
    extra='{"overwrite":{"pattern_byte":"0xDB","fraction_of_free_space":0.33,"note":"Deleted files whose clusters fall in the overwritten region must classify RED; the remainder should classify GREEN."}}'
    write_expected "$name" "$fs" "$img" "$CORPUS_MANIFEST" false \
        "Corpus written, a fixed subset deleted, then roughly a third of free space overwritten with 0xDB. Grades the Milestone 5 confidence classifier." \
        "$extra"
    rc_log "wrote $(basename "${img%.img}.expected.json")"
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
    local name="$1" fs="$2" img="$3" notes="$4"
    RC_NAME="$name" RC_FS="$fs" RC_IMG="$img" \
    RC_SHA="$(rc_sha256 "$img")" RC_BYTES="$(stat -c %s "$img")" \
    RC_SECTOR="$RC_SECTOR_BYTES" RC_CLUSTER="$RC_CLUSTER_BYTES" \
    RC_GENVER="$GENERATOR_VERSION" RC_NOTES="$notes" \
    python3 - "$CORPUS_MANIFEST" > "${img%.img}.expected.json" <<'PY'
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
                               .replace(microsecond=0).isoformat()},
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
    rc_log "frag corpus:  $(python3 -c 'import json,sys; print(len(json.load(open(sys.argv[1]))))' "$FRAG_MANIFEST") files"

    local f
    for f in "${selected[@]}"; do
        case "$f" in
            ntfs-basic)      build_basic ntfs-basic  ntfs  RCNTFS ;;
            fat32-basic)     build_basic fat32-basic vfat  RCFAT32 ;;
            exfat-basic)     build_basic exfat-basic exfat RCEXFAT ;;
            ext4-basic)      build_basic ext4-basic  ext4  RCEXT4 ;;
            fragmented-jpeg) build_fragmented ;;
            quickformat)     build_quickformat ;;
            overwritten)     build_overwritten ;;
            nopart)          build_nopart ;;
            *) rc_die "unknown fixture: $f (see --list)" ;;
        esac
    done

    rm -rf "$work"
    rc_step "done"
    ls -la "$OUT_DIR" >&2
}

main "$@"
