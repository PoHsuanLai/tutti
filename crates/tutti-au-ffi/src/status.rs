//! The `OSStatus` codes an Audio Unit answers with.

use coreaudio_sys as sys;

/// `noErr`.
pub(crate) const NO_ERR: i32 = 0;

/// What a callback answers when it could not do its work (a contained panic, a
/// null argument it cannot serve).
pub(crate) const CANNOT_DO_IN_CURRENT_CONTEXT: i32 = sys::kAudioUnitErr_CannotDoInCurrentContext;

/// An `OSStatus` from an AudioToolbox call: the `kAudioUnitErr_*` family by
/// name, anything else as [`Status::Other`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Status {
    /// `kAudioUnitErr_InvalidProperty`: the unit does not implement the property.
    InvalidProperty,
    /// `kAudioUnitErr_InvalidParameter`: the unit has no such parameter.
    InvalidParameter,
    /// `kAudioUnitErr_InvalidElement`: the element (bus) is out of range.
    InvalidElement,
    /// `kAudioUnitErr_NoConnection`: the input bus has no source.
    NoConnection,
    /// `kAudioUnitErr_FailedInitialization`: the unit could not initialize.
    FailedInitialization,
    /// `kAudioUnitErr_TooManyFramesToProcess`: more frames than the maximum slice.
    TooManyFramesToProcess,
    /// `kAudioUnitErr_InvalidFile`: the file is not valid for the unit.
    InvalidFile,
    /// `kAudioUnitErr_UnknownFileType`: the file type is not recognized.
    UnknownFileType,
    /// `kAudioUnitErr_FileNotSpecified`: the unit needs a file and has none.
    FileNotSpecified,
    /// `kAudioUnitErr_FormatNotSupported`: the stream format is not supported.
    FormatNotSupported,
    /// `kAudioUnitErr_Uninitialized`: the call needs an initialized unit.
    Uninitialized,
    /// `kAudioUnitErr_InvalidScope`: the scope is not valid here.
    InvalidScope,
    /// `kAudioUnitErr_PropertyNotWritable`: the property is read-only.
    PropertyNotWritable,
    /// `kAudioUnitErr_CannotDoInCurrentContext`: not callable from this thread or state.
    CannotDoInCurrentContext,
    /// `kAudioUnitErr_InvalidPropertyValue`: the value is not valid for the property.
    InvalidPropertyValue,
    /// `kAudioUnitErr_PropertyNotInUse`: the property is not in use in this configuration.
    PropertyNotInUse,
    /// `kAudioUnitErr_Initialized`: the call needs an uninitialized unit.
    Initialized,
    /// `kAudioUnitErr_InvalidOfflineRender`: offline render in a state the unit cannot do.
    InvalidOfflineRender,
    /// `kAudioUnitErr_Unauthorized`: the process may not use the unit.
    Unauthorized,
    /// Any other status.
    Other(i32),
}

impl Status {
    /// The status for a raw `OSStatus`.
    pub fn from_raw(raw: i32) -> Self {
        match raw {
            sys::kAudioUnitErr_InvalidProperty => Self::InvalidProperty,
            sys::kAudioUnitErr_InvalidParameter => Self::InvalidParameter,
            sys::kAudioUnitErr_InvalidElement => Self::InvalidElement,
            sys::kAudioUnitErr_NoConnection => Self::NoConnection,
            sys::kAudioUnitErr_FailedInitialization => Self::FailedInitialization,
            sys::kAudioUnitErr_TooManyFramesToProcess => Self::TooManyFramesToProcess,
            sys::kAudioUnitErr_InvalidFile => Self::InvalidFile,
            sys::kAudioUnitErr_UnknownFileType => Self::UnknownFileType,
            sys::kAudioUnitErr_FileNotSpecified => Self::FileNotSpecified,
            sys::kAudioUnitErr_FormatNotSupported => Self::FormatNotSupported,
            sys::kAudioUnitErr_Uninitialized => Self::Uninitialized,
            sys::kAudioUnitErr_InvalidScope => Self::InvalidScope,
            sys::kAudioUnitErr_PropertyNotWritable => Self::PropertyNotWritable,
            sys::kAudioUnitErr_CannotDoInCurrentContext => Self::CannotDoInCurrentContext,
            sys::kAudioUnitErr_InvalidPropertyValue => Self::InvalidPropertyValue,
            sys::kAudioUnitErr_PropertyNotInUse => Self::PropertyNotInUse,
            sys::kAudioUnitErr_Initialized => Self::Initialized,
            sys::kAudioUnitErr_InvalidOfflineRender => Self::InvalidOfflineRender,
            sys::kAudioUnitErr_Unauthorized => Self::Unauthorized,
            other => Self::Other(other),
        }
    }

