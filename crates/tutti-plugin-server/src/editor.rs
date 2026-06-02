//! Editor window state.

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum EditorState {
    #[default]
    Closed,
    Open,
}

impl EditorState {
    pub(crate) fn is_open(self) -> bool {
        matches!(self, EditorState::Open)
    }
}
