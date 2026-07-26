//! MPE mode configuration.
//!
//! [`MpeModeConfig`] is the app-facing MPE setting. The engine build reads it to
//! construct an [`MpeIngest`](tutti_midi_runtime::MpeIngest) — the input-edge
//! transform that rewrites classic-MPE channel-spread into native MIDI-2 per-note
//! messages (per M2-104, MPE zone handling is an *ingestion* concern, not a
//! synthesis one). Downstream synth voices then track per-note expression
//! themselves from the native per-note messages; there is no shared expression
//! table to publish.

/// Configures the MPE mode the engine installs. Insert this *before* the engine
/// builds to override the default. Default is
/// [`MpeMode::Disabled`](tutti_midi_io::MpeMode::Disabled) — apps that want MPE flip this
/// to `LowerZone` / `UpperZone` / `DualZone` / `SingleChannelRotation`. The
/// inspector UI writes it directly (it's a plain Bevy resource).
#[derive(bevy_ecs::resource::Resource, Debug, Clone)]
pub struct MpeModeConfig(pub tutti_midi_io::MpeMode);

impl Default for MpeModeConfig {
    fn default() -> Self {
        Self(tutti_midi_io::MpeMode::Disabled)
    }
}
