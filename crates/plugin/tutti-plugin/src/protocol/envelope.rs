//! Wire-envelope enums — the host↔server message types.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use super::midi::IpcMidiEventVec;
use super::process::ProcessAudioData;
use super::sample::SampleFormat;
use super::shm::SlabLayout;
use super::Normalized;
use super::ParamAddress;
use super::Samples;
use super::{LoadedPlugin, ParameterInfo, PluginDescriptor, PluginTail, Preset, PresetId};

/// Wire-deserialization fallback for [`HostMessage::LoadPlugin::block_size`]
/// when an older/partial message arrives without the field. The operative
/// value at runtime comes from `config.max_buffer_size` (see
/// `host::subprocess::launch`); this is only a safety net for legacy messages.
pub(crate) const DEFAULT_BLOCK_SIZE: usize = 512;

fn default_block_size() -> usize {
    DEFAULT_BLOCK_SIZE
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HostMessage {
    /// Lightweight metadata-only probe — reads factory/descriptor info
    /// without activating the plugin (avoids license dialogs).
    ProbePlugin {
        path: PathBuf,
    },
    LoadPlugin {
        path: PathBuf,
        sample_rate: f64,
        #[serde(default = "default_block_size")]
        block_size: usize,
        #[serde(default)]
        preferred_format: SampleFormat,
        #[serde(default)]
        shm_name: String,
    },
    UnloadPlugin,
    /// Process one audio block. Audio rides the shared `AudioSlab`, in the ring
    /// slot `seq` selects; the boxed payload carries the per-block side-band.
    ProcessAudio(Box<ProcessAudioData>),
    SetParameter {
        param_id: ParamAddress,
        value: f32,
    },
    SetAutomationState {
        /// Format-neutral automation mode; the format loader encodes it onto its
        /// own ABI server-side. See [`AutomationMode`](crate::protocol::AutomationMode).
        mode: crate::protocol::AutomationMode,
    },
    GetParameter {
        param_id: ParamAddress,
    },
    GetParameterList,
    GetParameterInfo {
        param_id: ParamAddress,
    },
    SetSampleRate {
        rate: f64,
    },
    Reset,
    SaveState,
    LoadState {
        data: Vec<u8>,
    },
    OpenEditor {
        parent_handle: u64,
    },
    CloseEditor,
    SetupSharedMemory {
        shm_name: String,
        layout: SlabLayout,
    },
    Shutdown,
    /// Tell the plugin whether it is rendering under realtime pressure.
    ///
    /// A sibling of [`SetSampleRate`](Self::SetSampleRate) rather than a field
    /// on [`ProcessAudio`](Self::ProcessAudio): three of the four formats can
    /// only accept this while the plugin is deactivated, and two of those
    /// rebuild buffers around it. Sending it per block would be both wasteful
    /// and unrepresentable.
    ///
    /// Appended rather than placed beside `SetSampleRate` because bincode
    /// encodes the discriminant over declaration order, so inserting mid-enum
    /// renumbers every later variant.
    SetRenderMode {
        mode: crate::protocol::RenderMode,
    },
    /// Ask for the plugin's preset list.
    ///
    /// Appended, like every variant since v11, because bincode encodes the
    /// discriminant over declaration order.
    GetPresetList,
    /// Ask the plugin to load one preset, by an id its list produced.
    ///
    /// [`PresetId`](crate::protocol::PresetId) is opaque and format-shaped —
    /// never construct one host-side. Three of the four formats number presets
    /// in a space that is not a position in the list, so an invented id loads
    /// the wrong preset rather than failing.
    LoadPreset {
        id: crate::protocol::PresetId,
    },
    /// Ask which preset the plugin considers current.
    GetCurrentPreset,
    /// Ask the plugin to render `value` as the text it would display for it.
    ///
    /// `value` is normalized, as everywhere on this wire; the loader converts
    /// to its format's domain.
    GetParameterText {
        param_id: crate::protocol::ParamAddress,
        value: crate::protocol::Normalized,
    },
    /// Ask the plugin to parse `text` into a value.
    ///
    /// Note this is not a pure query for VST2, whose `effString2Parameter`
    /// applies the parsed value as it reads it — see that loader's
    /// `parameter_value_from_text`.
    GetParameterValueFromText {
        param_id: crate::protocol::ParamAddress,
        text: String,
    },
}

// `AudioProcessed` carries an inline-256 `IpcMidiEventVec` (~5 KB), dwarfing the
// other variants. This is deliberate: the inline capacity keeps the common
// (0–handful of events) case heap-free. `BridgeMessage` is only ever
// (de)serialized on the off-RT bridge thread, so its stack size is not an RT
// concern, and boxing would just add an off-RT allocation.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BridgeMessage {
    PluginLoaded {
        /// Catalog identity (id, name, vendor, version, native class, editor).
        /// The probe path uses only this half; `loaded` is defaulted for a
        /// metadata-only probe that never activated the plugin.
        descriptor: Box<PluginDescriptor>,
        /// Engine-wiring data from instantiation (bus widths, latency, f64).
        /// Empty/default for a `ProbePlugin` reply.
        #[serde(default)]
        loaded: LoadedPlugin,
        negotiated_format: SampleFormat,
    },
    PluginUnloaded,
    /// Acknowledges a processed block. Audio output goes into the block's slot
    /// in the shared `AudioSlab`'s output ring, published there before this
    /// message is sent; the measured latency and any MIDI the plugin emitted
    /// travel here. (Parameter / note-expression output is still not routed
    /// back.) `midi_out` is capped at `MIDI_STACK_CAPACITY` server-side so it
    /// stays inline (no heap on the RT-adjacent path).
    ///
    /// **This message is not what makes the audio readable.** It used to be: the
    /// echoed `buffer_id` was the only evidence a reply belonged to a given
    /// block, because the slab carried no generation counter and reported a
    /// full-length success whether or not anyone had written the region. The
    /// slab now answers that itself, per slot, so the host would emit silence for
    /// an unpublished block even if this arrived for it. The echoed `seq` is kept
    /// for diagnostics and ordering.
    AudioProcessed {
        latency_us: u64,
        /// Echo of the request's [`ProcessAudioData::seq`].
        #[serde(default)]
        seq: u64,
        #[serde(default)]
        midi_out: IpcMidiEventVec,
    },
    ParameterValue {
        value: Option<f32>,
    },
    ParameterList {
        parameters: Vec<ParameterInfo>,
    },
    ParameterInfoResponse {
        info: Option<ParameterInfo>,
    },
    StateData {
        data: Vec<u8>,
    },
    EditorOpened {
        width: u32,
        height: u32,
    },
    EditorClosed,
    ParameterChanged {
        index: i32,
        value: f32,
    },
    /// Plugin reported a latency change at runtime. Host updates the
    /// corresponding `PluginClient::set_latency` so `AudioUnit::latency()`
    /// reports the new value. Note: does NOT trigger PDC re-analysis;
    /// the graph must be committed again for compensation to update.
    LatencyChanged {
        samples: Samples,
    },
    /// Plugin reported a new tail length at runtime. The host updates the value
    /// `AudioUnit::tail()` reports, so a bounce started after the change sizes
    /// its render from the current decay rather than the one loaded with.
    ///
    /// CLAP-only in practice: it is the one format that pairs its tail query
    /// with a host `changed` callback. VST3's restart flags have no tail member
    /// and AU has no tail property listener, so both are settled at load.
    TailChanged {
        tail: PluginTail,
    },
    /// Plugin changed its own parameter values at runtime (e.g. an in-plugin
    /// preset load). The host should re-read parameter values from the plugin.
    PluginParamValuesChanged,
    /// Plugin changed parameter titles/units/flags. The host should re-pull the
    /// parameter list.
    PluginParamTitlesChanged,
    /// Plugin's bus arrangement changed and was re-enumerated server-side. The
    /// host should rewire its audio graph from the plugin's refreshed metadata.
    PluginIoChanged,
    /// Plugin was torn down and rebuilt in place at the plugin's request
    /// (`kReloadComponent`). The host should resync all plugin state — it is
    /// effectively a fresh instance.
    PluginReloaded,
    /// The plugin's preset list.
    ///
    /// Empty when the format cannot enumerate — CLAP, whose discovery is a
    /// factory-level extension this host does not bind. That is **not** the
    /// same as "this plugin has no presets"; a caller separates the two by
    /// reading `Features::PRESET_LIST`, which is unprobed for CLAP and
    /// `Some(false)` for a format that was asked and declined.
    PresetList {
        presets: Vec<Preset>,
    },
    /// Whether the plugin accepted a [`LoadPreset`](HostMessage::LoadPreset).
    ///
    /// `false` is a refusal rather than an error, and is also the honest
    /// answer for VST3: its programs are selected through the parameter path,
    /// so there is no direct load to report on.
    PresetLoaded {
        ok: bool,
    },
    /// The preset the plugin considers current, if it will say. `None` means
    /// the format has no query (VST3, CLAP) or the plugin declined — never
    /// "the first one".
    CurrentPreset {
        id: Option<PresetId>,
    },
    SharedMemoryReady,
    Error {
        message: String,
    },
    /// Handshake sent once the subprocess is live. Carries the wire
    /// [`PROTOCOL_VERSION`](super::PROTOCOL_VERSION) so a host and subprocess
    /// built from mismatched commits refuse to proceed rather than mis-parsing
    /// each other's messages (bincode is not self-describing). `#[serde(default)]`
    /// = 0 for an ancient binary that predates the field → treated as a mismatch.
    Ready {
        #[serde(default)]
        protocol_version: u32,
    },
    Shutdown,
    /// The plugin's display string for the value that was asked about. `None`
    /// means the plugin did not answer — the caller renders the raw number
    /// rather than treating this as an empty label.
    ParameterText {
        text: Option<String>,
    },
    /// The value the plugin parsed a string into, normalized. `None` means it
    /// could not parse it, and the caller must leave its field unchanged.
    ParameterValueFromText {
        value: Option<Normalized>,
    },
}
