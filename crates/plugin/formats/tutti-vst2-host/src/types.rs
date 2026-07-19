//! Public types exposed by the VST2 host.
//!
//! Shared value types come from `tutti_plugin_types`; this module owns the
//! VST2-specific shapes (`PluginInfo` with `unique_id`-derived id,
//! `ParameterInfo` with normalized-only values, `ProcessContext`).

pub use tutti_plugin_types::{EditorSize, MidiEvent, TransportInfo, WindowHandle};

/// Plugin metadata gathered at load time.
#[derive(Debug, Clone, Default)]
pub struct PluginInfo {
    /// Stable identifier built from the plugin's `unique_id`.
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub version: String,
    pub num_inputs: usize,
    pub num_outputs: usize,
    /// The plugin's declared VST2 category, carried verbatim. Callers classify
    /// it themselves rather than relying on the derived `receives_midi` flag.
    pub category: Vst2Category,
    /// `true` if the plugin is a synth or declares MIDI input/output.
    pub receives_midi: bool,
    pub has_editor: bool,
    /// Reported initial latency, in samples.
    pub latency_samples: usize,
    /// `true` if the plugin advertised f64 precision support — the `vst`
    /// crate processes f32 only regardless, so this is informational.
    pub supports_f64: bool,
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
    pub id: u32,
    pub name: String,
    pub unit: String,
    /// Current normalized value in `[0.0, 1.0]`.
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
    pub midi: &'a [MidiEvent],
    pub transport: Option<&'a TransportInfo>,
    pub sample_rate: f64,
}

impl<'a> ProcessContext<'a> {
    pub fn new(sample_rate: f64) -> Self {
        Self {
            midi: &[],
            transport: None,
            sample_rate,
        }
    }

    pub fn midi(mut self, midi: &'a [MidiEvent]) -> Self {
        self.midi = midi;
        self
    }

    pub fn transport(mut self, transport: &'a TransportInfo) -> Self {
        self.transport = Some(transport);
        self
    }
}
