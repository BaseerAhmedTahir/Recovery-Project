//! Mobile recovery from the desktop side (SPEC.md section 6).
//!
//! - [`adb`] and [`android`]: logical extraction from an Android phone its
//!   holder has unlocked and authorized for USB debugging.
//! - [`ios`]: iOS backups - parse an existing one, or start one with
//!   libimobiledevice on a phone that trusts this computer.
//! - [`host`]: phone backups and sync caches already on this computer.
//! - `bridge` (feature `bridge`): the loopback-only receiver for the companion
//!   app, the one socket this project permits (SPEC.md 4.4).
//!
//! Nothing here reads a phone's storage at block level, bypasses a lock screen,
//! or runs without the device holder's explicit action on the device.

pub mod adb;
pub mod android;
#[cfg(feature = "bridge")]
pub mod bridge;
pub mod host;
pub mod ios;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("adb: {0}")]
    Adb(String),
    #[error("backup: {0}")]
    Backup(String),
    #[error("destination: {0}")]
    Destination(String),
    #[error("bridge: {0}")]
    Bridge(String),
    #[error("{0}")]
    Other(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
