//! Seeing and touching a phone whose screen is broken, over the USB cable.
//!
//! When the glass is cracked or the digitiser is dead, the phone still works:
//! `screencap` renders what is on the display and `input` delivers taps and
//! keys. Together they are enough to unlock the phone with its own code and
//! reach the files - the common "the screen is broken, my photos are on there"
//! case that otherwise ends at a repair shop.
//!
//! The same conditions as everything else here (SPEC.md section 0): the
//! phone must already have USB debugging switched on and this computer's key
//! accepted on the phone. That is a deliberate wall, not an oversight: a phone
//! that was never set up this way cannot be reached, and nothing in this file
//! changes that.
//!
//! What this is not: [`type_text`] types a code that is given to it, one code
//! per call. It cannot guess one, and there is no loop here that tries codes -
//! that is what turns a repair tool into a tool for stolen phones. Android
//! itself also slows and eventually wipes after repeated wrong codes, so a
//! wrong guess costs the data this program exists to save.

use crate::adb::{shell_quote, Adb};
use crate::{Error, Result};

/// A key that can be sent. Named rather than numeric so the CLI and the GUI
/// cannot send arbitrary key events by number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Power,
    Home,
    Back,
    Menu,
    Enter,
    Delete,
    Up,
    Down,
    Left,
    Right,
    VolumeUp,
    VolumeDown,
    Wake,
}

impl Key {
    /// Android keycodes, from `KeyEvent`.
    pub fn code(self) -> u32 {
        match self {
            Key::Back => 4,
            Key::Home => 3,
            Key::Menu => 82,
            Key::Power => 26,
            Key::Up => 19,
            Key::Down => 20,
            Key::Left => 21,
            Key::Right => 22,
            Key::Enter => 66,
            Key::Delete => 67,
            Key::VolumeUp => 24,
            Key::VolumeDown => 25,
            Key::Wake => 224,
        }
    }

    pub fn parse(name: &str) -> Option<Key> {
        Some(
            match name.to_ascii_lowercase().replace(['-', '_'], "").as_str() {
                "power" => Key::Power,
                "home" => Key::Home,
                "back" => Key::Back,
                "menu" => Key::Menu,
                "enter" | "ok" => Key::Enter,
                "delete" | "del" | "backspace" => Key::Delete,
                "up" => Key::Up,
                "down" => Key::Down,
                "left" => Key::Left,
                "right" => Key::Right,
                "volumeup" | "volup" => Key::VolumeUp,
                "volumedown" | "voldown" => Key::VolumeDown,
                "wake" => Key::Wake,
                _ => return None,
            },
        )
    }

    pub const ALL: &'static [&'static str] = &[
        "power",
        "wake",
        "home",
        "back",
        "menu",
        "enter",
        "delete",
        "up",
        "down",
        "left",
        "right",
        "volume-up",
        "volume-down",
    ];
}

/// The display's size in pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScreenSize {
    pub width: u32,
    pub height: u32,
}

/// Parse `wm size` output. An "Override size" wins: that is what is actually
/// being drawn, and what `input tap` coordinates are in.
pub fn parse_wm_size(text: &str) -> Option<ScreenSize> {
    let mut physical = None;
    let mut override_size = None;
    for line in text.lines() {
        let line = line.trim();
        let value = |prefix: &str| -> Option<ScreenSize> {
            let rest = line.strip_prefix(prefix)?.trim();
            let (w, h) = rest.split_once('x')?;
            Some(ScreenSize {
                width: w.trim().parse().ok()?,
                height: h.trim().parse().ok()?,
            })
        };
        if let Some(s) = value("Physical size:") {
            physical = Some(s);
        } else if let Some(s) = value("Override size:") {
            override_size = Some(s);
        }
    }
    override_size
        .or(physical)
        .filter(|s| s.width > 0 && s.height > 0)
}

/// Whether the display is on, from `dumpsys power`. `None` when this Android
/// version reports neither field.
pub fn parse_display_on(dumpsys_power: &str) -> Option<bool> {
    for key in ["mScreenOn=", "Display Power: state=", "mWakefulness="] {
        for (i, _) in dumpsys_power.match_indices(key) {
            let rest = dumpsys_power[i + key.len()..].trim_start();
            let word: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect();
            match word.to_ascii_uppercase().as_str() {
                "TRUE" | "ON" | "AWAKE" => return Some(true),
                "FALSE" | "OFF" | "ASLEEP" | "DOZE" => return Some(false),
                _ => {}
            }
        }
    }
    None
}

/// The display's size, asked of the phone.
pub fn screen_size(adb: &Adb) -> Result<ScreenSize> {
    let out = adb.shell("wm size")?;
    parse_wm_size(&out).ok_or_else(|| {
        Error::Adb(format!(
            "the phone did not report a screen size (it said {:?})",
            out.trim()
        ))
    })
}

