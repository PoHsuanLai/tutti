//! What the engine wants from a plugin, as one capability flag set.
//!
//! Modeled on wgpu's `Features`: a single [`bitflags`] set, read in both
//! directions. The host checks the negotiated flags to adapt (does this plugin
//! take MIDI? does it have an editor?); the engine checks the [`Features::CONSUMES`]
//! mask to gate per-block side-band sends (transport, automation, note
//! expression, sequencer context). One bit means both "the plugin can consume
//! this" and "so send it" — there is no plugin that can consume a thing yet
//! should not be sent it, so capability and want collapse to one bit.
//!
//! Numeric wiring (bus widths, latency samples) is NOT here — that is the
//! `Limits` half, and lives on [`crate::LoadedPlugin`] as plain fields. Only
//! flags that are *not* recoverable from those numbers are stored here;
//! `multi_bus` / `latency` stay derived methods on `LoadedPlugin`.
//!
//! The `Serialize`/`Deserialize` derives are gated behind the `serde` feature
//! (the IPC wire path enables it), and serialize as the underlying bits.

use bitflags::bitflags;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

bitflags! {
    /// The capabilities a loaded plugin reports. Filled at load time by the
    /// per-format loader (live-probed where the format exposes a query).
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
    #[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
    pub struct Features: u16 {
        // --- Audio (negotiated) ---
        /// Plugin negotiated 64-bit sample processing at activation.
        /// (Was `LoadedPlugin::supports_f64`.)
        const F64_AUDIO = 1 << 0;

        // --- MIDI (negotiated) ---
        /// Plugin consumes MIDI / note input.
        const MIDI_IN = 1 << 1;
        /// Plugin emits MIDI the host reads back.
        const MIDI_OUT = 1 << 2;

        // --- Editor (negotiated) ---
        /// Plugin has an editor / GUI. (Also kept on `PluginDescriptor` for
        /// the persisted, browse-before-load path; this is the post-load
        /// authoritative copy.)
        const EDITOR = 1 << 3;
        /// Editor supports host-driven resize.
        const EDITOR_RESIZE = 1 << 4;

        // --- Consumes (best-effort — the per-block send-gate set) ---
        /// Plugin wants transport / tempo / playhead each block.
        const TRANSPORT = 1 << 5;
        /// Plugin wants sample-accurate parameter automation curves
        /// (vs. only coarse `set_parameter`).
        const PARAM_AUTOMATION = 1 << 6;
        /// Plugin wants MPE / per-note expression events.
        const NOTE_EXPRESSION = 1 << 7;
        /// Plugin wants the sequencer-context bundle (chords / scales /
        /// per-note text+int). Audience of one format (VST3), by spec.
        const SEQUENCER_CONTEXT = 1 << 8;
    }
}

impl Features {
    /// The best-effort set the engine gates per-block sends on. Making the
    /// bucket a named mask (rather than a doc comment on scattered fields) is
    /// the anti-hybrid guarantee: the send path reads
    /// `features.intersection(Features::CONSUMES)`, never a format name.
    pub const CONSUMES: Features = Features::TRANSPORT
        .union(Features::PARAM_AUTOMATION)
        .union(Features::NOTE_EXPRESSION)
        .union(Features::SEQUENCER_CONTEXT);
}
