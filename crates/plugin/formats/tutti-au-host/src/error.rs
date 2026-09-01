//! Error types for Audio Unit hosting.

use thiserror::Error;

#[cfg(target_os = "macos")]
use crate::types::*;

/// Plugin-load phase label. The shared superset lives in `tutti-plugin-types`;
/// AU uses the Opening/Instantiation/Setup/Initialization subset — it has no
/// distinct Scanning phase (the OS registry answers that, not this crate) and
/// no Factory one (`AudioComponentInstanceNew` takes the component directly).
/// Re-exported so `AuError` and callers keep referring to
/// `crate::error::LoadStage`.
pub use tutti_plugin_types::LoadStage;

/// Errors returned by Audio Unit host operations.
#[derive(Error, Debug, Clone)]
pub enum AuError {
    /// The AU could not be loaded. `stage` says how far the load got, which
    /// separates "this component cannot be instantiated at all" from "it
    /// instantiated and then refused its configuration".
    ///
    /// The same shape the other three host crates report a load failure in,
    /// with one substitution forced by the ABI: AU is constructed from an
    /// **OS-registered `AudioComponent`**, not a file
    /// (`AudioComponentInstanceNew` takes the component handle), so there is no
    /// path to carry. `component` names it the way a user can match it against
    /// a plugin list — `"aufx/dely/appl"`, the type/subtype/manufacturer triple
    /// the registry itself is keyed on.
    #[error("Failed to load AudioUnit {component}: {stage} - {reason}")]
    LoadFailed {
        /// The component the load was attempted against, as its decoded
        /// `type/subtype/manufacturer` four-char triple.
        component: String,
        /// The phase that failed — Opening, Instantiation, Setup or
        /// Initialization for AU.
        stage: LoadStage,
        /// Human-readable cause, for logs rather than for matching on.
        reason: String,
    },

    /// An AudioToolbox call returned a non-zero `OSStatus`. `function` names
    /// the failing call (for diagnostics), and `code` is the raw status.
    #[error(
        "AudioUnit error in {function}: OSStatus {code} ({})",
        os_status_message(*.code)
    )]
    OsStatus {
        /// Name of the AudioToolbox function that failed.
        function: &'static str,
        /// Raw `OSStatus` returned by the call.
        code: i32,
    },
    /// A null `AudioComponent` handle was passed where a valid one was required.
    #[error("null AudioComponent handle")]
    NullComponent,
    /// The component sets `kAudioComponentFlag_RequiresAsyncInstantiation`, so
    /// `AudioComponentInstanceNew` cannot create it.
    ///
    /// `AudioComponent.h:498-502`: `AudioComponentInstantiate` "must be used to
    /// instantiate any component with
    /// kAudioComponentFlag_RequiresAsyncInstantiation set in its component
    /// flags". The system sets that flag automatically for v3 audio units with
    /// views.
    ///
    /// A distinct variant because the synchronous call's own answer is
    /// `kAudioUnitErr_CannotDoInCurrentContext` (-10863) — "cannot do in
    /// current context", which reads like a transient condition worth retrying
    /// rather than a component this entry point can never create. Measured on
    /// macOS 15.6: of 138 installed components, 5 set the flag and all 5 return
    /// -10863 here, every time.
    #[error(
        "this component requires AudioComponentInstantiate (asynchronous); \
         it is a v3 Audio Unit with a view, which \
         AudioComponentInstanceNew cannot create"
    )]
    RequiresAsyncInstantiation,
    /// CoreFoundation declined to allocate a string the host needed to hand to
    /// the AU.
    ///
    /// A distinct variant rather than a silent `Ok`, because the caller of
    /// `identity::set_nick_name` persists that name: reporting success for a
    /// write that never happened would lose it from the session with nothing to
    /// show the user.
    #[error("CoreFoundation string allocation failed")]
    CfStringAlloc,
    /// A buffer supplied to `process` was malformed or inconsistent with the
    /// configured stream (wrong frame count, mismatched channels, etc.).
    #[error("invalid buffer: {0}")]
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
    #[error(
        "AU rejected the {scope} sample rate: requested {requested} Hz, \
         AU reports {accepted} Hz"
    )]
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
    /// buffers from at `AudioUnitInitialize`, and `AuInstance::process` admits
    /// any `num_frames` up to the *recorded* block size. So a config holding a larger
    /// figure than the AU accepted disables that bound check in the unsafe
    /// direction: the render proceeds and the AU writes past buffers it allocated
    /// for fewer frames.
    ///
    /// Frame counts are raw `u32`, matching the property's C type — this is the
    /// value that crossed the AudioToolbox ABI, reported verbatim.
    #[error(
        "AU rejected the block size: requested {requested} frames, \
         AU reports {accepted} frames"
    )]
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
    /// status with the underlying error the AU recorded internally.
    #[error(
        "AudioUnit error in {function}: OSStatus {code} ({}){}",
        os_status_message(*.code),
        .last_render_error
            .map_or_else(String::new, |last| format!("; last render error {last}"))
    )]
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
    /// widened the enum enough to push `AuActive::uninitialize`'s
    /// `(AuActive, AuError)` past clippy's `result_large_err` threshold. A preset
    /// diagnostic must not tax the render path's result size.
    #[error("preset file I/O failed for {}: {}", .0.path, .0.message)]
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
    #[error("{} is not a valid .aupreset: {}", .0.path, .0.message)]
    InvalidPreset(Box<PresetFileError>),
    /// A `.aupreset` file is well-formed but belongs to a **different** AU.
    ///
    /// Refused rather than applied, and this is the variant the whole
    /// `aupreset` module exists to produce. Measured on macOS 15.6: an AU
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
    #[error(
        "{} is a preset for {}/{}/{}, but this AU is {}/{}/{}; refusing to apply \
         another plugin's state",
        .0.path,
        .0.file_type,
        .0.file_sub_type,
        .0.file_manufacturer,
        .0.au_type,
        .0.au_sub_type,
        .0.au_manufacturer
    )]
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

