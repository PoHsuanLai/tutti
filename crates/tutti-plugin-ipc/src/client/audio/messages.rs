//! In-process messages between the audio thread and the bridge thread.
//! Distinct from wire-format `HostMessage`/`BridgeMessage` in `protocol/`.

use super::ask::Reply;
use crate::protocol::{
    ChordChanges, MidiEventVec, Normalized, NoteExpressionChanges, NoteExpressionIntChanges,
    NoteExpressionTextChanges, ParamEdit, ParamId, ParameterChanges, ParameterInfo, PluginTail,
    Preset, PresetId, Samples, ScaleChanges, TransportChanges, TransportInfo,
};
use dpi::Size;
use tutti_plugin_types::{
    BlockStatus, ContentScale, PluginFailure, ResyncKind, StateError, StateUse,
};
use tutti_types::SampleRate;

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
    /// The transport changes inside the block (never at frame 0).
    pub transport_changes: TransportChanges,
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
            transport_changes: TransportChanges::new(),
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
        param_id: ParamId,
        value: Normalized,
    },
    SetAutomationState {
        /// Format-neutral automation mode; each format loader encodes it onto its
        /// own ABI at the FFI edge (VST3 `IAutomationState`, etc.). The wire does
        /// NOT carry a format-specific bitmask — see [`AutomationMode`](crate::protocol::AutomationMode).
        mode: crate::protocol::AutomationMode,
    },
    /// What the host says of the plugin's track; boxed to keep the queue's
    /// slots small.
    SetTrackInfo {
        info: Box<tutti_plugin_types::TrackInfo>,
    },
    SetSampleRate {
        rate: SampleRate,
    },
    /// Format-neutral render mode, encoded onto each format's own ABI at the
    /// FFI edge (VST3 `ProcessSetup::processMode`, CLAP `clap.render`, AU
    /// `kAudioUnitProperty_OfflineRender`, VST2
    /// `audioMasterGetCurrentProcessLevel`).
    SetRenderMode {
        mode: crate::protocol::RenderMode,
    },
    Reset,
    /// Reads what the plugin said unprompted off the control connection, so
    /// it reaches the listener while no other command is waiting on a reply.
    Drain,
    Shutdown,
    /// Chunked state read.
    ///
    /// `progress` is advanced per chunk so the waiting caller can tell a slow
    /// transfer from a stalled one; see [`StateProgress`](super::progress::StateProgress).
    SaveState {
        state_use: StateUse,
        progress: super::progress::StateProgress,
        reply: Reply<std::result::Result<Vec<u8>, StateError>>,
    },
    /// Chunked state write. `progress` carries the same meaning as on
    /// [`SaveState`](Self::SaveState) — bytes handed to the socket rather than
    /// bytes received.
    LoadState {
        state_use: StateUse,
        data: Vec<u8>,
        progress: super::progress::StateProgress,
        reply: Reply<std::result::Result<(), StateError>>,
    },
    GetParameterList {
        reply: Reply<Option<Vec<ParameterInfo>>>,
    },
    GetParameter {
        param_id: ParamId,
        reply: Reply<Option<Normalized>>,
    },
    GetPresetList {
        reply: Reply<Option<Vec<Preset>>>,
    },
    LoadPreset {
        id: PresetId,
        reply: Reply<bool>,
    },
    GetCurrentPreset {
        reply: Reply<Option<PresetId>>,
    },
    GetParameterText {
        param_id: ParamId,
        value: Normalized,
        reply: Reply<Option<String>>,
    },
    GetParameterValueFromText {
        param_id: ParamId,
        text: String,
        reply: Reply<Option<Normalized>>,
    },
    GetParameterPlain {
        param_id: ParamId,
        value: Normalized,
        reply: Reply<Option<f64>>,
    },
    GetParameterNormalized {
        param_id: ParamId,
        plain: f64,
        reply: Reply<Option<Normalized>>,
    },
    /// Open the editor in the server (`None` parent: floating).
    OpenEditor {
        parent: Option<u64>,
        scale: ContentScale,
        reply: Reply<std::result::Result<Option<Size>, String>>,
    },
    CloseEditor,
    SetEditorSize {
        size: Size,
        reply: Reply<std::result::Result<Size, String>>,
    },
}

