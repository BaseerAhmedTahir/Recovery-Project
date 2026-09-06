//! Errors for partition discovery.

pub type Result<T> = std::result::Result<T, PartitionError>;

#[derive(Debug, thiserror::Error)]
pub enum PartitionError {
    #[error("device is too small to hold a partition table ({sectors} sectors)")]
    TooSmall { sectors: u64 },

    #[error("the {which} GPT is corrupt: {detail}")]
    CorruptGpt { which: &'static str, detail: String },

    #[error(transparent)]
    Device(#[from] rc_device::DeviceError),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}
