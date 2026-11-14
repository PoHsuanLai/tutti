//! Host automation *mode* — format-agnostic vocabulary for what the host is
//! doing with automation on a plugin.
//!
//! The host tells the plugin whether it is reading automation back, actively
//! writing / recording it, or neither, so the plugin can adapt its editor UI
//! (e.g. a knob ring glows while the host records automation onto it). This is a
//! *host → plugin advisory*: the plugin does nothing audible with it.
//!
//! This type is deliberately **format-neutral** — it names the meaningful modes
//! and nothing else. The mapping onto a specific plugin ABI (VST3
//! `IAutomationState`, CLAP `param.set_automation`, …) lives at that format's
//! edge in `tutti-plugin`, not here: `tutti-plugin-types` is the low-level shared
//! vocabulary and must not depend on or bake in any one format's encoding.
//!
//! It is a single global mode (applies to the whole plugin), matching the only
//! host→plugin automation-state channel that exists (VST3's global
//! `IAutomationState`). A finer per-parameter model (CLAP indication) would be a
//! separate capability if/when it is wired.

/// What the host is doing with automation, surfaced to the plugin for UI
/// feedback. Global — applies to the whole plugin.
///
/// Exhaustive: the two flags (read, write) make exactly four modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum AutomationMode {
    /// The host is neither reading nor writing automation.
    #[default]
    Off,
    /// The host is reading automation (playing it back onto the plugin).
    Reading,
    /// The host is writing / recording automation from the plugin.
    Writing,
    /// The host is both reading and writing automation.
    ReadWriting,
}

/// Why [`AnyThread::set_automation_mode`](crate::AnyThread::set_automation_mode)
/// did not reach the plugin. The mode is an advisory, so none of these is a
/// failure of the plugin's audio; they differ in what the caller does next.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum AutomationModeError {
    /// The plugin's hosting carries no automation-state channel (no in-process
    /// format does). Permanent: do not retry.
    #[error("{format} carries no automation-state advisory in this hosting")]
    Unsupported {
        /// The plugin's format.
        format: String,
    },
    /// The push onto the command queue was dropped because the queue is full.
    /// Transient: the next change lands.
    #[error("the automation-state push was dropped: the command queue is full")]
    Dropped,
    /// The plugin is gone. Permanent.
    #[error("the plugin is gone")]
    PluginDead,
}