/// Bridge-thread → audio-thread (RT response path).
///
/// `Process` is on the RT path so it stays on a dedicated lock-free
/// queue, not on a per-request `Reply` (audio thread can't block).
///
/// **This queue carries no evidence about audio.** The slab answers that itself,
/// with a per-slot sequence number the server publishes after the last sample.
/// So these are notifications, not permissions — the host reads audio on the
/// strength of the slab, and emits silence for an unpublished block even if a
/// reply for it arrives.
///
/// What remains is the plugin's MIDI-out. The wire adapter decodes it on the
/// bridge thread (off-RT, see `dispatch`), so the list moves through the queue
/// already built; the RT thread only drains it into caller storage.
// `AudioProcessed` holds an inline-256 `MidiEventVec` (~5 KB) vs the zero-size
// `Error`. Intentional: the list is inline so popping + dropping it on the
// RT audio thread never touches the heap (see `submit`). Boxing would defeat
// that by moving the free onto the RT thread.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub(super) enum AudioResponse {
    AudioProcessed {
        /// The block this answers, as echoed by the server: its MIDI-out
        /// belongs with that block's audio (`take_replies`). The slab is what
        /// establishes the audio's validity.
        seq: u64,
        midi_out: MidiEventVec,
        /// What the plugin said of itself at the end of the block.
        status: BlockStatus,
    },
    /// A block failed. `seq` is `Some` when the failure is attributable to one
    /// request (the server replied `Error` to it) and `None` for a
    /// connection-level failure that ends every in-flight block. Either way the
    /// host needs no action: the server never published, so the sequence check
    /// fails and silence follows.
    Error { seq: Option<u64> },
}

/// Plugin-originated, unsolicited events observed on the control stream.
/// Delivered to a listener installed on `super::AudioBridge` via
/// `set_listener`; the bridge thread invokes it after draining replies.
#[derive(Debug, Clone)]
pub enum BridgeEvent {
    /// The plugin's processing latency changed, in frames.
    LatencyChanged {
        /// The latency it reports now.
        samples: Samples,
    },
    /// The plugin reported a new tail length at runtime (CLAP only — see
    /// [`BridgeMessage::TailChanged`](crate::protocol::BridgeMessage)).
    TailChanged {
        /// The tail it reports now.
        tail: PluginTail,
    },
    /// The plugin edited a parameter itself (its editor, its own automation or
    /// a modulation it reports): one [`ParamEdit`], in the order the plugin
    /// made them.
    ParameterChanged {
        /// The edit: the parameter, its value now and the gesture it falls in.
        edit: ParamEdit,
    },
    /// The plugin asked the host to resync some aspect of its state at runtime
    /// (preset load, param-title change, IO change, full reload). Carries no
    /// payload — the host re-reads from the plugin in response.
    Resync(ResyncKind),
    /// The plugin asked to be processed again (CLAP's `request_process`, after
    /// it slept). Carries no payload: the host wakes the node it parked.
    ProcessRequested,
    /// The plugin asked the host's transport to do something, in the order it
    /// asked.
    Transport {
        /// What it asked.
        request: tutti_plugin_types::TransportRequest,
    },
    /// The plugin refused the rate it was asked to run at (`Some`: the rate in
    /// Hz and the plugin's reason), or took a rate after refusing one (`None`).
    RateRefusal(Option<(SampleRate, String)>),
    /// The bridge died: the subprocess never connected, failed the handshake,
    /// or its stream dropped mid-session.
    ///
    /// Unlike every other variant, this one is not the *plugin* speaking — it
    /// is the bridge reporting that the plugin can no longer speak at all. It
    /// fires once, from the site that noticed, and is terminal: the engine
    /// offers no relaunch, so recovery means loading a fresh plugin.
    ///
    /// `cause` holds the `BridgeError` that ended the stream behind an `Arc`
    /// (that type is not `Clone`, and the reason has to outlive the call that
    /// produced it), reached as the failure's `source`. A host that asks later
    /// gets the real reason instead of a placeholder.
    ///
    /// **May arrive with no listener.** Two of the three crash sites run before
    /// `set_listener`, so a subscriber is not guaranteed to see this. The
    /// authoritative answer is the latched cause behind
    /// `PluginHandle::status`; this event is how a host learns *promptly*, not
    /// how it learns *reliably*.
    Crashed {
        /// What the site that noticed saw.
        cause: PluginFailure,
    },
}

/// How many transport requests [`Unsolicited`] holds between two drains.
pub(super) const UNSOLICITED_TRANSPORT: usize = 64;

/// How many parameter events [`Unsolicited`] holds between two drains.
pub(super) const UNSOLICITED_PARAMS: usize = 128;