    /// The raw `OSStatus`.
    pub fn raw(self) -> i32 {
        match self {
            Self::InvalidProperty => sys::kAudioUnitErr_InvalidProperty,
            Self::InvalidParameter => sys::kAudioUnitErr_InvalidParameter,
            Self::InvalidElement => sys::kAudioUnitErr_InvalidElement,
            Self::NoConnection => sys::kAudioUnitErr_NoConnection,
            Self::FailedInitialization => sys::kAudioUnitErr_FailedInitialization,
            Self::TooManyFramesToProcess => sys::kAudioUnitErr_TooManyFramesToProcess,
            Self::InvalidFile => sys::kAudioUnitErr_InvalidFile,
            Self::UnknownFileType => sys::kAudioUnitErr_UnknownFileType,
            Self::FileNotSpecified => sys::kAudioUnitErr_FileNotSpecified,
            Self::FormatNotSupported => sys::kAudioUnitErr_FormatNotSupported,
            Self::Uninitialized => sys::kAudioUnitErr_Uninitialized,
            Self::InvalidScope => sys::kAudioUnitErr_InvalidScope,
            Self::PropertyNotWritable => sys::kAudioUnitErr_PropertyNotWritable,
            Self::CannotDoInCurrentContext => sys::kAudioUnitErr_CannotDoInCurrentContext,
            Self::InvalidPropertyValue => sys::kAudioUnitErr_InvalidPropertyValue,
            Self::PropertyNotInUse => sys::kAudioUnitErr_PropertyNotInUse,
            Self::Initialized => sys::kAudioUnitErr_Initialized,
            Self::InvalidOfflineRender => sys::kAudioUnitErr_InvalidOfflineRender,
            Self::Unauthorized => sys::kAudioUnitErr_Unauthorized,
            Self::Other(raw) => raw,
        }
    }

    /// A short description, `"unknown error"` for [`Status::Other`].
    pub fn message(self) -> &'static str {
        match self {
            Self::InvalidProperty => "invalid property",
            Self::InvalidParameter => "invalid parameter",
            Self::InvalidElement => "invalid element",
            Self::NoConnection => "no connection",
            Self::FailedInitialization => "failed initialization",
            Self::TooManyFramesToProcess => "too many frames to process",
            Self::InvalidFile => "invalid file",
            Self::UnknownFileType => "unknown file type",
            Self::FileNotSpecified => "file not specified",
            Self::FormatNotSupported => "format not supported",
            Self::Uninitialized => "uninitialized",
            Self::InvalidScope => "invalid scope",
            Self::PropertyNotWritable => "property not writable",
            Self::CannotDoInCurrentContext => "cannot do in current context",
            Self::InvalidPropertyValue => "invalid property value",
            Self::PropertyNotInUse => "property not in use",
            Self::Initialized => "already initialized",
            Self::InvalidOfflineRender => "invalid offline render",
            Self::Unauthorized => "unauthorized",
            Self::Other(_) => "unknown error",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every named code round-trips through `raw`, and an unnamed one is `Other`.
    /// Mutation (run): swap two arms in `from_raw` -> the round trip fails.
    #[test]
    fn named_codes_round_trip() {
        for code in -10880..=-10840 {
            let status = Status::from_raw(code);
            assert_eq!(status.raw(), code);
            if matches!(status, Status::Other(_)) {
                assert_eq!(status.message(), "unknown error");
            }
        }
        assert_eq!(Status::from_raw(-10879), Status::InvalidProperty);
        assert_eq!(Status::from_raw(0), Status::Other(0));
    }
}
