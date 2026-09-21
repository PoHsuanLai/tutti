//! Wire-envelope enums — the host↔server message types.
//!
//! [`HostMessage`] travels host → subprocess, [`BridgeMessage`] back. Both are
//! bincode-encoded, so **variant order is wire-significant**: a discriminant is a
//! varint over declaration order, and inserting mid-enum renumbers every later
//! variant. Append, and bump
//! [`PROTOCOL_VERSION`](super::PROTOCOL_VERSION).

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

/// A request travelling host → subprocess.
///
/// Variant order is wire-significant; see the module docs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HostMessage {
    /// Lightweight metadata-only probe — reads factory/descriptor info
    /// without activating the plugin (avoids license dialogs).
    ProbePlugin {
        /// Filesystem path to the plugin bundle or shared library.
        path: PathBuf,
    },
    /// Instantiate and activate the plugin, ready to process audio.
    LoadPlugin {
        /// Filesystem path to the plugin bundle or shared library.
        path: PathBuf,
        /// Sample rate in Hz, as `f64` because plugin ABIs denominate it so.
        sample_rate: f64,
        /// Maximum frames per [`ProcessAudio`](Self::ProcessAudio) block. The
        /// plugin sizes its internal buffers from this, so a later block may not
        /// exceed it.
        #[serde(default = "default_block_size")]
        block_size: usize,
        /// Sample format the host would rather exchange; the subprocess replies
        /// with what it actually negotiated.
        #[serde(default)]
        preferred_format: SampleFormat,
        /// Name of the shared-memory audio slab, empty if not yet set up.
        #[serde(default)]
        shm_name: String,
    },
    /// Deactivate and destroy the plugin instance, keeping the subprocess alive.
    UnloadPlugin,
    /// Process one audio block. Audio rides the shared `AudioSlab`, in the ring
    /// slot `seq` selects; the boxed payload carries the per-block side-band.
    ProcessAudio(Box<ProcessAudioData>),
    /// Set one parameter to a normalized value.
    SetParameter {
        /// Which parameter — opaque handle or positional index, per format.
        param_id: ParamAddress,
        /// Normalized `0.0..=1.0`; the loader maps it into the plugin's domain.
        value: f32,
    },
    /// Enter or leave an automation gesture, so the plugin can record it.
    SetAutomationState {
        /// Format-neutral automation mode; the format loader encodes it onto its
        /// own ABI server-side. See [`AutomationMode`](crate::protocol::AutomationMode).
        mode: crate::protocol::AutomationMode,
    },
    /// Read one parameter's current normalized value.
    GetParameter {
        /// Which parameter — opaque handle or positional index, per format.
        param_id: ParamAddress,
    },
    /// Ask for every parameter the plugin declares.
    GetParameterList,
    /// Ask for one parameter's declaration (name, range, flags, group).
    GetParameterInfo {
        /// Which parameter — opaque handle or positional index, per format.
        param_id: ParamAddress,
    },
    /// Re-rate the plugin. Deactivates and reactivates it server-side.
    SetSampleRate {
        /// Sample rate in Hz, as `f64` because plugin ABIs denominate it so.
        rate: f64,
    },
    /// Clear the plugin's internal state — tails, delay lines, voices.
    Reset,
    /// Ask the plugin to serialize its state, answered by
    /// `BridgeMessage::StateData`.
    SaveState,
    /// One slice of a plugin state being restored, in order.
    ///
    /// State is the only message whose size a plugin chooses, and it routinely
    /// exceeds what a single frame may carry — see [`MAX_STATE_BYTES`]. So it
    /// travels as a sequence rather than one `LoadState`, and the receiver
    /// reassembles. Every frame stays under [`MAX_FRAME_BYTES`], so the
    /// allocation and deadline bounds on the wire are untouched by a large
    /// state.
    ///
    /// `seq` is checked, not trusted: a gap or a repeat means the sequence is
    /// not what the sender thinks it is, and silently reassembling it would
    /// hand the plugin a corrupt blob it would then try to parse.
    ///
    /// [`MAX_STATE_BYTES`]: crate::protocol::MAX_STATE_BYTES
    /// [`MAX_FRAME_BYTES`]: crate::protocol::MAX_FRAME_BYTES
    LoadStateChunk {
        /// Position in the sequence, starting at 0 and incrementing by one.
        seq: u32,
        /// Whether this is the final chunk. A one-chunk state sets it on `seq` 0.
        last: bool,
        /// Opaque, format-defined bytes. Never interpret them host-side.
        bytes: Vec<u8>,
    },
    /// Open the plugin's editor window.
    OpenEditor {
        /// Native parent window handle, cast to `u64` for the wire — an `NSView*`
        /// on macOS, an `HWND` on Windows, an X11 window id on Linux.
        parent_handle: u64,
    },
    /// Close the plugin's editor window.
    CloseEditor,
    /// Point the subprocess at the shared-memory audio slab.
    SetupSharedMemory {
        /// OS name of the shared-memory object to map.
        shm_name: String,
        /// Region geometry, which both sides must agree on exactly.
        layout: SlabLayout,
    },
    /// Ask the subprocess to exit cleanly.
    Shutdown,
    /// Tell the plugin whether it is rendering under realtime pressure.
    ///
    /// A sibling of [`SetSampleRate`](Self::SetSampleRate) rather than a field
    /// on [`ProcessAudio`](Self::ProcessAudio): three of the four formats can
    /// only accept this while the plugin is deactivated, and two of those
    /// rebuild buffers around it. Sending it per block would be both wasteful
    /// and unrepresentable.
    SetRenderMode {
        /// Realtime or offline.
        mode: crate::protocol::RenderMode,
    },
    /// Ask for the plugin's preset list.
    GetPresetList,
    /// Ask the plugin to load one preset, by an id its list produced.
    ///
    /// [`PresetId`](crate::protocol::PresetId) is opaque and format-shaped —
    /// never construct one host-side. Three of the four formats number presets
    /// in a space that is not a position in the list, so an invented id loads
    /// the wrong preset rather than failing.
    LoadPreset {
        /// Opaque, format-shaped preset id taken from the plugin's own list.
        id: crate::protocol::PresetId,
    },
    /// Ask which preset the plugin considers current.
    GetCurrentPreset,
    /// Ask the plugin to render `value` as the text it would display for it.
    ///
    /// `value` is normalized, as everywhere on this wire; the loader converts
    /// to its format's domain.
    GetParameterText {
        /// Which parameter — opaque handle or positional index, per format.
        param_id: crate::protocol::ParamAddress,
        /// The value to render, normalized.
        value: crate::protocol::Normalized,
    },
    /// Ask the plugin to parse `text` into a value.
    ///
    /// Note this is not a pure query for VST2, whose `effString2Parameter`
    /// applies the parsed value as it reads it — see that loader's
    /// `parameter_value_from_text`.
    GetParameterValueFromText {
        /// Which parameter — opaque handle or positional index, per format.
        param_id: crate::protocol::ParamAddress,
        /// Display text to parse, in whatever form the plugin renders.
        text: String,
    },
}

