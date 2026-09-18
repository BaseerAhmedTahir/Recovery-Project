# RECOVERY-CORE — building and what ships

Two scripts, run from the repository root on Windows:

```powershell
powershell -ExecutionPolicy Bypass -File packaging\build-windows.ps1
powershell -ExecutionPolicy Bypass -File packaging\build-android.ps1
```

The first produces `packaging\dist\RECOVERY-CORE\` (and an installer when
Tauri's bundler is available); the second produces a debug-signed APK you can
install on your own phone with `adb install -r`.

Both run the offline audits first and stop if one fails.

## What is in the folder

| File | What it is |
|---|---|
| `rc.exe` | The command line. Everything the engine can do is here, and the desktop app calls this binary for phones, SQLite and imaging. |
| `rc-gui.exe` | The desktop app. Looks for `rc.exe` beside it. |
| `docs\` | LIMITATIONS.md (read this one), PROGRESS.md, BRIDGE.md. |
| `platform-tools\` | `adb`, copied from your Android SDK if the build machine had one. |
| `ffmpeg.exe` | Copied from the build machine's PATH if it had one. |

## What is deliberately not bundled

- **ffmpeg** — needed only for video previews and video frame extraction.
  It is a large, separately licensed program. Put `ffmpeg.exe` beside `rc.exe`
  (or anywhere on PATH) and it is found; without it, video previews say so
  instead of failing quietly.
- **Android platform-tools (adb)** — Google's own download, with its own
  licence. Put the `platform-tools` folder beside `rc.exe`, or set `RC_ADB` to
  an `adb.exe`, or install the Android SDK.
- **A signing key for the APK.** There is none in this repository. The debug
  APK is signed with the SDK's standard debug key, which is fine for your own
  phone; a release APK is left unsigned for you to sign.

## Offline

The built programs make no network connection. This is enforced, not promised:

```powershell
cargo xtask audit          # all three of the audits below
cargo xtask audit-deps     # engine + CLI: no networking crate, and sockets
                           # only in the bridge module, behind its feature
cargo xtask audit-gui      # the desktop app's own dependency tree, its web
                           # view's content policy and its capabilities
cargo xtask audit-android  # the companion app's permissions, dependencies
                           # and socket use
```

The one socket in the whole project is the companion bridge: the desktop
listens on `127.0.0.1` and the phone reaches it through `adb reverse`, over the
USB cable (`docs\BRIDGE.md`).

**Building** is not offline: cargo, npm and Gradle download their dependencies,
and Tauri's installer bundler downloads NSIS once. Build on a machine with
internet, then copy the folder to the machine that does the recovery.

## Disk space while building

The build needs several gigabytes of temporary space and both toolchains
default to your system drive. Both scripts move their temporary files (and
Gradle's cache) onto the drive the repository is on when C: has little space
free, because on this machine C: is nearly full.

## Running it

- Reading a whole physical disk needs Administrator. Image files do not.
- Never write recovered files to the disk you are recovering from; the tool
  refuses that anyway.
- `rc --help` lists every command; the desktop app's "Phones, SQLite &
  imaging" tab is the same commands with forms.
