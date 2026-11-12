//! `tresult`, as a Rust error.

use vst3::Steinberg::{
    kInternalError, kInvalidArgument, kNoInterface, kNotImplemented, kNotInitialized, kOutOfMemory,
    kResultFalse, kResultOk, tresult,
};

/// A VST3 call that did not answer `kResultOk`, by name.
///
/// `kResultTrue` is `kResultOk` in the SDK (both are `0`), so a call whose
/// "false" is a real answer, as opposed to a failure, reports a `bool` and not
/// this: the wrapper says so where it is documented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Vst3Error {
    /// `kResultFalse`: the call ran and the answer is "no".
    False,
    /// `kInvalidArgument`.
    InvalidArgument,
    /// `kNotImplemented`: the plugin does not offer this call.
    NotImplemented,
    /// `kInternalError`.
    InternalError,
    /// `kNotInitialized`: the plugin is not in a state that takes the call.
    NotInitialized,
    /// `kOutOfMemory`.
    OutOfMemory,
    /// `kNoInterface`: a `queryInterface` the object does not answer.
    NoInterface,
    /// Not a `tresult`: the audio half of the plugin is out, so the plugin's
    /// state cannot change under it.
    Busy,
    /// Any other code, passed through unmapped.
    Other(i32),
}

impl Vst3Error {
    /// Names a non-OK `tresult`.
    #[must_use]
    pub fn from_code(code: tresult) -> Self {
        match code {
            c if c == kResultFalse => Self::False,
            c if c == kInvalidArgument => Self::InvalidArgument,
            c if c == kNotImplemented => Self::NotImplemented,
            c if c == kInternalError => Self::InternalError,
            c if c == kNotInitialized => Self::NotInitialized,
            c if c == kOutOfMemory => Self::OutOfMemory,
            c if c == kNoInterface => Self::NoInterface,
            c => Self::Other(c),
        }
    }

    /// The raw `tresult` this stands for.
    #[must_use]
    pub fn code(self) -> tresult {
        match self {
            Self::False => kResultFalse,
            Self::InvalidArgument => kInvalidArgument,
            Self::NotImplemented => kNotImplemented,
            Self::InternalError => kInternalError,
            Self::NotInitialized => kNotInitialized,
            Self::OutOfMemory => kOutOfMemory,
            Self::NoInterface => kNoInterface,
            Self::Busy => kInternalError,
            Self::Other(code) => code,
        }
    }
}

impl std::fmt::Display for Vst3Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::False => f.write_str("kResultFalse"),
            Self::InvalidArgument => f.write_str("kInvalidArgument"),
            Self::NotImplemented => f.write_str("kNotImplemented"),
            Self::InternalError => f.write_str("kInternalError"),
            Self::NotInitialized => f.write_str("kNotInitialized"),
            Self::OutOfMemory => f.write_str("kOutOfMemory"),
            Self::NoInterface => f.write_str("kNoInterface"),
            Self::Busy => f.write_str("the audio half is out"),
            Self::Other(code) => write!(f, "tresult {code}"),
        }
    }
}

impl std::error::Error for Vst3Error {}

/// `kResultOk` (and so `kResultTrue`) is success; any other code is the error
/// it names.
pub(crate) fn check(code: tresult) -> Result<(), Vst3Error> {
    if code == kResultOk {
        Ok(())
    } else {
        Err(Vst3Error::from_code(code))
    }
}

/// Whether `code` is `kResultOk`, which `kResultTrue` is too (both are `0`): for
/// a call whose `kResultFalse` is an answer, not an error to report.
pub(crate) fn is_ok(code: tresult) -> bool {
    code == kResultOk
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every named variant round-trips through its code, and an unnamed code
    /// keeps its value.
    #[test]
    fn a_code_round_trips_through_its_name() {
        for e in [
            Vst3Error::False,
            Vst3Error::InvalidArgument,
            Vst3Error::NotImplemented,
            Vst3Error::InternalError,
            Vst3Error::NotInitialized,
            Vst3Error::OutOfMemory,
            Vst3Error::NoInterface,
            Vst3Error::Other(12345),
        ] {
            assert_eq!(Vst3Error::from_code(e.code()), e);
        }
    }

    /// `kResultOk` is the only success, and `kResultFalse` is an error.
    #[test]
    fn only_ok_is_success() {
        assert_eq!(check(kResultOk), Ok(()));
        assert_eq!(check(kResultFalse), Err(Vst3Error::False));
        assert_eq!(check(kInvalidArgument), Err(Vst3Error::InvalidArgument));
    }
}
