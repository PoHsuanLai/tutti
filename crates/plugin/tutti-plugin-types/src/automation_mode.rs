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
//! host→plugin automation-state channel wired today (VST3's global
//! `IAutomationState`). A finer per-parameter model (CLAP indication) would be a
//! separate capability if/when it is wired.

/// What the host is doing with automation, surfaced to the plugin for UI
/// feedback. Global — applies to the whole plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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
