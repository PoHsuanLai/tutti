//! The error every call here returns.

use windows_sys::Win32::Media::MMSYSERR_NOERROR;

/// A winmm call that failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("winmm error: {operation} ({code})")]
pub struct Error {
    /// What was being done, as a static label.
    pub operation: &'static str,
    /// The `MMRESULT` winmm returned (`MMSYSERR_*` or `MIDIERR_*`), or
    /// [`Error::NOT_RETURNED`] where no call returned one.
    pub code: u32,
}

impl Error {
    /// The `code` of a failure no winmm call reported: a buffer winmm still
    /// holds after a timeout.
    pub const NOT_RETURNED: u32 = u32::MAX;
}

/// `Ok` for `MMSYSERR_NOERROR`, the failure of `operation` otherwise.
pub(crate) fn check(operation: &'static str, code: u32) -> Result<(), Error> {
    if code == MMSYSERR_NOERROR {
        Ok(())
    } else {
        Err(Error { operation, code })
    }
}
