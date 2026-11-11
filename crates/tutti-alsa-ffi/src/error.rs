//! An alsa-lib failure: the call and its code.

use std::ffi::CStr;
use std::fmt;

/// An alsa-lib call that returned a negative code.
///
/// ALSA reports failure as a negated `errno`, so the sign carries the meaning
/// and the magnitude is the code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Error {
    /// The alsa-lib function that failed, as a static label.
    pub operation: &'static str,
    /// alsa-lib's negative return.
    pub code: i32,
}

/// What this crate's calls return.
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// `snd_strerror` for the code: "No such file or directory" beats `-2` in a log.
    #[must_use]
    pub fn message(&self) -> String {
        message(self.code)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ALSA error: {} ({})", self.operation, self.code)
    }
}

impl std::error::Error for Error {}

/// `snd_strerror` as an owned string.
pub(crate) fn message(code: i32) -> String {
    // SAFETY: `snd_strerror` takes any int and returns a static NUL-terminated
    // string (or NULL, handled below).
    let ptr = unsafe { crate::sys::snd_strerror(code) };
    if ptr.is_null() {
        return format!("error {code}");
    }
    // SAFETY: non-NULL, NUL-terminated and static, as above; copied at once.
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

/// A return code as a result: failure is negative, anything else is the value.
pub(crate) fn check(code: i32, operation: &'static str) -> Result<i32> {
    if code < 0 {
        Err(Error { operation, code })
    } else {
        Ok(code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_negative_code_is_the_failure_and_zero_is_a_value() {
        assert_eq!(
            check(-2, "op"),
            Err(Error {
                operation: "op",
                code: -2
            })
        );
        assert_eq!(check(0, "op"), Ok(0));
        assert_eq!(check(7, "op"), Ok(7));
    }

    #[test]
    fn the_message_is_the_librarys() {
        assert!(message(-2).contains("No such file"));
    }
}
