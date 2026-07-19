//! Host↔server wire contract.
//!
//! Not part of the end-user API — these types exist so `tutti-plugin-server`
//! can build the other end of the IPC. Regular library users should not
//! reach into this module.
//!
//! Contains the [`PluginInstance`](crate::server::PluginInstance) trait
//! (what a loader must implement) and re-exports every wire-frame type the
//! IPC carries.

use crate::Result;

pub use crate::protocol::audio::{AudioBuffer, AudioBuffer32, AudioBuffer64, AudioBufferMut, Sample};
pub use crate::util::config::BridgeConfig;
pub use crate::protocol::{
    AuComponentType, BridgeMessage, BusChannels, ChordChanges, ChordValue, HostMessage,
    IpcMidiEvent, IpcMidiEventVec, LoadedPlugin, MidiEvent, MidiEventVec, NoteExpressionChanges,
    NoteExpressionIntChanges, NoteExpressionIntValue, NoteExpressionTextChanges,
    NoteExpressionTextValue, NoteExpressionType, NoteExpressionValue, ParameterChanges,
    ParameterFlags, ParameterInfo, ParameterPoint, ParameterQueue, PluginClass, PluginDescriptor,
    ProcessAudioData, SampleFormat, ScaleChanges, ScaleValue, SlabLayout, TransportInfo,
    Vst2Category,
};
pub use crate::host::subprocess::resolve_bundle;
pub use crate::util::transport::shm::AudioSlab;
pub use crate::util::window::{EditorSize, WindowHandle};

/// VST3-only sequencer-context inputs (chord / scale / per-note text / int
/// expression). No other format consumes these, so they live in one optional
/// bundle rather than as loose fields on the universal [`ProcessContext`]. The
/// VST3 loader is the only reader; everyone else leaves this `None`.
#[derive(Default)]
pub struct ExpressiveContext<'a> {
    pub chords: Option<&'a ChordChanges>,
    pub scales: Option<&'a ScaleChanges>,
    pub expr_texts: Option<&'a NoteExpressionTextChanges>,
    pub expr_ints: Option<&'a NoteExpressionIntChanges>,
}

/// Per-block inputs to [`PluginInstance::process`] beyond the audio buffer.
#[derive(Default)]
pub struct ProcessContext<'a> {
    pub midi_events: &'a [MidiEvent],
    /// VST3/CLAP only, ignored by VST2.
    pub param_changes: Option<&'a ParameterChanges>,
    /// VST3/CLAP only, ignored by VST2.
    pub note_expression: Option<&'a NoteExpressionChanges>,
    pub transport: Option<&'a TransportInfo>,
    /// VST3-only sequencer context. `None` for VST2/CLAP/AU.
    pub expressive: Option<ExpressiveContext<'a>>,
}

impl<'a> ProcessContext<'a> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn midi(mut self, events: &'a [MidiEvent]) -> Self {
        self.midi_events = events;
        self
    }

    pub fn params(mut self, changes: &'a ParameterChanges) -> Self {
        self.param_changes = Some(changes);
        self
    }

    pub fn note_expression(mut self, changes: &'a NoteExpressionChanges) -> Self {
        self.note_expression = Some(changes);
        self
    }

    pub fn transport(mut self, info: &'a TransportInfo) -> Self {
        self.transport = Some(info);
        self
    }

    pub fn expressive(mut self, ctx: ExpressiveContext<'a>) -> Self {
        self.expressive = Some(ctx);
        self
    }
}

/// Per-block outputs from [`PluginInstance::process`] beyond the audio buffer.
#[derive(Default)]
pub struct ProcessOutput {
    pub midi_events: MidiEventVec,
    pub param_changes: ParameterChanges,
    pub note_expression: NoteExpressionChanges,
}

/// Unified interface for VST2, VST3, CLAP, and AU plugin instances,
/// implemented on the server side of the IPC.
///
/// Static identity (name, vendor, native class, editor) goes through
/// [`descriptor`](Self::descriptor); engine-wiring data (per-bus widths,
/// latency, f64) through [`loaded`](Self::loaded). Both are snapshots of what
/// the plugin reported at load time. The trait's remaining methods are the
/// ones that need a live plugin reference.
pub trait PluginInstance: Send {
    fn descriptor(&self) -> &PluginDescriptor;

    fn loaded(&self) -> &LoadedPlugin;

    /// Process one audio block. The buffer carries the negotiated sample
    /// format (f32 or f64) as a tagged enum, so the trait stays
    /// dyn-compatible while implementations can branch once and delegate
    /// into a single generic inner body.
    fn process(
        &mut self,
        buffer: AudioBufferMut<'_>,
        ctx: &ProcessContext,
    ) -> Result<ProcessOutput>;

    fn set_sample_rate(&mut self, rate: f64);

    /// Parameter value in the format's native convention: normalized 0..1 for
    /// VST2/VST3/AU, but the plugin's **native plain range** for CLAP (CLAP has
    /// no normalization concept). A consumer that needs a normalized value must
    /// scale by the `min_value`/`max_value` carried on [`ParameterInfo`].
    fn get_parameter(&self, id: u32) -> f64;

    /// See [`get_parameter`](Self::get_parameter) for the value convention
    /// (normalized for VST2/VST3/AU, native plain range for CLAP).
    fn set_parameter(&mut self, id: u32, value: f64);

    /// Push the host automation read/write state to the plugin. Fire-and-forget;
    /// the default no-op covers formats without an automation-state concept
    /// (only VST3's `IAutomationState` implements it).
    fn set_automation_state(&mut self, _state: i32) {}

    fn get_parameter_list(&self) -> Vec<ParameterInfo>;

    fn open_editor(&mut self, parent: WindowHandle) -> Result<EditorSize>;

    fn close_editor(&mut self);

    fn get_state(&mut self) -> Result<Vec<u8>>;

    fn set_state(&mut self, data: &[u8]) -> Result<()>;
}
