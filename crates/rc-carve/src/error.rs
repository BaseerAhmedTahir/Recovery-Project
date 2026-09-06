//! Errors for the carving engine.

pub type Result<T> = std::result::Result<T, CarveError>;

#[derive(Debug, thiserror::Error)]
pub enum CarveError {
    #[error("signature database: {detail}")]
    SignatureDb { detail: String },

    #[error("no validator registered for id {id:?}")]
    UnknownValidator { id: String },

    #[error("scan aborted: {detail}")]
    Aborted { detail: String },

    #[error(transparent)]
    Device(#[from] rc_device::DeviceError),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}