// These constructors have 19 call sites between them, and every one is in a
// module `lib.rs` gates on `target_os = "macos"` (`instance`, `offline`,
// `aupreset`) — while `error` itself is declared unconditionally. So off macOS
// they compile with no consumers and read as dead. Scoped to `not(macos)`
// rather than a bare `allow` so the lint still catches real rot where the
// callers exist.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl AuError {
    /// Construct a [`AuError::RenderFailed`] from a failed `AudioUnitRender`
    /// call, optionally enriched with the AU's last-render-error.
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
                AuError::RequiresAsyncInstantiation => {
                    return "component requires asynchronous instantiation (AUv3 with a view)";
                }
                AuError::CfStringAlloc => return "CoreFoundation string allocation failed",
                AuError::InvalidBuffer(_) => return "invalid buffer",
                AuError::SampleRateRejected { .. } => return "sample rate rejected",
                AuError::BlockSizeRejected { .. } => return "block size rejected",
                AuError::PresetIo(_) => return "preset file I/O failed",
                AuError::InvalidPreset(_) => return "not a valid .aupreset",
                AuError::PresetIdentityMismatch(_) => {
                    return "preset belongs to a different Audio Unit";
                }
            };
            os_status_message(code)
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

/// Decode a well-known AudioUnit `OSStatus` into a short description, falling
/// back to `"unknown error"`. Shared by [`AuError::message`] and the `Display`
/// text of [`AuError::OsStatus`] / [`AuError::RenderFailed`] so the two cannot
/// drift.
#[cfg(target_os = "macos")]
fn os_status_message(code: i32) -> &'static str {
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

/// Off macOS the status constants are not compiled in (they live in the
/// macOS-gated `types` module), so every code reads as unknown — matching what
/// [`AuError::message`] reported before the table was shared.
#[cfg(not(target_os = "macos"))]
fn os_status_message(_code: i32) -> &'static str {
    "unknown error"
}
