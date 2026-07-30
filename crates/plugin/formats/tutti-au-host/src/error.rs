//! Error types for Audio Unit hosting.

use std::fmt;

#[cfg(target_os = "macos")]
use crate::types::*;

/// Errors returned by Audio Unit host operations.
#[derive(Debug, Clone)]
pub enum AuError {
    /// An AudioToolbox call returned a non-zero `OSStatus`. `function` names
    /// the failing call (for diagnostics), and `code` is the raw status.
    OsStatus {
        /// Name of the AudioToolbox function that failed.
        function: &'static str,
        /// Raw `OSStatus` returned by the call.
        code: i32,
    },
    /// A null `AudioComponent` handle was passed where a valid one was required.
    NullComponent,
    /// A buffer supplied to `process` was malformed or inconsistent with the
    /// configured stream (wrong frame count, mismatched channels, etc.).
    InvalidBuffer(String),
    /// The AU declined the requested sample rate: after the stream-format /
    /// `kAudioUnitProperty_SampleRate` writes, its ASBD still reports a
    /// different `mSampleRate`.
    ///
    /// Not recoverable the way a rejected channel count is. The channel count
    /// can be re-read and the render scratch resized to match, but a rate the
    /// AU is not running at silently corrupts everything downstream that trusts
    /// it — the render is at the wrong rate (pitch/time drift) and PDC latency
    /// is computed against a rate that does not exist.
    ///
    /// Rates are raw `f64` Hz, not `tutti_types::Hz`: these are the exact bytes
    /// read out of / written into the AudioToolbox `AudioStreamBasicDescription`
    /// at the C ABI boundary, and the diagnostic's whole job is to report what
    /// crossed that boundary verbatim.
    SampleRateRejected {
        /// Which bus disagreed — `"input"` or `"output"`.
        scope: &'static str,
        /// The rate this host asked for, in Hz.
        requested: f64,
        /// The rate the AU reports it is actually running at, in Hz.
        accepted: f64,
    },
    /// The AU declined the requested block size: after the
    /// `MaximumFramesPerSlice` write it still reports a different maximum.
    ///
    /// Fatal for the same reason [`AuError::SampleRateRejected`] is, but through
    /// a sharper edge. `MaximumFramesPerSlice` is what the AU sizes its internal
    /// buffers from at `AudioUnitInitialize`, and
    /// [`AuInstance::process`](crate::instance::AuInstance::process) admits any
    /// `num_frames` up to the *recorded* block size. So a config holding a larger
    /// figure than the AU accepted disables that bound check in the unsafe
    /// direction: the render proceeds and the AU writes past buffers it allocated
    /// for fewer frames.
    ///
    /// Frame counts are raw `u32`, matching the property's C type — this is the
    /// value that crossed the AudioToolbox ABI, reported verbatim.
    BlockSizeRejected {
        /// The maximum block size this host asked for, in frames.
        requested: u32,
        /// The maximum the AU reports it actually allocated for, in frames.
        accepted: u32,
    },
    /// `AudioUnitRender` returned a non-`noErr` status. `code` is the render
    /// call's own OSStatus; `last_render_error` is the AU's
    /// `kAudioUnitProperty_LastRenderError` at failure time, when it could be
    /// read and was itself non-`noErr` — diagnostics only, enriching the render
    /// status with the underlying error the AU recorded internally (A-2).
    RenderFailed {
        /// The failing call — always `"AudioUnitRender"`.
        function: &'static str,
        /// Raw `OSStatus` returned by `AudioUnitRender`.
        code: i32,
        /// The AU's last-render-error, if it could be queried and was non-zero.
        last_render_error: Option<i32>,
    },
    /// A `.aupreset` file could not be read or written. Filesystem-level only —
    /// the file's *contents* fail as [`AuError::InvalidPreset`].
    ///
    /// Boxed for the reason [`AuError::PresetIdentityMismatch`] is: `AuError` is
    /// the `Err` of every `Result` in this crate, and two inline `String`s here
    /// widened the enum enough to push `AuReady::uninitialize`'s
    /// `(AuReady, AuError)` past clippy's `result_large_err` threshold. A preset
    /// diagnostic must not tax the render path's result size.
    PresetIo(Box<PresetFileError>),
    /// A `.aupreset` file is not a usable preset: not a property list at all, a
    /// plist whose root is not a dictionary, a truncated file, or a dictionary
    /// missing the identity keys a host needs to validate it.
    ///
    /// Distinct from [`AuError::PresetIdentityMismatch`], which is a *well-formed*
    /// preset for a different plugin. A host reports the two differently: this one
    /// means the file is broken, that one means the user picked the wrong file.
    ///
    /// Boxed for the same size reason as [`AuError::PresetIo`].
    InvalidPreset(Box<PresetFileError>),
    /// A `.aupreset` file is well-formed but belongs to a **different** AU.
    ///
    /// Refused rather than applied, and this is the variant the whole
    /// [`crate::aupreset`] module exists to produce. Measured on macOS 15.6: an AU
    /// handed a dictionary bearing its own identity keys but another plugin's
    /// `data` blob *accepts* it and adopts nonsense parameter values (AUDelay took
    /// a 0.5 Hz lowpass cutoff where it had 15 kHz). The AU trusts these keys, so
    /// the host is the only thing that can check them.
    ///
    /// Both triples are reported as decoded four-char strings because that is the
    /// form a user can match against a plugin name; the comparison itself happens
    /// on the raw codes.
    ///
    /// Boxed because this is the widest variant by far — a path plus six four-char
    /// strings — and `AuError` is the `Err` of every `Result` in the crate,
    /// including `process`'s. Inlining it grew every one of those results by ~144
    /// bytes for a diagnostic that only materialises on a rejected file
    /// (`clippy::result_large_err`).
    PresetIdentityMismatch(Box<PresetMismatch>),
}

