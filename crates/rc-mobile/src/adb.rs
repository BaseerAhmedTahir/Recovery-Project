//! Driving a bundled or installed `adb` binary (SPEC.md 6.1: "Start with the
//! bundled binary; go native only if it proves necessary").
//!
//! Nothing here can work on a phone its holder has not set up: adb itself
//! refuses a device until USB debugging is on and this computer's RSA key has
//! been accepted on the unlocked phone. On top of that, extraction refuses a
//! phone whose lock screen is showing (see [`Checklist`]).
//!
//! Every adb invocation is a separate process with explicit arguments - no
//! shell string built from device-supplied names on the host side. Commands
//! run *on the phone* through `adb shell` do go through the phone's shell, so
//! device paths are single-quoted with [`shell_quote`].

use crate::{Error, Result};
use serde::Serialize;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

/// How to run adb. `prefix` lets tests run a scripted stand-in (`python
/// fake_adb.py`); normally it is empty.
#[derive(Clone, Debug)]
pub struct Adb {
    pub program: PathBuf,
    pub prefix: Vec<OsString>,
    pub serial: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceState {
    /// Authorized and usable.
    Device,
    /// Connected, but this computer's key has not been accepted on the phone.
    Unauthorized,
    Offline,
    /// The OS denied access to the USB device (udev rules on Linux).
    NoPermissions,
    Recovery,
    Sideload,
    Other(String),
}

#[derive(Clone, Debug, Serialize)]
pub struct DeviceEntry {
    pub serial: String,
    pub state: DeviceState,
    pub model: Option<String>,
    pub product: Option<String>,
}

impl Adb {
    /// Find adb: `RC_ADB`, a `platform-tools` directory next to this
    /// executable (the bundled copy), the Android SDK, then PATH.
    pub fn find() -> Option<Adb> {
        let exe = if cfg!(windows) { "adb.exe" } else { "adb" };
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(p) = std::env::var_os("RC_ADB") {
            candidates.push(PathBuf::from(p));
        }
        if let Ok(me) = std::env::current_exe() {
            if let Some(dir) = me.parent() {
                candidates.push(dir.join("platform-tools").join(exe));
            }
        }
        for var in ["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
            if let Some(sdk) = std::env::var_os(var) {
                candidates.push(PathBuf::from(sdk).join("platform-tools").join(exe));
            }
        }
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            candidates.push(
                PathBuf::from(local)
                    .join("Android")
                    .join("Sdk")
                    .join("platform-tools")
                    .join(exe),
            );
        }
        if let Some(path) = std::env::var_os("PATH") {
            candidates.extend(std::env::split_paths(&path).map(|d| d.join(exe)));
        }
        candidates
            .into_iter()
            .find(|p| p.is_file())
            .map(|program| Adb {
                program,
                prefix: Vec::new(),
                serial: None,
            })
    }

    pub fn with_serial(mut self, serial: &str) -> Adb {
        self.serial = Some(serial.to_string());
        self
    }

    fn command(&self) -> Command {
        let mut c = Command::new(&self.program);
        c.args(&self.prefix);
        if let Some(s) = &self.serial {
            c.arg("-s").arg(s);
        }
        c
    }

