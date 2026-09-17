#!/usr/bin/env python3
"""Build a synthetic, unencrypted iOS backup with photos in "Recently Deleted".

Usage: make_ios_backup.py OUT_DIR

This is SYNTHETIC. No iPhone or iTunes was involved: the directory layout,
Manifest.plist / Info.plist keys, the Manifest.db `Files` schema and the
Photos.sqlite tables and columns used (ZASSET, ZADDITIONALASSETATTRIBUTES,
ZTRASHEDSTATE, ZTRASHEDDATE, ZDIRECTORY, ZFILENAME, ...) follow how real
backups are documented to look. The databases are written by SQLite and the
plists by plistlib, so the byte formats are real; whether the schema matches
every iOS version is not something this can prove.

Writes OUT_DIR/backup/<udid>/ (a backup), OUT_DIR/encrypted/<udid>/ (the same
with IsEncrypted true, which must be refused), and OUT_DIR/truth.json.
"""

import hashlib
import json
import os
import plistlib
import random
import shutil
import sqlite3
import sys

UDID = "00008110-001A2B3C4D5E801E"
CORE_DATA_EPOCH = 978307200


def file_id(domain, rel):
    return hashlib.sha1(f"{domain}-{rel}".encode()).hexdigest()


def main():
    out = sys.argv[1]
    shutil.rmtree(out, ignore_errors=True)
    rng = random.Random(7)
    root = os.path.join(out, "backup", UDID)
    os.makedirs(root)

    files = []  # (domain, relativePath, flags, bytes or None)

    def add(domain, rel, data, flags=1):
        files.append((domain, rel, flags, data))
        if data is not None:
            fid = file_id(domain, rel)
            os.makedirs(os.path.join(root, fid[:2]), exist_ok=True)
            with open(os.path.join(root, fid[:2], fid), "wb") as f:
                f.write(data)

    # --- assets ---------------------------------------------------------------
    assets = []
    for i in range(1, 13):
        video = i % 5 == 0
        ext = "MOV" if video else "HEIC"
        name = f"IMG_{1000 + i:04d}.{ext}"
        directory = "DCIM/100APPLE" if i < 8 else "DCIM/101APPLE"
        trashed = i in (2, 5, 7, 11)
        body = bytes(rng.getrandbits(8) for _ in range(2000 + 97 * i))
        created = 700_000_000 + i * 3600.5
        a = {
            "pk": i,
            "kind": 1 if video else 0,
            "uti": "com.apple.quicktime-movie" if video else "public.heic",
            "directory": directory,
            "filename": name,
            "original_filename": f"holiday-{i}.{ext.lower()}",
            "trashed": trashed,
            "trashed_date": 715_000_000.25 + i if trashed else None,
            "created": created,
            "sha256": hashlib.sha256(body).hexdigest(),
            "derivatives": {},
        }
        add("CameraRollDomain", f"Media/{directory}/{name}", body)
        # Thumbnails for some, an edited render for one.
        if i % 2 == 1:
            thumb = bytes(rng.getrandbits(8) for _ in range(300))
            rel = f"Media/PhotoData/Thumbnails/V2/{directory}/{name}/5005.JPG"
            add("CameraRollDomain", rel, thumb)
            a["derivatives"][rel] = hashlib.sha256(thumb).hexdigest()
        if i == 7:
            render = bytes(rng.getrandbits(8) for _ in range(900))
            stem = name.rsplit(".", 1)[0]
            rel = f"Media/PhotoData/Mutations/{directory}/{stem}/Adjustments/FullSizeRender.jpg"
            add("CameraRollDomain", rel, render)
            a["derivatives"][rel] = hashlib.sha256(render).hexdigest()
        assets.append(a)

    # A trashed asset whose original is not in the backup.
    assets.append({
        "pk": 13, "kind": 0, "uti": "public.jpeg", "directory": "DCIM/101APPLE",
        "filename": "IMG_1013.JPG", "original_filename": "missing.jpg", "trashed": True,
        "trashed_date": 715_100_000.0, "created": 700_100_000.0, "sha256": None,
        "derivatives": {},
    })

    # --- Photos.sqlite ----------------------------------------------------------
    work = os.path.join(out, "work")
    os.makedirs(work)
    photos = os.path.join(work, "Photos.sqlite")
    con = sqlite3.connect(photos)
    con.executescript("""
        CREATE TABLE ZASSET ( Z_PK INTEGER PRIMARY KEY, Z_ENT INTEGER, Z_OPT INTEGER,
            ZKIND INTEGER, ZTRASHEDSTATE INTEGER, ZADDITIONALATTRIBUTES INTEGER,
            ZDATECREATED TIMESTAMP, ZTRASHEDDATE TIMESTAMP, ZDIRECTORY VARCHAR,
            ZFILENAME VARCHAR, ZUNIFORMTYPEIDENTIFIER VARCHAR );
        CREATE TABLE ZADDITIONALASSETATTRIBUTES ( Z_PK INTEGER PRIMARY KEY, Z_ENT INTEGER,
            Z_OPT INTEGER, ZASSET INTEGER, ZORIGINALFILENAME VARCHAR,
            ZORIGINALFILESIZE INTEGER );
        CREATE INDEX ZASSET_ZTRASHEDSTATE ON ZASSET (ZTRASHEDSTATE);
    """)
    for a in assets:
        con.execute(
            "INSERT INTO ZASSET VALUES (?,?,?,?,?,?,?,?,?,?,?)",
            (a["pk"], 3, 1, a["kind"], 1 if a["trashed"] else 0, 100 + a["pk"], a["created"],
             a["trashed_date"], a["directory"], a["filename"], a["uti"]),
        )
        con.execute(
            "INSERT INTO ZADDITIONALASSETATTRIBUTES VALUES (?,?,?,?,?,?)",
            (100 + a["pk"], 5, 1, a["pk"], a["original_filename"], 1234),
        )
    con.commit()
    con.close()
    add("CameraRollDomain", "Media/PhotoData/Photos.sqlite", open(photos, "rb").read())
    add("CameraRollDomain", "Media/DCIM", None, flags=2)
    add("HomeDomain", "Library/SMS/sms.db", b"not used here")

    # --- Manifest.db --------------------------------------------------------------
    manifest = os.path.join(root, "Manifest.db")
    con = sqlite3.connect(manifest)
    con.executescript("""
        CREATE TABLE Files (fileID TEXT PRIMARY KEY, domain TEXT, relativePath TEXT,
            flags INTEGER, file BLOB);
        CREATE INDEX FilesDomainIdx ON Files(domain);
        CREATE INDEX FilesRelativePathIdx ON Files(relativePath);
        CREATE TABLE Properties (key TEXT PRIMARY KEY, value BLOB);
    """)
    for domain, rel, flags, data in files:
        blob = plistlib.dumps({"$archiver": "NSKeyedArchiver", "Size": len(data or b"")},
                              fmt=plistlib.FMT_BINARY)
        con.execute("INSERT INTO Files VALUES (?,?,?,?,?)",
                    (file_id(domain, rel), domain, rel, flags, blob))
    con.commit()
    con.close()

    lockdown = {"DeviceName": "Test iPhone", "ProductType": "iPhone14,5",
                "ProductVersion": "17.5.1", "UniqueDeviceID": UDID}
    with open(os.path.join(root, "Manifest.plist"), "wb") as f:
        plistlib.dump({"IsEncrypted": False, "Version": "10.0", "Lockdown": lockdown,
                       "SystemDomainsVersion": "24.0"}, f, fmt=plistlib.FMT_BINARY)
    with open(os.path.join(root, "Info.plist"), "wb") as f:
        plistlib.dump({"Device Name": "Test iPhone", "Product Type": "iPhone14,5",
                       "Product Version": "17.5.1", "Unique Identifier": UDID}, f)
    with open(os.path.join(root, "Status.plist"), "wb") as f:
        plistlib.dump({"IsFullBackup": False, "SnapshotState": "finished"}, f,
                      fmt=plistlib.FMT_BINARY)

    enc = os.path.join(out, "encrypted", UDID)
    shutil.copytree(root, enc)
    with open(os.path.join(enc, "Manifest.plist"), "wb") as f:
        plistlib.dump({"IsEncrypted": True, "Version": "10.0", "Lockdown": lockdown}, f,
                      fmt=plistlib.FMT_BINARY)
    shutil.rmtree(work)

    trashed = []
    for a in assets:
        if not a["trashed"]:
            continue
        trashed.append({
            "pk": a["pk"], "filename": a["filename"], "directory": a["directory"],
            "original_filename": a["original_filename"], "kind": a["kind"],
            "trashed_unix": int(a["trashed_date"] + CORE_DATA_EPOCH),
            "created_unix": int(a["created"] + CORE_DATA_EPOCH),
            "original_sha256": a["sha256"], "derivatives": a["derivatives"],
        })
    with open(os.path.join(out, "truth.json"), "w") as f:
        json.dump({"udid": UDID, "device_name": "Test iPhone", "trashed": trashed,
                   "camera_roll_files": sum(1 for d, _, fl, _ in files
                                            if d == "CameraRollDomain" and fl != 2)},
                  f, indent=1)


if __name__ == "__main__":
    main()