/// A path plus what went wrong with it, shared by [`AuError::PresetIo`] and
/// [`AuError::InvalidPreset`].
///
/// One struct for both because the two carry the same shape and differ only in
/// *kind*: `PresetIo` means the bytes never arrived, `InvalidPreset` means they
/// arrived and were not a preset. Keeping the distinction in the variant rather
/// than in a field is what lets a caller `match` on it without inspecting a
/// string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresetFileError {
    /// The offending path (or a `<…>` placeholder when the dictionary came from an
    /// AU rather than a file).
    pub path: String,
    /// What specifically was wrong, for a message a user can act on.
    pub message: String,
}

/// The two component triples a [`AuError::PresetIdentityMismatch`] compares, and
/// the file they disagree about.
///
/// A named struct rather than inline variant fields so the variant can be boxed
/// without the call sites growing a tuple of seven positional `String`s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresetMismatch {
    /// The offending path.
    pub path: String,
    /// `componentType` the file claims.
    pub file_type: String,
    /// `componentSubType` the file claims.
    pub file_sub_type: String,
    /// `componentManufacturer` the file claims.
    pub file_manufacturer: String,
    /// `componentType` of the AU it was offered to.
    pub au_type: String,
    /// `componentSubType` of the AU it was offered to.
    pub au_sub_type: String,
    /// `componentManufacturer` of the AU it was offered to.
    pub au_manufacturer: String,
}

/// Convenience alias for `Result<T, AuError>`.
pub type Result<T> = std::result::Result<T, AuError>;

impl AuError {
    /// Construct a [`AuError::RenderFailed`] from a failed `AudioUnitRender`
    /// call, optionally enriched with the AU's last-render-error (A-2).
    pub(crate) fn render_failed(
        function: &'static str,
        code: i32,
        last_render_error: Option<i32>,
    ) -> Self {
        AuError::RenderFailed {
            function,
            code,
            last_render_error,
        }
    }

    /// Construct an [`AuError::InvalidPreset`] for `path`.
    ///
    /// A helper rather than an inline `Box::new(PresetFileError { .. })` at each of
    /// the eight construction sites, matching what
    /// [`AuError::render_failed`](Self::render_failed) does for the render path.
    pub(crate) fn invalid_preset(path: impl Into<String>, message: impl Into<String>) -> Self {
        AuError::InvalidPreset(Box::new(PresetFileError {
            path: path.into(),
            message: message.into(),
        }))
    }

    /// Construct an [`AuError::PresetIo`] for `path`.
    pub(crate) fn preset_io(path: impl Into<String>, message: impl Into<String>) -> Self {
        AuError::PresetIo(Box::new(PresetFileError {
            path: path.into(),
            message: message.into(),
        }))
    }

