#!/usr/bin/env python3
"""Build SQLite databases with deleted SMS rows, and record what was deleted.

The databases are written by the SQLite library Python links against, not by
this project, so rc-sqlite-carve is checked against real page layouts, real
freeblocks, a real freelist and a real WAL.

Usage: make_sms_db.py OUT_DIR

Writes, in OUT_DIR:

  rollback.db              journal_mode=DELETE. Scattered single deletions
                           (freeblocks inside live pages) and one whole thread
                           deleted (pages onto the freelist), then a few new
                           messages written over part of the freed space.
  wal.db, wal.db-wal       journal_mode=WAL, copied while the connection is still
                           open so the WAL is not checkpointed away. The main
                           file holds the rows as of the last checkpoint; the WAL
                           holds the later deletions, and also rows that were
                           inserted and deleted again without ever reaching the
                           main file.
  secure.db                the rollback.db history with secure_delete=ON, which
                           zeroes deleted content: nothing may be recovered.
  truth.json               every message ever written, whether it is live or
                           deleted at the end, and for deleted ones whether its
                           body text is still anywhere in the files - the
                           measured upper bound on what carving can find.

The schema is Android's `sms` table from mmssms.db (telephony provider), with
its column order and defaults.

Deterministic for a given SQLite version: seeded, no timestamps from the clock.
"""

import json
import os
import random
import shutil
import sqlite3
import sys

SMS_SCHEMA = """CREATE TABLE sms (
    _id INTEGER PRIMARY KEY,
    thread_id INTEGER,
    address TEXT,
    person INTEGER,
    date INTEGER,
    date_sent INTEGER DEFAULT 0,
    protocol INTEGER,
    read INTEGER DEFAULT 0,
    status INTEGER DEFAULT -1,
    type INTEGER,
    reply_path_present INTEGER,
    subject TEXT,
    body TEXT,
    service_center TEXT,
    locked INTEGER DEFAULT 0,
    sub_id INTEGER DEFAULT -1,
    error_code INTEGER DEFAULT 0,
    creator TEXT,
    seen INTEGER DEFAULT 0
)"""

THREADS_SCHEMA = """CREATE TABLE threads (
    _id INTEGER PRIMARY KEY AUTOINCREMENT,
    date INTEGER DEFAULT 0,
    message_count INTEGER DEFAULT 0,
    recipient_ids TEXT,
    snippet TEXT,
    read INTEGER DEFAULT 1
)"""

COLUMNS = ["_id", "thread_id", "address", "person", "date", "date_sent", "protocol",
           "read", "status", "type", "reply_path_present", "subject", "body",
           "service_center", "locked", "sub_id", "error_code", "creator", "seen"]

WORDS = ("meet running late call me tomorrow ok thanks see you at the station "
         "dinner tonight bring the keys parcel arrived doctor at nine love you "
         "can't talk now where are you the code is happy birthday").split()


def message(rng, mid, thread, t0):
    n = rng.randint(3, 40)
    body = " ".join(rng.choice(WORDS) for _ in range(n))
    # Every body is unique, so its presence in the file can be searched for.
    body = f"[m{mid:05d}] {body}"
    if rng.random() < 0.05:
        body += " " + "x" * rng.randint(200, 900)
    return {
        "_id": mid,
        "thread_id": thread,
        "address": f"+4479{thread:02d}{rng.randint(100000, 999999)}",
        "person": None,
        "date": t0 + mid * 61_000,
        "date_sent": t0 + mid * 61_000 - rng.randint(0, 5000),
        "protocol": 0,
        "read": 1,
        "status": -1,
        "type": rng.choice([1, 2]),
        "reply_path_present": 0,
        "subject": None,
        "body": body,
        "service_center": None if rng.random() < 0.5 else "+447785016005",
        "locked": 0,
        "sub_id": 1,
        "error_code": 0,
        "creator": "com.google.android.apps.messaging",
        "seen": 1,
    }


def insert(con, m):
    con.execute(
        f"INSERT INTO sms ({','.join(COLUMNS)}) VALUES ({','.join('?' * len(COLUMNS))})",
        [m[c] for c in COLUMNS],
    )


def setup(con, mode, secure=False):
    con.execute("PRAGMA page_size=4096")
    con.execute("PRAGMA auto_vacuum=NONE")
    con.execute(f"PRAGMA journal_mode={mode}")
    # Some SQLite builds (Android's among them) default secure_delete on, which
    # zeroes deleted content. Off is what a phone that does not zero looks like;
    # the test for the on case is that nothing is recovered and nothing invented.
    con.execute(f"PRAGMA secure_delete={'ON' if secure else 'OFF'}")
    con.execute(SMS_SCHEMA)
    con.execute(THREADS_SCHEMA)


def present(files, text):
    needle = text.encode("utf-8")
    return any(needle in open(f, "rb").read() for f in files if os.path.exists(f))


