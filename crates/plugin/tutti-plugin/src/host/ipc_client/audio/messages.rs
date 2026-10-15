//! In-process messages between the audio thread and the bridge thread.
//! Distinct from wire-format `HostMessage`/`BridgeMessage` in `protocol/`.

use super::ask::Reply;
use crate::error::StateError;
use crate::protocol::{
    ChordChanges, MidiEventVec, Normalized, NoteExpressionChanges, NoteExpressionIntChanges,
    NoteExpressionTextChanges, ParamAddress, ParameterChanges, ParameterInfo, PluginTail, Preset,
    PresetId, Samples, ScaleChanges, TransportInfo,
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
    /// Format-neutral render mode, encoded onto each format's own ABI at the
    /// FFI edge (VST3 `ProcessSetup::processMode`, CLAP `clap.render`, AU
    /// `kAudioUnitProperty_OfflineRender`, VST2
    /// `audioMasterGetCurrentProcessLevel`).
    SetRenderMode {
        mode: crate::protocol::RenderMode,
    },
    Reset,
    Shutdown,
    /// Chunked state read.
    ///
    /// `progress` is advanced per chunk so the waiting caller can tell a slow
    /// transfer from a stalled one; see [`StateProgress`](super::progress::StateProgress).
    SaveState {
        progress: super::progress::StateProgress,
        reply: Reply<std::result::Result<Vec<u8>, StateError>>,
    },
    /// Chunked state write. `progress` carries the same meaning as on
    /// [`SaveState`](Self::SaveState) — bytes handed to the socket rather than
    /// bytes received.
    LoadState {
        data: Vec<u8>,
        progress: super::progress::StateProgress,
        reply: Reply<std::result::Result<(), StateError>>,
    },
    GetParameterList {
        reply: Reply<Option<Vec<ParameterInfo>>>,
    },
    GetParameter {
        param_id: ParamAddress,
        reply: Reply<Option<f32>>,
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
        param_id: ParamAddress,
        value: Normalized,
        reply: Reply<Option<String>>,
    },
    GetParameterValueFromText {
        param_id: ParamAddress,
        text: String,
        reply: Reply<Option<Normalized>>,
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
/// What remains is the plugin's MIDI-out. The `IpcMidiEvent → MidiEvent`
/// conversion happens on the bridge thread (off-RT, see `dispatch`), so the
/// list moves through the queue already built; the RT thread only drains it
/// into caller storage.
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
    },
    /// A block failed. `seq` is `Some` when the failure is attributable to one
    /// request (the server replied `Error` to it) and `None` for a
    /// connection-level failure that ends every in-flight block. Either way the
    /// host needs no action: the server never published, so the sequence check
    /// fails and silence follows.
    Error { seq: Option<u64> },
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
    /// The bridge died: the subprocess never connected, failed the handshake,
    /// or its stream dropped mid-session.
    ///
    /// Unlike every other variant, this one is not the *plugin* speaking — it
    /// is the bridge reporting that the plugin can no longer speak at all. It
    /// fires once, from the site that noticed, and is terminal: the engine
    /// offers no relaunch, so recovery means loading a fresh plugin.
    ///
    /// `cause` is a message rather than a `BridgeError` because that type is
    /// not `Clone` and the reason has to outlive the call that produced it.
    /// Stringifying at the detection site is what makes the cause available at
    /// all — a host that asks later gets the real reason instead of a
    /// placeholder.
    ///
    /// **May arrive with no listener.** Two of the three crash sites run before
    /// `set_listener`, so a subscriber is not guaranteed to see this. The
    /// authoritative answer is the latched cause behind
    /// `PluginHandle::status`; this event is how a host learns *promptly*, not
    /// how it learns *reliably*.
    Crashed {
        cause: String,
    },
}

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
/// 4. `Resync(ParamTitles)`, then `Resync(ParamValues)` last: a re-read
///    reads the plugin as it is now, so it must win over any queued value,
///    and a drop's `ParamValues` must follow the values that survived it.
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
    params: std::collections::VecDeque<(i32, f32)>,
    latency: Option<Samples>,
    tail: Option<PluginTail>,
    reloaded: bool,
    io: bool,
    param_titles: bool,
    param_values: bool,
    /// Parameter events dropped since the last [`take_dropped`](Self::take_dropped).
    dropped: u64,
}

impl Unsolicited {
    pub(super) fn new() -> Self {
        Self {
            params: std::collections::VecDeque::with_capacity(UNSOLICITED_PARAMS),
            latency: None,
            tail: None,
            reloaded: false,
            io: false,
            param_titles: false,
            param_values: false,
            dropped: 0,
        }
    }

    /// Queue a parameter event, dropping (and counting) the oldest when full.
    pub(super) fn param(&mut self, index: i32, value: f32) {
        if self.params.len() == UNSOLICITED_PARAMS {
            self.params.pop_front();
            self.dropped += 1;
            self.resync(ResyncKind::ParamValues);
        }
        self.params.push_back((index, value));
    }

    /// The plugin's new latency, replacing one already held.
    pub(super) fn latency(&mut self, samples: Samples) {
        self.latency = Some(samples);
    }

    /// The plugin's new tail, replacing one already held.
    pub(super) fn tail(&mut self, tail: PluginTail) {
        self.tail = Some(tail);
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
            .map(|(index, value)| BridgeEvent::ParameterChanged { index, value });
        let latency = self
            .latency
            .take()
            .map(|samples| BridgeEvent::LatencyChanged { samples });
        let tail = self
            .tail
            .take()
            .map(|tail| BridgeEvent::TailChanged { tail });
        structural
            .into_iter()
            .flatten()
            .chain(params)
            .chain(latency)
            .chain(tail)
            .chain(refresh.into_iter().flatten())
    }