/// The unsolicited events read off the control stream while a command waited
/// for its reply, held until `pump` hands them to the listener.
///
/// A plain buffer, not a queue between threads: `recv_reply` fills it and
/// `pump` drains it after each command, both on the bridge thread, so it is
/// owned by `pump` and lent to `handle`. The listener runs after the drain
/// rather than from `recv_reply` itself so a slow listener never delays the
/// reply a blocked caller is waiting on.
///
/// **State events cannot be lost.** A latency, a tail and each resync kind is
/// a latest-wins slot: a plugin reporting its latency twice during one
/// command is heard once, with the second figure, and no flood of anything
/// else can evict it (PDC would never hear of it). Only parameter events
/// queue, bounded and allocated once: past [`UNSOLICITED_PARAMS`] the oldest
/// is dropped, counted ([`take_dropped`](Self::take_dropped)), and a
/// `Resync(ParamValues)` is raised in its place, so a host that lost a
/// parameter value is told to re-read them all.
///
/// **Delivery order** ([`drain`](Self::drain)), per command:
///
/// 1. `Resync(Reloaded)`, then `Resync(Io)`: structural changes first. A
///    plugin that rebuilds itself or its buses and then reports parameters
///    reports them against the new table, so the host must rebuild before
///    it applies them.
/// 2. The parameter events, in arrival order.
/// 3. The latency, then the tail: values the host takes as they stand.
/// 4. `Resync(ParamTitles)`, then `Resync(ParamValues)`: a re-read
///    reads the plugin as it is now, so it must win over any queued value,
///    and a drop's `ParamValues` must follow the values that survived it.
/// 5. `ProcessRequested`, last: the node it wakes finds the host's view of
///    the plugin already current.
///
/// What is lost is the order of a parameter event *before* a structural
/// change in the same command: it is delivered after it, against the
/// rebuilt table. A `Reloaded` asks the host to re-read everything, which
/// covers it.
///
/// Filled one kind at a time ([`param`](Self::param),
/// [`latency`](Self::latency), [`tail`](Self::tail), [`resync`](Self::resync)),
/// so [`Crashed`](BridgeEvent::Crashed), which the bridge sends from the site
/// that noticed, cannot come through here.
pub(super) struct Unsolicited {
    params: std::collections::VecDeque<ParamEdit>,
    latency: Option<Samples>,
    tail: Option<PluginTail>,
    rate: Option<Option<(SampleRate, String)>>,
    /// Rate changes the bridge stopped waiting for, whose answers have not
    /// arrived.
    late_rate_owed: u32,
    /// What a late answer is applied to, when it is read.
    lifecycle: Option<super::lifecycle::Lifecycle>,
    reloaded: bool,
    io: bool,
    param_titles: bool,
    param_values: bool,
    process_requested: bool,
    /// Transport requests, oldest first. Every one counts, so none is merged:
    /// past [`UNSOLICITED_TRANSPORT`] the oldest is dropped.
    transport: std::collections::VecDeque<tutti_plugin_types::TransportRequest>,
    /// Parameter events dropped since the last [`take_dropped`](Self::take_dropped).
    dropped: u64,
}

impl Unsolicited {
    pub(super) fn new() -> Self {
        Self {
            params: std::collections::VecDeque::with_capacity(UNSOLICITED_PARAMS),
            latency: None,
            tail: None,
            rate: None,
            late_rate_owed: 0,
            lifecycle: None,
            reloaded: false,
            io: false,
            param_titles: false,
            param_values: false,
            process_requested: false,
            transport: std::collections::VecDeque::with_capacity(UNSOLICITED_TRANSPORT),
            dropped: 0,
        }
    }

    /// Queue a parameter event, dropping (and counting) the oldest when full.
    pub(super) fn param(&mut self, edit: ParamEdit) {
        if self.params.len() == UNSOLICITED_PARAMS {
            self.params.pop_front();
            self.dropped += 1;
            self.resync(ResyncKind::ParamValues);
        }
        self.params.push_back(edit);
    }

    /// The plugin's new latency, replacing one already held.
    pub(super) fn latency(&mut self, samples: Samples) {
        self.latency = Some(samples);
    }

    /// The plugin's new tail, replacing one already held.
    pub(super) fn tail(&mut self, tail: PluginTail) {
        self.tail = Some(tail);
    }

    /// A rate change was given up on; its answer is still to come.
    pub(super) fn expect_late_rate_reply(&mut self) {
        self.late_rate_owed = self.late_rate_owed.saturating_add(1);
    }