def build_rollback(out, rng, t0, name="rollback.db", secure=False):
    path = os.path.join(out, name)
    for f in (path, path + "-journal"):
        if os.path.exists(f):
            os.remove(f)
    con = sqlite3.connect(path, isolation_level=None)
    setup(con, "DELETE", secure)
    msgs = {}
    mid = 1
    con.execute("BEGIN")
    for thread in range(1, 9):
        con.execute("INSERT INTO threads (_id, recipient_ids, snippet) VALUES (?, ?, ?)",
                    (thread, str(thread), "..."))
        for _ in range(rng.randint(25, 60)):
            m = message(rng, mid, thread, t0)
            insert(con, m)
            msgs[mid] = m
            mid += 1
    con.execute("COMMIT")

    deleted = set()
    # Scattered single deletions: freeblocks inside pages that stay live.
    con.execute("BEGIN")
    for victim in rng.sample(sorted(msgs), 30):
        if msgs[victim]["thread_id"] == 5:
            continue
        con.execute("DELETE FROM sms WHERE _id=?", (victim,))
        deleted.add(victim)
    con.execute("COMMIT")
    # A whole conversation deleted: its pages go to the freelist.
    con.execute("BEGIN")
    for k, m in msgs.items():
        if m["thread_id"] == 5:
            deleted.add(k)
    con.execute("DELETE FROM sms WHERE thread_id=5")
    con.execute("DELETE FROM threads WHERE _id=5")
    con.execute("COMMIT")
    # New messages afterwards reuse some of the freed space.
    con.execute("BEGIN")
    for _ in range(12):
        m = message(rng, mid, 9, t0)
        insert(con, m)
        msgs[mid] = m
        mid += 1
    con.execute("COMMIT")
    con.close()
    return path, [path], msgs, deleted


def build_wal(out, rng, t0):
    work = os.path.join(out, "wal-work")
    shutil.rmtree(work, ignore_errors=True)
    os.makedirs(work)
    live_path = os.path.join(work, "wal.db")
    con = sqlite3.connect(live_path, isolation_level=None)
    setup(con, "WAL")
    con.execute("PRAGMA wal_autocheckpoint=0")
    msgs = {}
    mid = 1
    con.execute("BEGIN")
    for thread in range(1, 6):
        for _ in range(rng.randint(30, 50)):
            m = message(rng, mid, thread, t0)
            insert(con, m)
            msgs[mid] = m
            mid += 1
    con.execute("COMMIT")
    # Everything so far reaches the main file.
    con.execute("PRAGMA wal_checkpoint(TRUNCATE)")

    deleted = set()
    # Deleted after the checkpoint: gone in the WAL's pages, present in the
    # main file's older pages.
    con.execute("BEGIN")
    for victim in rng.sample(sorted(msgs), 25):
        con.execute("DELETE FROM sms WHERE _id=?", (victim,))
        deleted.add(victim)
    con.execute("COMMIT")
    # Written and deleted again inside the WAL, never checkpointed: these exist
    # only in older WAL frames.
    transient = []
    for _ in range(3):
        con.execute("BEGIN")
        for _ in range(6):
            m = message(rng, mid, 7, t0)
            insert(con, m)
            msgs[mid] = m
            transient.append(mid)
            mid += 1
        con.execute("COMMIT")
    con.execute("BEGIN")
    for victim in transient[::2]:
        con.execute("DELETE FROM sms WHERE _id=?", (victim,))
        deleted.add(victim)
    con.execute("COMMIT")

    # Copy while the connection is open: closing would checkpoint and delete
    # the WAL.
    path = os.path.join(out, "wal.db")
    for suffix in ("", "-wal"):
        shutil.copyfile(live_path + suffix, path + suffix)
    con.close()
    shutil.rmtree(work, ignore_errors=True)
    return path, [path, path + "-wal"], msgs, deleted


def main():
    out = sys.argv[1]
    os.makedirs(out, exist_ok=True)
    t0 = 1_725_000_000_000
    truth = {"sqlite_version": sqlite3.sqlite_version, "columns": COLUMNS, "databases": {}}
    secure = lambda out, rng, t0: build_rollback(out, rng, t0, "secure.db", True)
    for name, build, seed in (("rollback", build_rollback, 11), ("wal", build_wal, 12),
                              ("secure", secure, 11)):
        rng = random.Random(seed)
        path, files, msgs, deleted = build(out, rng, t0)
        rows = []
        for k in sorted(msgs):
            m = msgs[k]
            state = "deleted" if k in deleted else "live"
            row = {"state": state, "values": [m[c] for c in COLUMNS]}
            if state == "deleted":
                row["body_in_files"] = present(files, m["body"])
            rows.append(row)
        truth["databases"][name] = {
            "file": os.path.basename(path),
            "files": [os.path.basename(f) for f in files],
            "rows": rows,
        }
    with open(os.path.join(out, "truth.json"), "w", encoding="utf-8") as f:
        json.dump(truth, f, indent=1)


if __name__ == "__main__":
    main()