// `AudioProcessed` carries an inline-256 `IpcMidiEventVec` (~5 KB), dwarfing the
// other variants. This is deliberate: the inline capacity keeps the common
// (0–handful of events) case heap-free. `BridgeMessage` is only ever
// (de)serialized on the off-RT bridge thread, so its stack size is not an RT
// concern, and boxing would just add an off-RT allocation.
/// A reply or notification travelling subprocess → host.
///
/// Variant order is wire-significant; see the module docs.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BridgeMessage {
    /// The plugin was probed or loaded successfully.
    PluginLoaded {
        /// Catalog identity (id, name, vendor, version, native class, editor).
        /// The probe path uses only this half; `loaded` is defaulted for a
        /// metadata-only probe that never activated the plugin.
        descriptor: Box<PluginDescriptor>,
        /// Engine-wiring data from instantiation (bus widths, latency, f64).
        /// Empty/default for a `ProbePlugin` reply.
        #[serde(default)]
        loaded: LoadedPlugin,
        /// The sample format actually agreed on, which may differ from the
        /// host's preference.
        negotiated_format: SampleFormat,
    },
    /// The plugin instance was destroyed; the subprocess remains alive.
    PluginUnloaded,
    /// Acknowledges a processed block. Audio output goes into the block's slot
    /// in the shared `AudioSlab`'s output ring, published there before this
    /// message is sent; the measured latency and any MIDI the plugin emitted
    /// travel here. (Parameter / note-expression output is not routed back.)
    /// `midi_out` is capped at `MIDI_STACK_CAPACITY` server-side so it stays
    /// inline (no heap on the RT-adjacent path).
    ///
    /// **This message is not what makes the audio readable.** The slab carries a
    /// per-slot generation counter and answers that itself, so the host emits
    /// silence for an unpublished block even if this arrives for it. The echoed
    /// `seq` is for diagnostics and ordering.
    AudioProcessed {
        /// Wall-clock time the plugin spent in `process`, in microseconds.
        latency_us: u64,
        /// Echo of the request's [`ProcessAudioData::seq`].
        #[serde(default)]
        seq: u64,
        /// MIDI the plugin emitted during the block.
        #[serde(default)]
        midi_out: IpcMidiEventVec,
    },
    /// One parameter's normalized value, or `None` if the plugin declined.
    ParameterValue {
        /// Normalized `0.0..=1.0`.
        value: Option<f32>,
    },
    /// Every parameter the plugin declares, in the plugin's own order.
    ParameterList {
        /// The declarations, empty if the plugin exposes no parameters.
        parameters: Vec<ParameterInfo>,
    },
    /// One parameter's declaration, or `None` if the address is unknown.
    ParameterInfoResponse {
        /// The declaration, if the plugin recognized the address.
        info: Option<ParameterInfo>,
    },
    /// One slice of the plugin's serialized state, answering
    /// [`HostMessage::SaveState`], in order.
    ///
    /// The mirror of [`HostMessage::LoadStateChunk`] and chunked for the same
    /// reason: a sample-embedding instrument's state does not fit one frame.
    /// The host reassembles against [`MAX_STATE_BYTES`].
    ///
    /// An **empty** state — a plugin that declines, or no plugin loaded — is
    /// one chunk with `last: true` and no bytes, not an absent reply. A caller
    /// waiting on a sequence must always see it terminate.
    ///
    /// [`MAX_STATE_BYTES`]: crate::protocol::MAX_STATE_BYTES
    StateChunk {
        /// Position in the sequence, starting at 0 and incrementing by one.
        seq: u32,
        /// Whether this is the final chunk.
        last: bool,
        /// Opaque, format-defined bytes. Never interpret them host-side.
        bytes: Vec<u8>,
    },
    /// The editor window opened, at the size the plugin asked for.
    EditorOpened {
        /// Editor width in pixels.
        width: u32,
        /// Editor height in pixels.
        height: u32,
    },
    /// The editor window closed, whether the host or the plugin closed it.
    EditorClosed,
    /// The plugin moved one of its own parameters, typically from its editor.
    ParameterChanged {
        /// Positional parameter index as the plugin reported it.
        index: i32,
        /// Normalized `0.0..=1.0`.
        value: f32,
    },
    /// Plugin reported a latency change at runtime. Host updates the
    /// corresponding `PluginClient::set_latency` so `AudioUnit::latency()`
    /// reports the new value. Note: does NOT trigger PDC re-analysis;
    /// the graph must be committed again for compensation to update.
    LatencyChanged {
        /// The plugin's new reported latency, in frames.
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
        /// The plugin's new tail, which may be bounded, unbounded or unreported.
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
        /// The plugin's presets, in its own order.
        presets: Vec<Preset>,
    },
    /// Whether the plugin accepted a [`LoadPreset`](HostMessage::LoadPreset).
    ///
    /// `false` is a refusal rather than an error, and is also the honest
    /// answer for VST3: its programs are selected through the parameter path,
    /// so there is no direct load to report on.
    PresetLoaded {
        /// Whether the plugin accepted the preset.
        ok: bool,
    },
    /// The preset the plugin considers current, if it will say. `None` means
    /// the format has no query (VST3, CLAP) or the plugin declined — never
    /// "the first one".
    CurrentPreset {
        /// The current preset's opaque id, if the plugin will say.
        id: Option<PresetId>,
    },
    /// The subprocess mapped the shared-memory slab and is ready to process.
    SharedMemoryReady,
    /// The subprocess failed at something it was asked to do.
    ///
    /// Fire-and-forget: nothing awaits this, so a request that a caller blocks
    /// on needs its own reply variant rather than relying on this.
    Error {
        /// Human-readable reason, already formatted subprocess-side.
        message: String,
    },
    /// Handshake sent once the subprocess is live. Carries the wire
    /// [`PROTOCOL_VERSION`](super::PROTOCOL_VERSION) so a host and subprocess
    /// built from mismatched commits refuse to proceed rather than mis-parsing
    /// each other's messages (bincode is not self-describing). `#[serde(default)]`
    /// = 0 for an ancient binary that predates the field → treated as a mismatch.
    Ready {
        /// The subprocess's [`PROTOCOL_VERSION`](super::PROTOCOL_VERSION).
        #[serde(default)]
        protocol_version: u32,
    },
    /// The subprocess is exiting.
    Shutdown,
    /// The plugin's display string for the value that was asked about. `None`
    /// means the plugin did not answer — the caller renders the raw number
    /// rather than treating this as an empty label.
    ParameterText {
        /// The plugin's display string, or `None` if it did not answer.
        text: Option<String>,
    },
    /// The value the plugin parsed a string into, normalized. `None` means it
    /// could not parse it, and the caller must leave its field unchanged.
    ParameterValueFromText {
        /// The parsed value, normalized.
        value: Option<Normalized>,
    },
    /// Acknowledges a `HostMessage::LoadState`, carrying the plugin's refusal
    /// if it had one.
    ///
    /// A caller awaits this frame, so the answer reports that the state was
    /// *loaded* rather than merely that the request was sent. It carries the
    /// message rather than a bool because the subprocess has already formatted
    /// it and a caller wants to show the user *why* their preset did not load.
    StateLoaded {
        /// The plugin's refusal, or `None` on success.
        error: Option<String>,
    },
}
