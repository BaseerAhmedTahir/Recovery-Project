#!/usr/bin/env python3
"""Recover the true fragment layout of every file in a fragmented fixture.

The fragmented fixture's ground truth records only the *method* - filler files
in alternating holes on FAT32 - and says each file is "expected to be
fragmented". That is an expectation. Reassembly cannot be graded against it:
without knowing where a file's pieces actually landed there is no way to tell
a near miss from a wild one, or to know whether a two-fragment technique is
even the right tool for a file that turned out to be in fifteen.

This locates every cluster of every file in the image and reports the runs.
It needs the files' exact bytes, which the corpus generator reproduces; each is
checked against the fixture's recorded hash first, and a mismatch is fatal
rather than a guess.

Usage:
    fragmap.py IMAGE EXPECTED_JSON CORPUS_DIR   print the layout as JSON
"""

import hashlib
import json
import mmap
import os
import sys


def locate(img, files, cluster):
    """Map each file's clusters to image offsets and group them into runs.

    FAT clusters are aligned to the start of the data region, not to the start
    of the image, so the alignment phase is found from one file's first
    cluster before any cluster is indexed.
    """
    with open(img, "rb") as fh:
        mm = mmap.mmap(fh.fileno(), 0, access=mmap.ACCESS_READ)

        # Find the phase from the first cluster of the first file.
        probe_path, probe = next(iter(files.items()))
        first = mm.find(probe[:cluster])
        if first < 0:
            raise SystemExit("%s: first cluster not found in the image" % probe_path)
        phase = first % cluster

        # Index every cluster of the image at that phase, by content hash.
        index = {}
        off = phase
        while off + cluster <= len(mm):
            h = hashlib.blake2b(mm[off : off + cluster], digest_size=16).digest()
            index.setdefault(h, []).append(off)
            off += cluster

        layouts = {}
        for path, data in files.items():
            runs = []
            ambiguous = 0
            unfound = 0
            prev_end = None
            for i in range(0, len(data), cluster):
                piece = data[i : i + cluster]
                if len(piece) == cluster:
                    cands = index.get(hashlib.blake2b(piece, digest_size=16).digest(), [])
                else:
                    # The final partial cluster: search for its bytes directly,
                    # at the phase, near where the previous run ended.
                    cands = []
                    start = (prev_end if prev_end is not None else phase)
                    hit = mm.find(piece, start)
                    while hit >= 0 and (hit - phase) % cluster != 0:
                        hit = mm.find(piece, hit + 1)
                    if hit >= 0:
                        cands = [hit]
                if not cands:
                    unfound += 1
                    prev_end = None
                    continue
                if len(cands) > 1:
                    ambiguous += 1
                # Prefer the candidate that continues the current run; otherwise
                # the first one after the previous run, since FAT allocates
                # forward; otherwise the first one at all.
                if prev_end is not None and prev_end in cands:
                    at = prev_end
                elif prev_end is not None and any(c > prev_end for c in cands):
                    at = min(c for c in cands if c > prev_end)
                else:
                    at = cands[0]

                if runs and runs[-1][0] + runs[-1][1] == at:
                    runs[-1][1] += len(piece)
                else:
                    runs.append([at, len(piece)])
                prev_end = at + len(piece)

            layouts[path] = {
                "size": len(data),
                "fragments": [{"offset": o, "length": n} for o, n in runs],
                "fragment_count": len(runs),
                "clusters_ambiguous": ambiguous,
                "clusters_unfound": unfound,
                "in_order": all(runs[k][0] < runs[k + 1][0] for k in range(len(runs) - 1)),
                "gaps": [
                    runs[k + 1][0] - (runs[k][0] + runs[k][1]) for k in range(len(runs) - 1)
                ],
            }
        mm.close()
    return phase, layouts


def main(argv):
    if len(argv) != 4:
        sys.stderr.write(__doc__)
        return 2
    img, expected, corpus = argv[1:]
    truth = json.load(open(expected, encoding="utf-8"))
    cluster = int(truth.get("cluster_bytes", 4096))

    files = {}
    for path, meta in truth["files"].items():
        src = os.path.join(corpus, path.replace("/", os.sep))
        with open(src, "rb") as fh:
            data = fh.read()
        if hashlib.sha256(data).hexdigest() != meta["sha256"]:
            raise SystemExit(
                "%s: the regenerated file does not match the fixture's hash; the "
                "layout would describe different bytes" % path
            )
        files[path] = data

    phase, layouts = locate(img, files, cluster)
    json.dump({"cluster_bytes": cluster, "phase": phase, "files": layouts},
              sys.stdout, indent=2, sort_keys=True)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
