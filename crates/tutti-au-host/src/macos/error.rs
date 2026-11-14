//! What only the macOS build constructs or converts from: `AuError`'s
//! constructors over AudioToolbox failures and the `tutti-au-ffi` conversions.

use crate::error::{AuError, LoadFailure, LoadStage, PresetFileFailure};

impl AuError {
    /// Constructs a [`AuError::RenderFailed`] from a failed `AudioUnitRender`
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

    /// Constructs an [`AuError::LoadFailed`] for `component`.
    pub(crate) fn load_failed(
        component: impl Into<String>,
        stage: LoadStage,
        reason: impl Into<String>,
    ) -> Self {
        AuError::LoadFailed(Box::new(LoadFailure {
            component: component.into(),
            stage,
            reason: reason.into(),
        }))
    }

    /// Constructs an [`AuError::InvalidPreset`] for `path`.
    ///
    /// A helper rather than an inline `Box::new(PresetFileFailure { .. })` at each of
    /// the eight construction sites, matching what
    /// [`AuError::render_failed`](Self::render_failed) does for the render path.
    pub(crate) fn invalid_preset(path: impl Into<String>, message: impl Into<String>) -> Self {
        AuError::InvalidPreset(Box::new(PresetFileFailure {
            path: path.into(),
            message: message.into(),
        }))
    }

    /// Constructs an [`AuError::PresetIo`] for `path`.
    pub(crate) fn preset_io(path: impl Into<String>, message: impl Into<String>) -> Self {
        AuError::PresetIo(Box::new(PresetFileFailure {
            path: path.into(),
            message: message.into(),
        }))
    }
}

impl From<tutti_au_ffi::OsError> for AuError {
    fn from(e: tutti_au_ffi::OsError) -> Self {
        AuError::OsStatus {
            function: e.function,
            code: e.status,
        }
    }
}

impl From<tutti_au_ffi::Error> for AuError {
    fn from(e: tutti_au_ffi::Error) -> Self {
        match e {
            tutti_au_ffi::Error::Os(e) => e.into(),
            tutti_au_ffi::Error::Invalid(why) => AuError::InvalidBuffer(why),
            tutti_au_ffi::Error::RenderFailed {
                function,
                status,
                last,
            } => AuError::render_failed(function, status, last),
            other => AuError::InvalidBuffer(other.to_string()),
        }
    }
}