    /// Returns a human-readable description of the error.
    ///
    /// For `OsStatus` errors this decodes well-known AudioUnit status codes
    /// (e.g. `-10867` → `"uninitialized"`) and falls back to `"unknown error"`.
    pub fn message(&self) -> &'static str {
        #[cfg(target_os = "macos")]
        {
            let code = match self {
                AuError::OsStatus { code, .. } => *code,
                AuError::RenderFailed { code, .. } => *code,
                AuError::NullComponent => return "null component",
                AuError::InvalidBuffer(_) => return "invalid buffer",
                AuError::SampleRateRejected { .. } => return "sample rate rejected",
                AuError::BlockSizeRejected { .. } => return "block size rejected",
                AuError::PresetIo(_) => return "preset file I/O failed",
                AuError::InvalidPreset(_) => return "not a valid .aupreset",
                AuError::PresetIdentityMismatch(_) => {
                    return "preset belongs to a different Audio Unit"
                }
            };
            match code {
                K_AUDIO_UNIT_ERR_INVALID_PROPERTY => "invalid property",
                K_AUDIO_UNIT_ERR_INVALID_PARAMETER => "invalid parameter",
                K_AUDIO_UNIT_ERR_INVALID_ELEMENT => "invalid element",
                K_AUDIO_UNIT_ERR_NO_CONNECTION => "no connection",
                K_AUDIO_UNIT_ERR_FAILED_INITIALIZATION => "failed initialization",
                K_AUDIO_UNIT_ERR_TOO_MANY_FRAMES_TO_PROCESS => "too many frames to process",
                K_AUDIO_UNIT_ERR_INVALID_FILE => "invalid file",
                K_AUDIO_UNIT_ERR_UNKNOWN_FILE_TYPE => "unknown file type",
                K_AUDIO_UNIT_ERR_FILE_NOT_SPECIFIED => "file not specified",
                K_AUDIO_UNIT_ERR_FORMAT_NOT_SUPPORTED => "format not supported",
                K_AUDIO_UNIT_ERR_UNINITIALIZED => "uninitialized",
                K_AUDIO_UNIT_ERR_INVALID_SCOPE => "invalid scope",
                K_AUDIO_UNIT_ERR_PROPERTY_NOT_WRITABLE => "property not writable",
                K_AUDIO_UNIT_ERR_CANNOT_DO_IN_CURRENT_CONTEXT => "cannot do in current context",
                K_AUDIO_UNIT_ERR_INVALID_PROPERTY_VALUE => "invalid property value",
                K_AUDIO_UNIT_ERR_PROPERTY_NOT_IN_USE => "property not in use",
                K_AUDIO_UNIT_ERR_INITIALIZED => "already initialized",
                K_AUDIO_UNIT_ERR_INVALID_OFFLINE_RENDER => "invalid offline render",
                K_AUDIO_UNIT_ERR_UNAUTHORIZED => "unauthorized",
                _ => "unknown error",
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            match self {
                AuError::NullComponent => "null component",
                AuError::InvalidBuffer(_) => "invalid buffer",
                AuError::SampleRateRejected { .. } => "sample rate rejected",
                AuError::BlockSizeRejected { .. } => "block size rejected",
                AuError::PresetIo(_) => "preset file I/O failed",
                AuError::InvalidPreset(_) => "not a valid .aupreset",
                AuError::PresetIdentityMismatch(_) => "preset belongs to a different Audio Unit",
                _ => "unknown error",
            }
        }
    }
}

impl fmt::Display for AuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuError::OsStatus { function, code } => write!(
                f,
                "AudioUnit error in {}: OSStatus {} ({})",
                function,
                code,
                self.message()
            ),
            AuError::RenderFailed {
                function,
                code,
                last_render_error,
            } => match last_render_error {
                Some(last) => write!(
                    f,
                    "AudioUnit error in {}: OSStatus {} ({}); last render error {}",
                    function,
                    code,
                    self.message(),
                    last
                ),
                None => write!(
                    f,
                    "AudioUnit error in {}: OSStatus {} ({})",
                    function,
                    code,
                    self.message()
                ),
            },
            AuError::NullComponent => write!(f, "null AudioComponent handle"),
            AuError::InvalidBuffer(msg) => write!(f, "invalid buffer: {msg}"),
            AuError::SampleRateRejected {
                scope,
                requested,
                accepted,
            } => write!(
                f,
                "AU rejected the {scope} sample rate: requested {requested} Hz, \
                 AU reports {accepted} Hz"
            ),
            AuError::BlockSizeRejected {
                requested,
                accepted,
            } => write!(
                f,
                "AU rejected the block size: requested {requested} frames, \
                 AU reports {accepted} frames"
            ),
            AuError::PresetIo(e) => {
                write!(f, "preset file I/O failed for {}: {}", e.path, e.message)
            }
            AuError::InvalidPreset(e) => {
                write!(f, "{} is not a valid .aupreset: {}", e.path, e.message)
            }
            AuError::PresetIdentityMismatch(m) => {
                let PresetMismatch {
                    path,
                    file_type,
                    file_sub_type,
                    file_manufacturer,
                    au_type,
                    au_sub_type,
                    au_manufacturer,
                } = &**m;
                write!(
                    f,
                    "{path} is a preset for \
                     {file_type}/{file_sub_type}/{file_manufacturer}, but this AU is \
                     {au_type}/{au_sub_type}/{au_manufacturer}; refusing to apply \
                     another plugin's state"
                )
            }
        }
    }
}

impl std::error::Error for AuError {}
