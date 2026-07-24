//! Format-agnostic plugin error, returned by the [`PluginFormatHost`] trait.
//!
//! This is the *lean* error the shared trait speaks — it carries only the
//! failure modes a format loader can produce (load, state, process, editor),
//! never the IPC/host-coupled variants (`ServerNotFound`, `ProtocolMismatch`,
//! `IpcError`, …) that live on `tutti-plugin`'s `BridgeError`. The host crate
//! maps `PluginError` into its richer `BridgeError` at the IPC boundary.
//!
//! [`PluginFormatHost`]: crate::PluginFormatHost

use crate::editor::EditorError;
use crate::load_stage::LoadStage;

/// Failure produced by a [`PluginFormatHost`](crate::PluginFormatHost) method.
///
/// Deliberately narrow: only the failure modes a format loader can hit. The
/// `tutti-plugin` host crate widens this into its `BridgeError` (which adds
/// the IPC/subprocess variants) via `From<PluginError>`.
#[derive(Debug)]
pub enum PluginError {
    /// A plugin failed to load at a specific phase. `reason` carries the
    /// format-native error text.
    Load { stage: LoadStage, reason: String },
    /// Saving or restoring plugin state failed.
    State(String),
    /// Processing an audio block failed.
    Process(String),
    /// Opening / resizing the plugin editor failed.
    Editor(EditorError),
    /// Any other failure that doesn't fit the categories above.
    Other(String),
}

impl core::fmt::Display for PluginError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Load { stage, reason } => {
                write!(f, "plugin load failed at {stage} stage: {reason}")
            }
            Self::State(msg) => write!(f, "plugin state error: {msg}"),
            Self::Process(msg) => write!(f, "plugin process error: {msg}"),
            Self::Editor(e) => write!(f, "plugin editor error: {e}"),
            Self::Other(msg) => write!(f, "plugin error: {msg}"),
        }
    }
}

impl std::error::Error for PluginError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Editor(e) => Some(e),
            _ => None,
        }
    }
}

impl From<EditorError> for PluginError {
    fn from(e: EditorError) -> Self {
        Self::Editor(e)
    }
}

/// Result alias for [`PluginFormatHost`](crate::PluginFormatHost) methods.
pub type Result<T> = core::result::Result<T, PluginError>;