/// A PNG of what is on the phone's screen right now.
///
/// `exec-out` rather than `shell` so the bytes arrive unchanged; older adb
/// bridges mangled line endings in binary output.
pub fn screenshot(adb: &Adb) -> Result<Vec<u8>> {
    let png = adb.run(&["exec-out", "screencap", "-p"])?;
    if png.starts_with(b"\x89PNG") {
        return Ok(png);
    }
    // Some devices only have the older path, which needs the CRLF repair.
    let raw = adb.run(&["shell", "screencap", "-p"])?;
    let repaired = raw
        .windows(2)
        .enumerate()
        .filter(|(_, w)| w == b"\r\n")
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
    if !repaired.is_empty() {
        let mut out = Vec::with_capacity(raw.len());
        let mut skip = false;
        for (i, b) in raw.iter().enumerate() {
            if skip {
                skip = false;
                continue;
            }
            if repaired.binary_search(&i).is_ok() {
                out.push(b'\n');
                skip = true;
            } else {
                out.push(*b);
            }
        }
        if out.starts_with(b"\x89PNG") {
            return Ok(out);
        }
    }
    Err(Error::Adb(
        "the phone did not return a screenshot; screencap may be blocked on this device"
            .to_string(),
    ))
}

fn check_point(size: ScreenSize, x: u32, y: u32) -> Result<()> {
    if x >= size.width || y >= size.height {
        return Err(Error::Adb(format!(
            "({x},{y}) is outside the phone's {}x{} screen",
            size.width, size.height
        )));
    }
    Ok(())
}

/// Tap once, as a finger would.
pub fn tap(adb: &Adb, size: ScreenSize, x: u32, y: u32) -> Result<()> {
    check_point(size, x, y)?;
    adb.shell(&format!("input tap {x} {y}"))?;
    Ok(())
}

/// Drag from one point to another over `ms` milliseconds - a swipe, or a
/// slow drag when `ms` is large.
pub fn swipe(adb: &Adb, size: ScreenSize, from: (u32, u32), to: (u32, u32), ms: u32) -> Result<()> {
    check_point(size, from.0, from.1)?;
    check_point(size, to.0, to.1)?;
    let ms = ms.clamp(20, 10_000);
    adb.shell(&format!(
        "input swipe {} {} {} {} {ms}",
        from.0, from.1, to.0, to.1
    ))?;
    Ok(())
}

/// Press a named key.
pub fn key(adb: &Adb, k: Key) -> Result<()> {
    adb.shell(&format!("input keyevent {}", k.code()))?;
    Ok(())
}

/// Type text: the unlock code the person knows, or a search box's contents.
///
/// One string per call, by hand. There is deliberately nothing here that tries
/// a series of codes.
pub fn type_text(adb: &Adb, text: &str) -> Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    if text.len() > 512 {
        return Err(Error::Adb("that is too long to type".to_string()));
    }
    // `input text` takes one argument and treats %s as a space.
    let arg = text.replace(' ', "%s");
    adb.shell(&format!("input text {}", shell_quote(&arg)))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screen_size_prefers_the_override() {
        let s = parse_wm_size("Physical size: 1080x2400\nOverride size: 720x1600\n").unwrap();
        assert_eq!(
            s,
            ScreenSize {
                width: 720,
                height: 1600
            }
        );
        let s = parse_wm_size("Physical size: 1440x3120").unwrap();
        assert_eq!(
            s,
            ScreenSize {
                width: 1440,
                height: 3120
            }
        );
        assert_eq!(parse_wm_size("nothing here"), None);
        assert_eq!(parse_wm_size("Physical size: 0x0"), None);
    }

    #[test]
    fn display_state_is_read_from_any_of_the_known_fields() {
        assert_eq!(parse_display_on("mScreenOn=true"), Some(true));
        assert_eq!(parse_display_on("  mScreenOn=false\n"), Some(false));
        assert_eq!(parse_display_on("Display Power: state=OFF"), Some(false));
        assert_eq!(parse_display_on("mWakefulness=Awake"), Some(true));
        assert_eq!(parse_display_on("mWakefulness=Asleep"), Some(false));
        assert_eq!(parse_display_on("something else entirely"), None);
    }

    #[test]
    fn keys_are_named_not_numbered() {
        assert_eq!(Key::parse("power"), Some(Key::Power));
        assert_eq!(Key::parse("VOLUME-UP"), Some(Key::VolumeUp));
        assert_eq!(Key::parse("volume_down"), Some(Key::VolumeDown));
        assert_eq!(Key::parse("26"), None, "raw keycodes are not accepted");
        assert_eq!(Key::parse("call"), None);
        for name in Key::ALL {
            assert!(Key::parse(name).is_some(), "{name} is listed but unparsed");
        }
    }

    #[test]
    fn taps_outside_the_screen_are_refused() {
        // No phone needed: the bounds check happens before adb is called.
        let size = ScreenSize {
            width: 1080,
            height: 2400,
        };
        assert!(check_point(size, 1079, 2399).is_ok());
        assert!(check_point(size, 1080, 10).is_err());
        assert!(check_point(size, 10, 2400).is_err());
    }
}
