//! A path as bytes and back: the one place the platform decides how.
//!
//! On unix a name is any bytes, so a lossy copy would name a different file;
//! elsewhere (Windows) a path is wide text and only its UTF-8 form can be
//! stored, lossily when it is not valid Unicode.

use std::path::{Path, PathBuf};

#[cfg(unix)]
mod platform {
    use super::*;
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    pub(super) fn raw_bytes(path: &Path) -> Option<&[u8]> {
        Some(path.as_os_str().as_bytes())
    }

    pub(super) fn from_bytes(bytes: &[u8]) -> PathBuf {
        PathBuf::from(OsStr::from_bytes(bytes))
    }
}

#[cfg(not(unix))]
mod platform {
    use super::*;

    pub(super) fn raw_bytes(_path: &Path) -> Option<&[u8]> {
        None
    }

    pub(super) fn from_bytes(bytes: &[u8]) -> PathBuf {
        PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

/// The path's own bytes where the platform has them (unix), `None` where it
/// does not.
pub(crate) fn raw_bytes(path: &Path) -> Option<&[u8]> {
    platform::raw_bytes(path)
}

/// A path read back from [`raw_bytes`] or [`stored_bytes`].
pub(crate) fn from_bytes(bytes: &[u8]) -> PathBuf {
    platform::from_bytes(bytes)
}

/// A path as the pedal stores it: the raw bytes where the platform has them,
/// else the UTF-8 text (lossy for a path that is not valid Unicode).
pub(crate) fn stored_bytes(path: &Path) -> std::borrow::Cow<'_, [u8]> {
    match raw_bytes(path) {
        Some(bytes) => std::borrow::Cow::Borrowed(bytes),
        None => std::borrow::Cow::Owned(path.to_string_lossy().into_owned().into_bytes()),
    }
}
