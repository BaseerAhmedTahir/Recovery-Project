//! Subcommand implementations. Each module owns its clap `Args` and a `run`.

pub mod devices;
pub mod image;
pub mod list_deleted;
pub mod smoke;
pub mod verify;
