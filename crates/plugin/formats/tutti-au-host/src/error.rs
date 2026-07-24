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
        }
    }
}

impl std::error::Error for AuError {}
