//! What a saved plugin state is for.

/// What a plugin state is saved or loaded for.
///
/// A plugin that distinguishes the two (CLAP's `state-context`) may keep
/// things in a project's state that a copy of the instance must not share.
/// Every other format has one state and ignores the use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum StateUse {
    /// The state of the instance itself, saved with or restored from a project.
    Project,
    /// The state a second instance is made from: a fork of the live one.
    Duplicate,
}
