//! In-process messages between the audio thread and the bridge thread.
//! Distinct from wire-format `HostMessage`/`BridgeMessage` in `protocol/`.

use super::ask::Reply;
use crate::protocol::{
    ChordChanges, MidiEventVec, NoteExpressionChanges, NoteExpressionIntChanges,
    NoteExpressionTextChanges, ParamAddress, ParameterChanges, ParameterInfo, PluginTail, Samples,
    ScaleChanges, TransportInfo,
};

/// Audio-thread bulk payload for one `Process` command. Heap-boxed and
/// recycled between calls to avoid RT allocation.
#[derive(Debug)]
pub(super) struct ProcessPayload {
    /// Which block this is. Monotonic per plugin instance, starting at 1, and
    /// the same number that indexes this block's slab ring slot — the host and
    /// the server address the shared region by it.
    pub seq: u64,
    pub num_samples: usize,
    pub midi_events: MidiEventVec,
    pub param_changes: ParameterChanges,
    pub note_expression: NoteExpressionChanges,
    /// VST3 sequencer-context inputs. Empty until a host produces them.
    pub chords: ChordChanges,
    pub scales: ScaleChanges,
    pub expr_texts: NoteExpressionTextChanges,
    pub expr_ints: NoteExpressionIntChanges,
    pub transport: TransportInfo,
}

impl ProcessPayload {
    pub(super) fn empty() -> Self {
        Self {
            seq: 0,
            num_samples: 0,
            midi_events: MidiEventVec::new(),
            param_changes: ParameterChanges::new(),
            note_expression: NoteExpressionChanges::new(),
            chords: ChordChanges::new(),
            scales: ScaleChanges::new(),
            expr_texts: NoteExpressionTextChanges::new(),
            expr_ints: NoteExpressionIntChanges::new(),
            transport: TransportInfo::default(),
        }
    }
}

/// Audio-thread → bridge-thread.
///
/// Variants that need a reply carry their own [`Reply<T>`]; the calling
/// thread holds the paired `Ask<T>` and blocks on it. There is no shared
/// response queue — responses cannot be misrouted between requests.
pub(super) enum Command {
    Process(Box<ProcessPayload>),
    SetParameter {
        param_id: ParamAddress,
        value: f32,
    },
    SetAutomationState {
        /// Format-neutral automation mode; each format loader encodes it onto its
        /// own ABI at the FFI edge (VST3 `IAutomationState`, etc.). The wire does
        /// NOT carry a format-specific bitmask — see [`AutomationMode`].
        mode: crate::protocol::AutomationMode,
    },
    SetSampleRate {
        rate: f64,
    },
    Reset,
    Shutdown,
    SaveState {
        reply: Reply<Option<Vec<u8>>>,
    },
    LoadState {
        data: Vec<u8>,
        reply: Reply<bool>,
    },
    GetParameterList {
        reply: Reply<Option<Vec<ParameterInfo>>>,
    },
    GetParameter {
        param_id: ParamAddress,
        reply: Reply<Option<f32>>,
    },
}

/// Bridge-thread → audio-thread (RT response path).
///
/// `Process` is on the RT path so it stays on a dedicated lock-free
/// queue, not on a per-request `Reply` (audio thread can't block).
///
/// **This queue no longer carries evidence about audio.** It once did: the RT
/// thread waited for the reply matching the block it had just submitted, because
/// nothing else could tell it whether the slab held that block's output. The
/// slab now answers that itself, with a per-slot sequence number the server
/// publishes after the last sample. So these are notifications, not permissions
/// — the host reads audio on the strength of the slab, and would emit silence
/// for an unpublished block even if a reply for it arrived.
///
/// What remains is the plugin's MIDI-out. The `IpcMidiEvent → MidiEvent`
/// conversion happens on the bridge thread (off-RT, see `dispatch`), so the
/// SmallVec moves through the queue already built; the RT thread only drains it
/// into caller storage.
// `AudioProcessed` holds an inline-256 `MidiEventVec` (~5 KB) vs the zero-size
// `Error`. Intentional: the SmallVec stays inline so popping + dropping it on the
// RT audio thread never touches the heap (see `submit`). Boxing would defeat
// that by moving the free onto the RT thread.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub(super) enum AudioResponse {
    AudioProcessed {
        /// The block this answers, as echoed by the server. Kept for diagnostics
        /// and ordering; the slab is what establishes validity.
        #[allow(dead_code)]
        seq: u64,
        midi_out: MidiEventVec,
    },
    /// A block failed. `seq` is `Some` when the failure is attributable to one
    /// request (the server replied `Error` to it) and `None` for a
    /// connection-level failure that ends every in-flight block. Either way the
    /// host needs no action: the server never published, so the sequence check
    /// fails and silence follows.
    Error {
        #[allow(dead_code)]
        seq: Option<u64>,
    },
}

