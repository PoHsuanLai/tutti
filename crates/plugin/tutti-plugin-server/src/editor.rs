//! Whether the hosted plugin's editor window is open.
//!
//! Its own module because the server tracks the window's open/closed state
//! separately from the plugin: the window must be closed on shutdown even when
//! the plugin is being dropped anyway, and a second open request on an
//! already-open editor is a no-op rather than a second window.

/// Open/closed state of the hosted plugin's editor window.
///
/// The server's belief, not the windowing system's — the plugin owns the actual
/// window. They diverge only if a plugin closes its own editor without telling
/// the host, which no supported format does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum EditorState {
    /// No editor window. The state a session starts and ends in.
    #[default]
    Closed,
    /// An editor window is open and parented to the host-supplied handle.
    Open,
}

impl EditorState {
    /// Whether an editor window is currently open.
    pub(crate) fn is_open(self) -> bool {
        matches!(self, EditorState::Open)
    }
}
