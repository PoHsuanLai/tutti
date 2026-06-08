//! In-process messages between the audio thread and the bridge thread.
//! Distinct from wire-format `HostMessage`/`BridgeMessage` in `protocol/`.

use super::ask::Reply;
use crate::protocol::{
    MidiEventVec, NoteExpressionChanges, ParameterChanges, ParameterInfo, TransportInfo,
};

/// Audio-thread bulk payload for one `Process` command. Heap-boxed and
/// recycled between calls to avoid RT allocation.
#[derive(Debug)]
pub(super) struct ProcessPayload {
    pub buffer_id: u32,
    pub num_samples: usize,
    pub midi_events: MidiEventVec,
    pub param_changes: ParameterChanges,
    pub note_expression: NoteExpressionChanges,
    pub transport: TransportInfo,
}

impl ProcessPayload {
    pub(super) fn empty() -> Self {
        Self {
            buffer_id: 0,
            num_samples: 0,
            midi_events: MidiEventVec::new(),
            param_changes: ParameterChanges::new(),
            note_expression: NoteExpressionChanges::new(),
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
        param_id: u32,
        value: f32,
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
        param_id: u32,
        reply: Reply<Option<f32>>,
    },
}

/// Bridge-thread → audio-thread (RT response path).
///
/// `Process` is on the RT path so it stays on a dedicated lock-free
/// queue, not on a per-request `Reply` (audio thread can't block).
#[derive(Debug, Clone)]
pub(super) enum AudioResponse {
    AudioProcessed,
    Error,
}

/// Plugin-originated, unsolicited events observed on the control stream.
/// Delivered to a listener installed on [`super::AudioBridge`] via
/// `set_listener`; the bridge thread invokes it after draining replies.
#[derive(Debug, Clone)]
pub enum BridgeEvent {
    LatencyChanged { samples: usize },
    ParameterChanged { index: i32, value: f32 },
    /// The plugin asked the host to resync some aspect of its state at runtime
    /// (preset load, param-title change, IO change, full reload). Carries no
    /// payload — the host re-reads from the plugin in response.
    Resync(ResyncKind),
}

/// Which aspect of plugin state a [`BridgeEvent::Resync`] asks the host to
/// re-read. Distinct from `LatencyChanged`/`ParameterChanged`, which carry the
/// new value inline; these say only "your cached view of X is stale."
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