    /// An inbox whose late rate answers are applied to `lifecycle`.
    pub(super) fn with_lifecycle(lifecycle: super::lifecycle::Lifecycle) -> Self {
        Self {
            lifecycle: Some(lifecycle),
            ..Self::new()
        }
    }

    /// Take `SampleRateSet` as a late answer if one is owed (`true`), applying
    /// it **now**: it is read in stream order, ahead of whatever is read after
    /// it, so a later answer (the request being waited on) lands after it and
    /// wins. `false` when it answers the request being waited on.
    pub(super) fn late_rate_reply(&mut self, rate: SampleRate, error: Option<String>) -> bool {
        if self.late_rate_owed == 0 {
            return false;
        }
        self.late_rate_owed -= 1;
        if let Some(lifecycle) = self.lifecycle.clone() {
            super::dispatch::apply_rate_reply(&lifecycle, self, rate, error);
        }
        true
    }

    /// The server's editor asked its window for `size`.
    pub(super) fn editor_wants_size(&mut self, size: Size) {
        if let Some(lifecycle) = &self.lifecycle {
            lifecycle.set_editor_wants(size);
        }
    }

    /// The server closed its editor on its own account.
    pub(super) fn editor_closed(&mut self) {
        if let Some(lifecycle) = &self.lifecycle {
            lifecycle.set_editor_closed();
        }
    }

    /// The plugin asked to be processed again; a second request adds nothing.
    pub(super) fn process_requested(&mut self) {
        self.process_requested = true;
    }

    /// Queue a transport request, dropping the oldest when full.
    pub(super) fn transport(&mut self, request: tutti_plugin_types::TransportRequest) {
        if self.transport.len() == UNSOLICITED_TRANSPORT {
            self.transport.pop_front();
        }
        self.transport.push_back(request);
    }

    /// A rate refusal, or its clearing, replacing one already held.
    pub(super) fn rate_refusal(&mut self, refusal: Option<(SampleRate, String)>) {
        self.rate = Some(refusal);
    }

    /// A resync request; a second of the same kind adds nothing.
    pub(super) fn resync(&mut self, kind: ResyncKind) {
        *match kind {
            ResyncKind::Reloaded => &mut self.reloaded,
            ResyncKind::Io => &mut self.io,
            ResyncKind::ParamTitles => &mut self.param_titles,
            ResyncKind::ParamValues => &mut self.param_values,
        } = true;
    }

