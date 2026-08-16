//! Public types exposed by the VST2 host.
//!
//! Shared value types come from `tutti_plugin_types`; this module owns the
//! VST2-specific shapes (`PluginInfo` with `unique_id`-derived id,
//! `ParameterInfo` with normalized-only values, `ProcessContext`).

pub use tutti_plugin_types::Samples;

pub use tutti_plugin_types::{
    ChannelLayout, EditorSize, MidiEvent, PluginTail, TimeSignature, TransportInfo, WindowHandle,
};

/// Plugin metadata gathered at load time.
#[derive(Debug, Clone, Default)]
pub struct PluginInfo {
    /// Stable identifier built from the plugin's `unique_id`.
    pub id: String,
    /// Display name from `effGetEffectName`, or the file stem if the plugin
    /// declares none.
    pub name: String,
    /// Vendor string from `effGetVendorString`; empty if the plugin declines.
    pub vendor: String,
    /// Version as the plugin reports it, rendered as a string. VST2 carries a
    /// bare `i32` with no encoding agreed across vendors, so this is for display
    /// rather than for comparison.
    pub version: String,
    /// Declared audio input width. Zero for an instrument.
    pub num_inputs: ChannelLayout,
    /// Declared audio output width.
    pub num_outputs: ChannelLayout,
    /// The plugin's declared VST2 category, carried verbatim. Callers classify
    /// it themselves rather than relying on the derived `receives_midi` flag.
    pub category: Vst2Category,
    /// `true` if the plugin is a synth or declares MIDI input/output.
    pub receives_midi: bool,
    /// `true` if the plugin declares at least one MIDI **output** bus
    /// (`get_info().midi_outputs > 0`) — it emits MIDI the host reads back.
    pub emits_midi: bool,
    /// `true` if the plugin declared `effFlagsHasEditor`. A `false` here means
    /// [`crate::Vst2Instance`]'s editor calls have nothing to open.
    pub has_editor: bool,
    /// Reported initial latency, in samples.
    ///
    /// `AEffect::initial_delay` is an `i32`; a negative one is not a latency,
    /// so it is clamped to zero at the load site rather than carried.
    pub latency_samples: Samples,
    /// `true` if the plugin declared `effFlagsCanDoubleReplacing`.
    ///
    /// Load-bearing, not informational: `process_f64` reads it to choose
    /// between `processReplacingF64` and the narrowing f32 fallback.
    pub supports_f64: bool,
    /// Ring-out after input stops, decoded from `effGetTailSize`.
    ///
    /// **VST2 encodes this inversely to every other format**, which is why the
    /// decode lives here rather than in [`PluginTail::from_samples`]: on the
    /// wire `0` means "no tail *information*, host decides" and `1` means "no
    /// tail at all". `from_samples` maps `0 => None`, so feeding it a VST2
    /// answer reads "unknown" as "silent" — and a bounce sizing its render from
    /// that adds no decay, truncating every reverb.
    pub tail: PluginTail,
}

/// The plugin's declared VST2 category. Canonical definition lives in
/// `tutti-plugin-types` (shared with the catalog/wire layer); this crate maps
/// the native `vst::Category` into it via `From` (see `instance.rs`).
pub use tutti_plugin_types::Vst2Category;

/// Single VST2 parameter descriptor.
///
/// VST2 doesn't expose min/max/step metadata, so values are always
/// normalized in `[0.0, 1.0]`. The `unit` string is whatever the plugin
/// returns from `getParameterLabel` (typically "Hz", "dB", "%", or empty).
#[derive(Debug, Clone)]
pub struct ParameterInfo {
    /// Dense index in `[0, numParams)` — VST2 addresses parameters by
    /// position, and the ABI's own `i32` is carried rather than re-signed.
    pub id: i32,
    /// Display name from `effGetParamName`.
    pub name: String,
    /// The plugin's own unit label from `effGetParamLabel` — typically `"Hz"`,
    /// `"dB"`, `"%"`, or empty. A free-form string, not a parsed unit type: VST2
    /// makes no promise about its contents, so it cannot be mapped onto the
    /// engine's `Hz` / `Db` vocabulary without guessing.
    pub unit: String,
    /// Current normalized value in `[0.0, 1.0]`. Reads `0.0` for a plugin
    /// exposing no `getParameter` — see [`crate::Vst2Instance::parameter`],
    /// which distinguishes the two.
    pub current: f32,
}

/// Stack-allocated event collection. Inline storage matches
/// `tutti-plugin`'s `MidiEventVec` so shim layers can pass the value
/// straight through without re-allocating.
pub type MidiEventVec = smallvec::SmallVec<[MidiEvent; 256]>;

/// Per-block inputs to `Vst2Instance::process_f32`/`process_f64` beyond
/// the audio buffer.
#[derive(Default)]
pub struct ProcessContext<'a> {
    /// Events to deliver before the block renders, each carrying its own frame
    /// offset within the block. Empty is normal for an effect.
    pub midi: &'a [MidiEvent],
    /// Transport state for this block. `None` leaves the plugin's `VstTimeInfo`
    /// reporting a stopped transport, which is what a plugin syncing to host
    /// tempo will see.
    pub transport: Option<&'a TransportInfo>,
    /// Sample rate in Hz.
    ///
    /// A bare `f64`, not the engine's `SampleRate`: this value reaches
    /// `effSetSampleRate`, a C ABI taking a float. The unit types stop at that
    /// boundary by design.
    pub sample_rate: f64,
}

impl<'a> ProcessContext<'a> {
    /// A context with no MIDI and no transport, carrying only `sample_rate` in
    /// Hz. Layer the rest on with [`Self::midi`] and [`Self::transport`].
    pub fn new(sample_rate: f64) -> Self {
        Self {
            midi: &[],
            transport: None,
            sample_rate,
        }
    }

    /// Attaches this block's MIDI events, replacing any already set.
    pub fn midi(mut self, midi: &'a [MidiEvent]) -> Self {
        self.midi = midi;
        self
    }

    /// Attaches this block's transport state, replacing any already set.
    pub fn transport(mut self, transport: &'a TransportInfo) -> Self {
        self.transport = Some(transport);
        self
    }
}
