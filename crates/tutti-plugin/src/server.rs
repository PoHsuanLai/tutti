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

pub use crate::audio::{AudioBuffer, AudioBuffer32, AudioBuffer64, AudioBufferMut, Sample};
pub use crate::config::BridgeConfig;
pub use crate::protocol::{
    AudioIO, AudioProcessedFullData, AudioProcessedMidiData, BridgeMessage, BusDirection,
    BusLayout, ChordChanges, ChordValue, HostMessage, IpcMidiEvent, IpcMidiEventVec, MidiEvent,
    MidiEventVec, NoteExpressionChanges, NoteExpressionIntChanges, NoteExpressionIntValue,
    NoteExpressionTextChanges, NoteExpressionTextValue, NoteExpressionType, NoteExpressionValue,
    ParameterChanges, ParameterFlags, ParameterInfo, ParameterPoint, ParameterQueue, PluginInfo,
    ProcessAudioFullData, ProcessAudioMidiData, SampleFormat, ScaleChanges, ScaleValue,
    SlabLayout, TransportInfo,
};
pub use crate::subprocess::resolve_bundle;
pub use crate::transport::shm::AudioSlab;
pub use crate::window::{EditorSize, WindowHandle};

/// Per-block inputs to [`PluginInstance::process`] beyond the audio buffer.
#[derive(Default)]
pub struct ProcessContext<'a> {
    pub midi_events: &'a [MidiEvent],
    /// VST3/CLAP only, ignored by VST2.
    pub param_changes: Option<&'a ParameterChanges>,
    /// VST3/CLAP only, ignored by VST2.
    pub note_expression: Option<&'a NoteExpressionChanges>,
    /// VST3-only sequencer-context inputs (chord / scale / per-note text / int
    /// expression). Ignored by VST2/CLAP/AU.
    pub chords: Option<&'a ChordChanges>,
    pub scales: Option<&'a ScaleChanges>,
    pub expr_texts: Option<&'a NoteExpressionTextChanges>,
    pub expr_ints: Option<&'a NoteExpressionIntChanges>,
    pub transport: Option<&'a TransportInfo>,
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

    pub fn chords(mut self, changes: &'a ChordChanges) -> Self {
        self.chords = Some(changes);
        self
    }

    pub fn scales(mut self, changes: &'a ScaleChanges) -> Self {
        self.scales = Some(changes);
        self
    }

    pub fn expr_texts(mut self, changes: &'a NoteExpressionTextChanges) -> Self {
        self.expr_texts = Some(changes);
        self
    }

    pub fn expr_ints(mut self, changes: &'a NoteExpressionIntChanges) -> Self {
        self.expr_ints = Some(changes);
        self
    }

    pub fn transport(mut self, info: &'a TransportInfo) -> Self {
        self.transport = Some(info);
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
/// Static capability queries (`has_editor`, `supports_f64`, etc.) go
/// through [`metadata`](Self::metadata) — one authoritative source for
/// what the plugin reported at load time. The trait's remaining methods
/// are the ones that need a live plugin reference.
pub trait PluginInstance: Send {
    fn metadata(&self) -> &PluginInfo;

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

    /// Normalized 0..1.
    fn get_parameter(&self, id: u32) -> f64;

    /// Normalized 0..1.
    fn set_parameter(&mut self, id: u32, value: f64);

    fn get_parameter_list(&mut self) -> Vec<ParameterInfo>;

    fn get_parameter_info(&mut self, id: u32) -> Option<ParameterInfo>;

    fn open_editor(&mut self, parent: WindowHandle) -> Result<EditorSize>;

    fn close_editor(&mut self);

    fn editor_idle(&mut self);

    fn get_state(&mut self) -> Result<Vec<u8>>;

    fn set_state(&mut self, data: &[u8]) -> Result<()>;
}
