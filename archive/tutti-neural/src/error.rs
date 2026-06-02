//! Crate error type.
//!
//! [`enum@Error`] covers the surface area of the public API
//! ([`engine`](crate::engine()), [`Engine`](crate::Engine) methods, asset
//! probing). Lower-level backend failures surface as
//! [`BackendError`](crate::BackendError) and are wrapped into
//! [`Error::Inference`] when they cross through the [`Engine`](crate::Engine).

use thiserror::Error;

/// Convenience `Result` alias with `Error` as the error type.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors from neural inference operations.
#[derive(Debug, Error)]
pub enum Error {
    /// Error from another Tutti subsystem (transport, I/O, etc.).
    #[error("audio system error: {0}")]
    TuttiCore(#[from] tutti_core::Error),

    /// Wrapped [`BackendError`](crate::BackendError) stringified. Covers
    /// model-not-found, forward-pass failures, backend init errors.
    #[error("inference error: {0}")]
    Inference(String),

    /// Builder or runtime config rejected (e.g. probe deemed the model
    /// unusable as a same-rate audio model).
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),

    /// Engine thread disconnected while the caller was waiting for a reply.
    #[error("inference thread send failed")]
    InferenceThreadSend,

    /// Engine thread disconnected while the caller was waiting for a reply.
    #[error("inference thread recv failed")]
    InferenceThreadRecv,
}
