//! The errors the safe handles return.

use crate::status::Status;

/// A failed AudioToolbox call: which one, and the `OSStatus` it answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{function} failed with OSStatus {status}")]
pub struct OsError {
    /// The AudioToolbox function that failed, for diagnostics.
    pub function: &'static str,
    /// The `OSStatus` it returned.
    pub status: i32,
}

impl OsError {
    /// The status, by name when it is a known `kAudioUnitErr_*` code.
    pub fn code(&self) -> Status {
        Status::from_raw(self.status)
    }
}

/// What a safe handle can fail with.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// An AudioToolbox call answered a non-zero `OSStatus`.
    #[error(transparent)]
    Os(#[from] OsError),
    /// A value the caller handed over, or one the system handed back, was not
    /// usable (a plist that does not decode, a size the property does not
    /// have). The text says which.
    #[error("invalid: {0}")]
    Invalid(String),
    /// A system function this macOS does not have (one added after the host's
    /// minimum deployment target, looked up at run time).
    #[error("{0} is not available on this macOS")]
    Unavailable(&'static str),
    /// A render call failed; `last` is the unit's own last-render-error
    /// property, when it reports one.
    #[error("{function} failed with OSStatus {status}")]
    RenderFailed {
        /// The AudioToolbox function that failed.
        function: &'static str,
        /// The `OSStatus` it returned.
        status: i32,
        /// What `kAudioUnitProperty_LastRenderError` reports, if non-zero.
        last: Option<i32>,
    },
}

impl Error {
    /// The `OSStatus` of an [`Error::Os`], or `None`.
    pub fn status(&self) -> Option<i32> {
        match self {
            Error::Os(e) => Some(e.status),
            Error::RenderFailed { status, .. } => Some(*status),
            Error::Invalid(_) | Error::Unavailable(_) => None,
        }
    }
}

/// The result of a safe handle's call.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Turn a raw `OSStatus` into a `Result`, naming the function that returned it.
///
/// # Errors
/// An [`OsError`] carrying `function` and `status` when `status` is non-zero.
pub fn check(function: &'static str, status: i32) -> std::result::Result<(), OsError> {
    if status == 0 {
        Ok(())
    } else {
        Err(OsError { function, status })
    }
}