/// Plugin-originated, unsolicited events observed on the control stream.
/// Delivered to a listener installed on [`super::AudioBridge`] via
/// `set_listener`; the bridge thread invokes it after draining replies.
#[derive(Debug, Clone)]
pub enum BridgeEvent {
    LatencyChanged {
        samples: Samples,
    },
    /// The plugin reported a new tail length at runtime (CLAP only — see
    /// [`BridgeMessage::TailChanged`](crate::protocol::BridgeMessage)).
    TailChanged {
        tail: PluginTail,
    },
    ParameterChanged {
        index: i32,
        value: f32,
    },
    /// The plugin asked the host to resync some aspect of its state at runtime
    /// (preset load, param-title change, IO change, full reload). Carries no
    /// payload — the host re-reads from the plugin in response.
    Resync(ResyncKind),
}

/// Which aspect of plugin state a [`BridgeEvent::Resync`] asks the host to
/// re-read. Distinct from `LatencyChanged`/`ParameterChanged`, which carry the
/// new value inline; these say only "your cached view of X is stale."
///
/// This is the internal wire vocabulary. The public [`PluginHandle`] callbacks
/// split it *by consequence* into [`PluginRefresh`] (cosmetic, re-read a cached
/// view) and [`PluginInvalidation`] (structural, re-plan the graph) — see
/// [`ResyncKind::classify`].
///
/// [`PluginHandle`]: crate::host::handles::PluginHandle
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResyncKind {
    /// Re-read all parameter values (plugin loaded a preset / wrote them back).
    ParamValues,
    /// Re-pull the parameter list (titles, units, or flags changed).
    ParamTitles,
    /// Re-read the bus layout and rewire the audio graph.
    Io,
    /// The plugin instance was rebuilt; resync everything.
    Reloaded,
}

impl ResyncKind {
    /// Split this wire signal into its host-facing consequence: a cosmetic
    /// [`PluginRefresh`] (re-read a cached view, no graph edit) or a structural
    /// [`PluginInvalidation`] (rewire + PDC re-plan).
    pub fn classify(self) -> ResyncClass {
        match self {
            ResyncKind::ParamValues => ResyncClass::Refresh(PluginRefresh::ParamValues),
            ResyncKind::ParamTitles => ResyncClass::Refresh(PluginRefresh::ParamTitles),
            ResyncKind::Io => ResyncClass::Invalidate(PluginInvalidation::Io),
            ResyncKind::Reloaded => ResyncClass::Invalidate(PluginInvalidation::Reloaded),
        }
    }
}

/// The consequence a [`ResyncKind`] maps to — which of the two split callbacks
/// the host should fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResyncClass {
    Refresh(PluginRefresh),
    Invalidate(PluginInvalidation),
}

/// A **cosmetic** plugin→host notification: the host's cached *view* of some
/// plugin state is stale and should be re-read, but the audio graph is
/// unaffected. Delivered via
/// [`PluginHandle::on_refresh`](crate::host::handles::PluginHandle::on_refresh).
/// Mirrors CLAP `params.rescan(flags)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginRefresh {
    /// Re-read all parameter values (a preset load / internal write-back).
    ParamValues,
    /// Re-pull the parameter list (titles, units, or flags changed).
    ParamTitles,
}

/// A **structural** plugin→host notification: the plugin changed in a way that
/// invalidates the audio graph's plan, so the host must rewire and re-run
/// latency compensation (PDC). Delivered via
/// [`PluginHandle::on_invalidate`](crate::host::handles::PluginHandle::on_invalidate).
/// Mirrors CLAP `request_restart()` + `audio_ports.rescan()` + `latency.changed()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginInvalidation {
    /// The plugin reported new processing latency. Carries the new value; the
    /// node's own atomic is already updated live, but compensation delays across
    /// the graph only re-plan on a commit.
    Latency { samples: Samples },
    /// The plugin reported a new tail length. Carries the new value; the node's
    /// own cell is already updated live, but an offline render sizes its length
    /// once at the start, so a bounce already in flight keeps the old figure.
    Tail { tail: PluginTail },
    /// The plugin's bus layout changed — re-read it and rewire the graph.
    Io,
    /// The plugin instance was rebuilt in place; re-plan everything.
    Reloaded,
}