    /// How many parameter events were dropped since the last call.
    pub(super) fn take_dropped(&mut self) -> u64 {
        std::mem::take(&mut self.dropped)
    }
}

/// Which aspect of plugin state the plugin asked the host to re-read.
///
/// Unlike a latency or parameter change, these carry no new value; they say
/// only "your cached view of X is stale."
///
/// This is the wire vocabulary. The [`PluginHandle`] callbacks
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
    /// Maps this signal to its host-facing consequence: a cosmetic
    /// [`PluginRefresh`] (re-read a cached view, no graph edit) or a structural
    /// [`PluginInvalidation`] (rewire and re-plan latency compensation).
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
///
/// Not `Copy`, following [`PluginInvalidation`], which it wraps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResyncClass {
    Refresh(PluginRefresh),
    Invalidate(PluginInvalidation),
}

/// A cosmetic notification that the host's cached view of some plugin state is
/// stale.
///
/// Re-read the state; the audio graph is unaffected. Delivered via
/// [`PluginHandle::on_refresh`](crate::host::handles::PluginHandle::on_refresh).
/// Mirrors CLAP `params.rescan(flags)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginRefresh {
    /// Re-read all parameter values (a preset load / internal write-back).
    ParamValues,
    /// Re-pull the parameter list (titles, units, or flags changed).
    ParamTitles,
}

/// A structural notification that the plugin changed in a way that invalidates
/// the audio graph's plan.
///
/// The host must rewire and re-run latency compensation (PDC). Delivered via
/// [`PluginHandle::on_invalidate`](crate::host::handles::PluginHandle::on_invalidate).
/// Mirrors CLAP `request_restart()` + `audio_ports.rescan()` + `latency.changed()`.
///
/// **Not `Copy`**, because [`Crashed`](Self::Crashed) carries an owned cause.
/// `Clone` is enough: every consumer takes this by value through
/// `Fn(PluginInvalidation)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginInvalidation {
    /// The plugin reported new processing latency. Carries the new value; the
    /// node's own atomic is already updated live, but compensation delays across
    /// the graph only re-plan on a commit.
    Latency {
        /// The plugin's new latency, in **frames**.
        samples: Samples,
    },
    /// The plugin reported a new tail length. Carries the new value; the node's
    /// own cell is already updated live, but an offline render sizes its length
    /// once at the start, so a bounce already in flight keeps the old figure.
    Tail {
        /// The plugin's new tail, which may be bounded, unbounded or unreported.
        tail: PluginTail,
    },
    /// The plugin's bus layout changed — re-read it and rewire the graph.
    Io,
    /// The plugin instance was rebuilt in place; re-plan everything.
    Reloaded,
    /// The plugin died and is not coming back — unwire it.
    ///
    /// Structural rather than cosmetic, and terminal rather than a request to
    /// re-read: every other variant asks the host to *update* its plan, this one
    /// says there is nothing left to plan around. The engine offers no relaunch,
    /// so a host recovers by loading a replacement, carrying whatever state it
    /// captured while the plugin was healthy.
    ///
    /// A host that misses this event is not left guessing — `PluginHandle`'s
    /// status query reports the same cause, latched. See
    /// `BridgeEvent::Crashed` for why both exist.
    Crashed {
        /// Why the plugin died, latched at the detection site.
        cause: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each drained event as a short label, for whole-sequence asserts.
    fn labels(inbox: &mut Unsolicited) -> Vec<String> {
        inbox
            .drain()
            .map(|ev| match ev {
                BridgeEvent::ParameterChanged { index, .. } => format!("p{index}"),
                BridgeEvent::LatencyChanged { samples } => format!("latency {}", samples.get()),
                BridgeEvent::TailChanged { .. } => "tail".to_owned(),
                BridgeEvent::Resync(kind) => format!("{kind:?}"),
                other => panic!("unexpected {other:?}"),
            })
            .collect()
    }

    /// **The inbox hands parameter events over in arrival order and
    /// empties.**
    ///
    /// Mutation (run): `param` uses `push_front` → the parameters come out
    /// reversed → fails. `drain` leaves the latency slot set (`clone` for
    /// `take`) → the second drain is not empty → fails.
    #[test]
    fn the_inbox_keeps_arrival_order_and_empties() {
        let mut inbox = Unsolicited::new();
        for i in 0..3 {
            inbox.param(i, 0.0);
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
    ///
    /// Mutations (run): the latency back in the bounded queue (`latency`
    /// queuing itself as a parameter event, so the flood evicts it) → no
    /// `latency` label → fails. `param` without the
    /// `pop_front` → the inbox grows past its capacity (and reallocates) →
    /// fails. `pop_back` for `pop_front` → the newest held event is lost
    /// instead → fails. The drop not counted → `take_dropped` is 0 → fails.
    /// The drop not raising `ParamValues` → that label is missing → fails.
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
            inbox.param(i, 0.0);
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
    ///
    /// Mutations (run): structural resyncs chained after the parameters
    /// (`params.chain(structural…)`) → `p0` first → fails. `Io` before
    /// `Reloaded` in `structural` → fails. `ParamValues` before
    /// `ParamTitles` in `refresh` → fails. `refresh` chained before the
    /// latency → fails.
    #[test]
    fn structural_resyncs_lead_and_refreshes_trail() {
        let mut inbox = Unsolicited::new();
        inbox.resync(ResyncKind::ParamValues);
        inbox.resync(ResyncKind::ParamTitles);
        inbox.tail(PluginTail::default());
        inbox.latency(Samples::new(16));
        inbox.resync(ResyncKind::Io);
        inbox.resync(ResyncKind::Reloaded);
        inbox.param(0, 0.0);
        inbox.param(1, 0.0);
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
