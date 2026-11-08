//! The errors the safe handles return.

use std::fmt;

/// A failed AudioToolbox call: which one, and the `OSStatus` it answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OsError {
    /// The AudioToolbox function that failed, for diagnostics.
    pub function: &'static str,
    /// The `OSStatus` it returned.
    pub status: i32,
}

impl fmt::Display for OsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} failed with OSStatus {}", self.function, self.status)
    }
}

impl std::error::Error for OsError {}

/// What a safe handle can fail with.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// An AudioToolbox call answered a non-zero `OSStatus`.
    Os(OsError),
    /// A value the caller handed over, or one the system handed back, was not
    /// usable (a plist that does not decode, a size the property does not
    /// have). The text says which.
    Invalid(String),
    /// A render call failed; `last` is the unit's own last-render-error
    /// property, when it reports one.
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
            Error::Invalid(_) => None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Os(e) => e.fmt(f),
            Error::Invalid(why) => write!(f, "invalid: {why}"),
            Error::RenderFailed {
                function, status, ..
            } => write!(f, "{function} failed with OSStatus {status}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<OsError> for Error {
    fn from(e: OsError) -> Self {
        Error::Os(e)
    }
}

/// The result of a safe handle's call.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Turn a raw `OSStatus` into a `Result`, naming the function that returned it.
pub fn check(function: &'static str, status: i32) -> std::result::Result<(), OsError> {
    if status == 0 {
        Ok(())
    } else {
        Err(OsError { function, status })
    }
}
