#!/usr/bin/env python3
"""A scripted stand-in for `adb`, for tests. NOT a real phone.

It answers the handful of commands rc-mobile sends, in the output formats real
adb and Android's toybox print them, from a scenario directory:

  $FAKE_ADB_SCENARIO/scenario.json   {"devices": "<adb devices -l output>",
                                      "sdk": "34",
                                      "dumpsys_window": "...",
                                      "content_query": "..."}
  $FAKE_ADB_SCENARIO/phone/          the phone's filesystem; /storage/emulated/0
                                     maps to phone/storage/emulated/0

Commands: devices -l; [-s SERIAL] shell <cmd>; [-s SERIAL] pull -a SRC DST;
[-s SERIAL] reverse tcp:N tcp:N. Every invocation is appended to calls.log.
"""

import fnmatch
import json
import os
import re
import shlex
import shutil
import sys

scen = os.environ["FAKE_ADB_SCENARIO"]
cfg = json.load(open(os.path.join(scen, "scenario.json")))
phone = os.path.join(scen, "phone")
args = sys.argv[1:]
with open(os.path.join(scen, "calls.log"), "a", encoding="utf-8") as log:
    log.write(json.dumps(args) + "\n")

if args[:1] == ["-s"]:
    args = args[2:]


def local(remote):
    return os.path.join(phone, remote.lstrip("/"))


def find(cmd):
    # find '<root>' <expr> -type f -exec stat -c '%s|%Y|%n' {} + 2>/dev/null
    words = shlex.split(cmd)
    root = words[1]
    expr = words[2:words.index("-type")]
    patterns = [expr[i + 1] for i, w in enumerate(expr) if w == "-name"]
    base = local(root)
    if not os.path.isdir(base):
        return ""
    lines = []
    for d, _, names in os.walk(base):
        for n in sorted(names):
            if patterns and not any(fnmatch.fnmatchcase(n, p) for p in patterns):
                continue
            full = os.path.join(d, n)
            rel = os.path.relpath(full, phone).replace(os.sep, "/")
            st = os.stat(full)
            lines.append(f"{st.st_size}|{int(st.st_mtime)}|/{rel}")
    return "\n".join(lines) + ("\n" if lines else "")


cmd = args[0] if args else ""
if cmd == "devices":
    sys.stdout.write(cfg["devices"])
elif cmd == "shell":
    sh = args[1]
    if sh == "getprop ro.build.version.sdk":
        sys.stdout.write(cfg.get("sdk", "34") + "\n")
    elif sh == "dumpsys window":
        sys.stdout.write(cfg.get("dumpsys_window", ""))
    elif sh.startswith("content query"):
        sys.stdout.write(cfg.get("content_query", "No result found.\n"))
    elif sh.startswith("find "):
        sys.stdout.write(find(sh))
    else:
        sys.stderr.write(f"fake adb: unknown shell command {sh!r}\n")
        sys.exit(1)
elif cmd == "pull":
    src, dst = args[-2], args[-1]
    if not os.path.isfile(local(src)):
        sys.stderr.write(f"adb: error: failed to stat remote object '{src}'\n")
        sys.exit(1)
    shutil.copy2(local(src), dst)
    print(f"{src}: 1 file pulled.")
elif cmd == "reverse":
    print(args[-1].split(":")[1])
else:
    sys.stderr.write(f"fake adb: unsupported {args!r}\n")
    sys.exit(1)
