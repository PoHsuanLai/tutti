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

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message(), self.raw())
    }
}

impl std::error::Error for Status {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each named constant maps to its own variant and back, and has a message.
    ///
    /// Mutation (run): delete or swap an arm of `from_raw` (or of `message`) ->
    /// the variant (or the "not unknown" assertion) fails for that constant.
    #[test]
    fn every_named_code_has_its_variant_and_message() {
        let named = [
            (sys::kAudioUnitErr_InvalidProperty, Status::InvalidProperty),
            (
                sys::kAudioUnitErr_InvalidParameter,
                Status::InvalidParameter,
            ),
            (sys::kAudioUnitErr_InvalidElement, Status::InvalidElement),
            (sys::kAudioUnitErr_NoConnection, Status::NoConnection),
            (
                sys::kAudioUnitErr_FailedInitialization,
                Status::FailedInitialization,
            ),
            (
                sys::kAudioUnitErr_TooManyFramesToProcess,
                Status::TooManyFramesToProcess,
            ),
            (sys::kAudioUnitErr_InvalidFile, Status::InvalidFile),
            (sys::kAudioUnitErr_UnknownFileType, Status::UnknownFileType),
            (
                sys::kAudioUnitErr_FileNotSpecified,
                Status::FileNotSpecified,
            ),
            (
                sys::kAudioUnitErr_FormatNotSupported,
                Status::FormatNotSupported,
            ),
            (sys::kAudioUnitErr_Uninitialized, Status::Uninitialized),
            (sys::kAudioUnitErr_InvalidScope, Status::InvalidScope),
            (
                sys::kAudioUnitErr_PropertyNotWritable,
                Status::PropertyNotWritable,
            ),
            (
                sys::kAudioUnitErr_CannotDoInCurrentContext,
                Status::CannotDoInCurrentContext,
            ),
            (
                sys::kAudioUnitErr_InvalidPropertyValue,
                Status::InvalidPropertyValue,
            ),
            (
                sys::kAudioUnitErr_PropertyNotInUse,
                Status::PropertyNotInUse,
            ),
            (sys::kAudioUnitErr_Initialized, Status::Initialized),
            (
                sys::kAudioUnitErr_InvalidOfflineRender,
                Status::InvalidOfflineRender,
            ),
            (sys::kAudioUnitErr_Unauthorized, Status::Unauthorized),
        ];
        assert_eq!(named.len(), 19);
        for (raw, status) in named {
            assert_eq!(Status::from_raw(raw), status, "{raw}");
            assert_eq!(status.raw(), raw);
            assert_ne!(status.message(), "unknown error", "{status:?}");
            assert!(status.to_string().contains(&raw.to_string()));
        }
    }

    /// Anything else is `Other`, keeping its raw value.
    #[test]
    fn an_unnamed_code_is_other() {
        assert_eq!(Status::from_raw(0), Status::Other(0));
        assert_eq!(Status::from_raw(-50), Status::Other(-50));
        assert_eq!(Status::Other(-50).message(), "unknown error");
        assert_eq!(Status::Other(-50).raw(), -50);
    }
}