    /// The held events, leaving the buffer empty, in the order the type
    /// documents: structural resyncs, parameters, latency, tail, refresh
    /// resyncs.
    pub(super) fn drain(&mut self) -> impl Iterator<Item = BridgeEvent> + '_ {
        let raised =
            |flag: &mut bool, kind| std::mem::take(flag).then_some(BridgeEvent::Resync(kind));
        let structural = [
            raised(&mut self.reloaded, ResyncKind::Reloaded),
            raised(&mut self.io, ResyncKind::Io),
        ];
        let refresh = [
            raised(&mut self.param_titles, ResyncKind::ParamTitles),
            raised(&mut self.param_values, ResyncKind::ParamValues),
        ];
        let params = self
            .params
            .drain(..)
            .map(|edit| BridgeEvent::ParameterChanged { edit });
        let latency = self
            .latency
            .take()
            .map(|samples| BridgeEvent::LatencyChanged { samples });
        let tail = self
            .tail
            .take()
            .map(|tail| BridgeEvent::TailChanged { tail });
        let transport = self
            .transport
            .drain(..)
            .map(|request| BridgeEvent::Transport { request });
        let rate = self.rate.take().map(BridgeEvent::RateRefusal);
        let process =
            std::mem::take(&mut self.process_requested).then_some(BridgeEvent::ProcessRequested);
        structural
            .into_iter()
            .flatten()
            .chain(params)
            .chain(latency)
            .chain(tail)
            .chain(transport)
            .chain(rate)
            .chain(refresh.into_iter().flatten())
            .chain(process)
    }

    /// How many parameter events were dropped since the last call.
    pub(super) fn take_dropped(&mut self) -> u64 {
        std::mem::take(&mut self.dropped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each drained event as a short label, for whole-sequence asserts.
    fn labels(inbox: &mut Unsolicited) -> Vec<String> {
        inbox
            .drain()
            .map(|ev| match ev {
                BridgeEvent::ParameterChanged { edit } => format!("p{}", edit.id()),
                BridgeEvent::LatencyChanged { samples } => format!("latency {}", samples.get()),
                BridgeEvent::TailChanged { .. } => "tail".to_owned(),
                BridgeEvent::ProcessRequested => "process".to_owned(),
                BridgeEvent::Transport { request } => format!("transport {request:?}"),
                BridgeEvent::Resync(kind) => format!("{kind:?}"),
                other => panic!("unexpected {other:?}"),
            })
            .collect()
    }

    /// **A request to be processed again is heard once, after everything the
    /// host re-reads for**, so the node it wakes finds the host's view of the
    /// plugin current.
    #[test]
    fn a_process_request_is_heard_once_and_last() {
        let mut inbox = Unsolicited::new();
        inbox.process_requested();
        inbox.resync(ResyncKind::ParamValues);
        inbox.latency(Samples::new(64));
        inbox.process_requested();
        assert_eq!(labels(&mut inbox), ["latency 64", "ParamValues", "process"]);
        assert!(labels(&mut inbox).is_empty());
    }

    /// **The inbox hands parameter events over in arrival order and
    /// empties.**
    #[test]
    fn the_inbox_keeps_arrival_order_and_empties() {
        let mut inbox = Unsolicited::new();
        for i in 0..3 {
            inbox.param(ParamEdit::change(
                ParamId::new(i as u32),
                Normalized::default(),
            ));
        }
        inbox.latency(Samples::new(64));
        assert_eq!(labels(&mut inbox), ["p0", "p1", "p2", "latency 64"]);
        assert!(labels(&mut inbox).is_empty());
        assert_eq!(inbox.take_dropped(), 0);
    }

    /// **A flood of parameter events cannot evict a state event.** Latency,
    /// tail and resync arrive first, then more parameter events than the
    /// inbox holds (a preset load inside one `LoadState`): every state event
    /// is still delivered, latest-wins, the oldest parameter events are the
    /// ones dropped, the drops are counted, a `ParamValues` resync stands in
    /// for them, and the inbox never reallocates.
    #[test]
    fn a_parameter_flood_drops_only_parameters() {
        let mut inbox = Unsolicited::new();
        let cap = inbox.params.capacity();
        inbox.latency(Samples::new(32));
        inbox.tail(PluginTail::default());
        inbox.resync(ResyncKind::Reloaded);
        inbox.latency(Samples::new(64));
        let n = UNSOLICITED_PARAMS as i32;
        for i in 0..n + 2 {
            inbox.param(ParamEdit::change(
                ParamId::new(i as u32),
                Normalized::default(),
            ));
        }
        assert_eq!(inbox.params.capacity(), cap, "the inbox reallocated");
        assert_eq!(inbox.take_dropped(), 2);
        assert_eq!(inbox.take_dropped(), 0, "taking the count resets it");

        let got = labels(&mut inbox);
        let (reloaded, rest) = got.split_at(1);
        let (params, state) = rest.split_at(UNSOLICITED_PARAMS);
        assert_eq!(reloaded, ["Reloaded"]);
        assert_eq!(params.first().map(String::as_str), Some("p2"));
        assert_eq!(params.last(), Some(&format!("p{}", n + 1)));
        assert_eq!(state, ["latency 64", "tail", "ParamValues"]);
    }

    /// **Structural resyncs come before the parameter events, refresh
    /// resyncs after them.** A plugin that reloads (or rebuilds its buses)
    /// and then reports parameters reports them against its new table, so
    /// the host must hear `Reloaded` and `Io` first; a `ParamValues` re-read
    /// must win over every queued value, so it comes last. Every kind is
    /// raised, in the reverse of the delivery order, around the parameters.
    #[test]
    fn structural_resyncs_lead_and_refreshes_trail() {
        let mut inbox = Unsolicited::new();
        inbox.resync(ResyncKind::ParamValues);
        inbox.resync(ResyncKind::ParamTitles);
        inbox.tail(PluginTail::default());
        inbox.latency(Samples::new(16));
        inbox.resync(ResyncKind::Io);
        inbox.resync(ResyncKind::Reloaded);
        inbox.param(ParamEdit::change(ParamId::new(0), Normalized::default()));
        inbox.param(ParamEdit::change(ParamId::new(1), Normalized::default()));
        inbox.resync(ResyncKind::Reloaded);
        assert_eq!(
            labels(&mut inbox),
            [
                "Reloaded",
                "Io",
                "p0",
                "p1",
                "latency 16",
                "tail",
                "ParamTitles",
                "ParamValues"
            ]
        );
        assert!(labels(&mut inbox).is_empty(), "every slot was taken");
    }
}
