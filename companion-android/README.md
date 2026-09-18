# RECOVERY Companion (Android)

The phone-side app. It does the three things an app on an unrooted phone can
honestly do, and nothing else.

1. **The system trash** (Android 11 and newer). Photos and videos you deleted
   in the last ~30 days are still on the phone, marked as trashed. The app
   lists them with the date each one will be erased for good, and puts the ones
   you tick back where they came from. Android shows its own confirmation
   dialog for that, so nothing is restored without you tapping Allow.
2. **Caches and leftovers.** Thumbnails, WhatsApp/Telegram media folders, and
   `.trashed-*` files still on the card. These are often the only surviving
   copy of a photo whose original is gone — usually smaller than the original.
3. **Send to the computer over USB.** The desktop tool does the heavy work
   (carving, validation, ratings). The app streams the files you pick to it
   over the USB cable, checked with a SHA-256 per file.

## What it cannot do, and why

A deleted photo that is past the trash is not recoverable from the phone's
internal storage by any app, rooted or not. Android encrypts each file with its
own key and throws that key away when the file is deleted, and the flash
controller erases the blocks. Anything that claims otherwise about internal
storage is guessing or selling something. Removable SD cards are different:
take the card out and scan it with the desktop tool, which can carve it
properly.

## Permissions

- **Photos and videos** (`READ_MEDIA_IMAGES`, `READ_MEDIA_VIDEO`, or
  `READ_EXTERNAL_STORAGE` on Android 12 and below) — to list what can be
  recovered.
- **`INTERNET`** — Android requires this even for a socket to `127.0.0.1`,
  which is all this app opens. The connection is carried to your computer by
  `adb reverse` over the USB cable. There is no analytics, no advertising, no
  update check, and `Bridge.kt` refuses any address that is not loopback.

Not requested: all-files access, network state, background work, boot startup,
or the list of your installed apps. The app has no service and no receiver: it
does nothing while it is not open. `cargo xtask audit-android` fails the build
if any of that changes.

## Building

```powershell
powershell -ExecutionPolicy Bypass -File ..\packaging\build-android.ps1
```

Or by hand, from this directory:

```powershell
gradle :app:testDebugUnitTest      # unit tests, including the bridge protocol
gradle :app:assembleDebug          # app\build\outputs\apk\debug\app-debug.apk
```

Needs the Android SDK (`ANDROID_HOME`) and a JDK 17. On a machine whose system
drive is nearly full, set `GRADLE_USER_HOME` and `TMP` to a drive with room —
the packaging script does this for you.

## The bridge protocol is checked against the desktop

`app/src/test/.../BridgeProtocolTest.kt` writes `protocol-golden.bin`: the
exact bytes this app sends for a two-file session. The desktop test
`crates/rc-mobile/tests/bridge.rs` replays that file into the real receiver and
checks both files arrive with the right names, categories and hashes. Neither
side is tested against the other's assumptions, and neither needs a phone.

## Not verified on a real phone

No phone or emulator image was available on the machine this was built on. The
MediaStore queries, the trash-restore request and the USB bridge are written to
the documented APIs and the protocol is checked end to end against the desktop,
but none of it has run on real hardware. Try it on your own phone and expect
rough edges; `docs/LIMITATIONS.md` says the same.
