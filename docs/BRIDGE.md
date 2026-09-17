# Companion bridge protocol (RCB1)

The companion app on the phone sends files to `rc bridge` on the computer.
This is the only network socket in the project (SPEC.md 4.4), and it never
leaves the machine and its USB cable.

## Transport

- The computer listens on **127.0.0.1** only (`rc bridge --port 38300`), and
  closes any connection whose peer is not loopback.
- `rc bridge` runs `adb reverse tcp:38300 tcp:38300`, so when the app connects
  to `127.0.0.1:38300` *on the phone*, adb carries the connection over USB to
  the computer's loopback listener.
- Nothing listens on a network interface, on either side. Wi-Fi mode (TLS
  with a pinned per-session certificate, SPEC.md 6.4) is **not implemented**.

The source audit (`cargo xtask audit-deps`) fails the build if socket types
appear anywhere but `crates/rc-mobile/src/bridge.rs`, if that module is not
behind the `bridge` feature, or if a `bind` in it does not name loopback.

## Pairing

`rc bridge` prints a six-digit code. The person holding the phone types it into
the app. A wrong code gets `NO` and the connection is closed; the computer keeps
waiting for a correct one. The code is not a cryptographic secret - the socket
is loopback-only - it exists so a transfer cannot start without the phone's
holder doing it.

## Messages

All lines are UTF-8 and end with `\n`. Lines are at most 64 KiB.

```text
app      RCB1 <code>
rc       OK                         (or NO, then close)

app      {"path":"/storage/emulated/0/DCIM/.trashed-1726000000-IMG_1.jpg",
          "size":2048,"sha256":"<64 hex>","category":"trashed"}
app      <exactly size bytes>
rc       ACK <sha256>               (or NAK <reason>; the file is discarded)

         ... repeated per file ...

app      {"end":true}
rc       BYE <number of files kept>
```

- `category` is one of `trashed`, `thumbnail`, `app-media`, `database`, or
  anything else (filed under `other`).
- The computer writes each file to `<out>/<category>/<path on the phone>`,
  dropping empty, `.` and `..` path components so nothing escapes `<out>`, and
  replacing characters Windows cannot store. It never overwrites a file.
- The SHA-256 of the received bytes must match the header, or the file is
  deleted and `NAK sha256 <what arrived>` is sent.
- Files larger than 64 GiB are refused.
- After `BYE`, `<out>/manifest.json` lists every kept file with its phone path,
  local path, size and SHA-256.

## Implementations

- Computer: `crates/rc-mobile/src/bridge.rs`, tested by
  `crates/rc-mobile/tests/bridge.rs`.
- Phone: `companion-android/app/src/main/java/.../BridgeClient.kt`.