    /// Run adb with `args`; stdout on success.
    pub fn run(&self, args: &[&str]) -> Result<Vec<u8>> {
        let out =
            self.command().args(args).output().map_err(|e| {
                Error::Adb(format!("could not run {}: {e}", self.program.display()))
            })?;
        if !out.status.success() {
            return Err(Error::Adb(format!(
                "adb {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(out.stdout)
    }

    /// Run a command in the phone's shell.
    pub fn shell(&self, cmd: &str) -> Result<String> {
        Ok(String::from_utf8_lossy(&self.run(&["shell", cmd])?).replace("\r\n", "\n"))
    }

    pub fn devices(&self) -> Result<Vec<DeviceEntry>> {
        let mut plain = self.clone();
        plain.serial = None;
        let out = plain.run(&["devices", "-l"])?;
        Ok(parse_devices(&String::from_utf8_lossy(&out)))
    }

    /// Copy a file off the phone into `dest`, which must not exist yet.
    pub fn pull(&self, remote: &str, dest: &Path) -> Result<()> {
        if dest.exists() {
            return Err(Error::Adb(format!("{} already exists", dest.display())));
        }
        let dest_s = dest.to_string_lossy().to_string();
        self.run(&["pull", "-a", remote, &dest_s])?;
        if !dest.is_file() {
            return Err(Error::Adb(format!("adb pull did not produce {dest_s}")));
        }
        Ok(())
    }
}

pub fn parse_devices(text: &str) -> Vec<DeviceEntry> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("List of devices") || line.starts_with('*') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let (Some(serial), Some(state)) = (parts.next(), parts.next()) else {
            continue;
        };
        let mut state = match state {
            "device" => DeviceState::Device,
            "unauthorized" => DeviceState::Unauthorized,
            "offline" => DeviceState::Offline,
            "recovery" => DeviceState::Recovery,
            "sideload" => DeviceState::Sideload,
            "no" => DeviceState::NoPermissions,
            other => DeviceState::Other(other.to_string()),
        };
        let mut model = None;
        let mut product = None;
        for p in parts {
            if let Some(v) = p.strip_prefix("model:") {
                model = Some(v.to_string());
            } else if let Some(v) = p.strip_prefix("product:") {
                product = Some(v.to_string());
            } else if p == "permissions;" || p.starts_with("permissions") {
                state = DeviceState::NoPermissions;
            }
        }
        out.push(DeviceEntry {
            serial: serial.to_string(),
            state,
            model,
            product,
        });
    }
    out
}

/// Quote a string for the phone's `sh`.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Is the lock screen showing, from `dumpsys window`? `None` when the output
/// has none of the fields this knows, which varies by Android version.
pub fn keyguard_showing(dumpsys_window: &str) -> Option<bool> {
    let mut seen = None;
    for key in [
        "mDreamingLockscreen=",
        "mKeyguardShowing=",
        "isKeyguardShowing=",
        "mShowingLockscreen=",
        "KeyguardShowing=",
    ] {
        for (i, _) in dumpsys_window.match_indices(key) {
            let rest = &dumpsys_window[i + key.len()..];
            if rest.starts_with("true") {
                return Some(true);
            }
            if rest.starts_with("false") {
                seen = Some(false);
            }
        }
    }
    seen
}

/// What must be true before anything is read from a phone. Shown to the user
/// item by item, so a missing step is named rather than failing obscurely.
#[derive(Clone, Debug, Serialize)]
pub struct Checklist {
    pub adb_found: bool,
    pub device_connected: bool,
    /// USB debugging on and this computer's key accepted on the phone.
    pub authorized: bool,
    /// `Some(true)` unlocked, `Some(false)` lock screen showing, `None` could
    /// not be determined on this Android version.
    pub unlocked: Option<bool>,
    pub serial: Option<String>,
    pub model: Option<String>,
    pub api_level: Option<u32>,
    pub problems: Vec<String>,
}

impl Checklist {
    /// Ready to extract. An undetermined lock state needs the user's explicit
    /// confirmation that the phone in front of them is unlocked.
    pub fn ready(&self, user_confirms_unlocked: bool) -> bool {
        self.adb_found
            && self.device_connected
            && self.authorized
            && match self.unlocked {
                Some(u) => u,
                None => user_confirms_unlocked,
            }
    }
}

pub fn checklist(adb: Option<&Adb>, serial: Option<&str>) -> Checklist {
    let mut c = Checklist {
        adb_found: adb.is_some(),
        device_connected: false,
        authorized: false,
        unlocked: None,
        serial: None,
        model: None,
        api_level: None,
        problems: Vec::new(),
    };
    let Some(adb) = adb else {
        c.problems.push(
            "adb was not found. Put Android platform-tools next to rc, set RC_ADB, or install \
             the Android SDK platform-tools."
                .into(),
        );
        return c;
    };
    let devices = match adb.devices() {
        Ok(d) => d,
        Err(e) => {
            c.problems.push(format!("adb could not list devices: {e}"));
            return c;
        }
    };
    let chosen: Vec<&DeviceEntry> = devices
        .iter()
        .filter(|d| serial.map_or(true, |s| d.serial == s))
        .collect();
    let dev = match chosen.as_slice() {
        [] => {
            c.problems.push(match serial {
                Some(s) => format!("no device with serial {s} is connected"),
                None => "no phone is connected over USB".into(),
            });
            return c;
        }
        [one] => *one,
        many => {
            c.problems.push(format!(
                "{} devices are connected; choose one with --serial ({})",
                many.len(),
                many.iter()
                    .map(|d| d.serial.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            return c;
        }
    };
    c.device_connected = true;
    c.serial = Some(dev.serial.clone());
    c.model = dev.model.clone();
    match &dev.state {
        DeviceState::Device => c.authorized = true,
        DeviceState::Unauthorized => {
            c.problems.push(
                "the phone has not authorized this computer: unlock it and accept the \
                 'Allow USB debugging?' prompt"
                    .into(),
            );
            return c;
        }
        DeviceState::NoPermissions => {
            c.problems
                .push("the operating system denied access to the USB device".into());
            return c;
        }
        other => {
            c.problems.push(format!(
                "the phone is in state {other:?}, not ready for adb"
            ));
            return c;
        }
    }
    let adb = adb.clone().with_serial(&dev.serial);
    c.api_level = adb
        .shell("getprop ro.build.version.sdk")
        .ok()
        .and_then(|s| s.trim().parse().ok());
    match adb.shell("dumpsys window") {
        Ok(out) => {
            c.unlocked = keyguard_showing(&out).map(|showing| !showing);
            match c.unlocked {
                Some(false) => c
                    .problems
                    .push("the lock screen is showing: unlock the phone".into()),
                None => c.problems.push(
                    "could not tell from this Android version whether the phone is unlocked; \
                     unlock it and confirm"
                        .into(),
                ),
                Some(true) => {}
            }
        }
        Err(e) => c
            .problems
            .push(format!("could not read the lock state: {e}")),
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_list_states() {
        let text = "List of devices attached\n\
                    R58M123ABC             device usb:1-1 product:beyond1lteeea model:SM_G973F device:beyond1 transport_id:1\n\
                    0123456789ABCDEF       unauthorized usb:1-2 transport_id:2\n\
                    emulator-5554          offline transport_id:3\n\
                    ZY22               no permissions; see [http://developer.android.com/tools/device.html] usb:1-3\n\n";
        let d = parse_devices(text);
        assert_eq!(d.len(), 4);
        assert_eq!(d[0].state, DeviceState::Device);
        assert_eq!(d[0].model.as_deref(), Some("SM_G973F"));
        assert_eq!(d[1].state, DeviceState::Unauthorized);
        assert_eq!(d[2].state, DeviceState::Offline);
        assert_eq!(d[3].state, DeviceState::NoPermissions);
    }

    #[test]
    fn keyguard_fields() {
        assert_eq!(
            keyguard_showing(
                "  mShowingDream=false mDreamingLockscreen=true mDreamingSleepToken=null"
            ),
            Some(true)
        );
        assert_eq!(
            keyguard_showing("KeyguardController:\n    mKeyguardShowing=false\n"),
            Some(false)
        );
        assert_eq!(keyguard_showing("nothing relevant"), None);
    }

    #[test]
    fn quoting_survives_quotes() {
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }
}
