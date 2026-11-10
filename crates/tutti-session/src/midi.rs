//! MIDI in the session (feature `midi`; design 015 §4.3, §5.2): a live
//! input per target, one clip per target, the hardware input's route rules,
//! and the hardware out the clock feeds, drained by [`Phase::MidiOut`].
//!
//! Every piece is wiring. A clip is a `MidiClipNode`, a live input a
//! `MidiQueueNode`, each the session's own node fed into its target's event
//! input through the session's feeds ([`Graph::set_feed`]); a route rule
//! gives a target ports of the hardware input node. A clip or a live input
//! outlives its target: a host that re-binds a target to a new node
//! re-aims them ([`Midi::retarget`]), and removes one with
//! [`Session::remove`](crate::Session::remove).
//!
//! [`Phase::MidiOut`]: crate::Phase::MidiOut

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tutti_graph::UmpAt;

use tutti_graph::Offset;
use tutti_midi_runtime::{
    MidiClipControls, MidiClipNode, MidiQueueNode, MidiReceiver, MidiSender, RouteRule,
    TimedMidiEvent, compile_routes,
};
use tutti_types::{NodeKey, Samples};
use tutti_ump::{MidiChannel, Ump};

use crate::graph::{Feeder, Graph};
use crate::{Error, EventSource, MidiNodes, Result};

/// Max events drained per chunk into the stack buffer: one frame of clock at
/// any sane tempo is a handful of events, so this is generous headroom.
const DRAIN_CHUNK: usize = 256;

/// A UMP MIDI output and its JR stamp stream: where the session's hardware
/// out goes ([`Midi::set_hardware_out`]).
///
/// The sink carries UMP words to the wire, so JR Timestamps and other
/// MIDI-2-only messages survive, which is the whole reason this type exists.
///
/// The [`JrStream`](tutti_midi_runtime::JrStream) is the whole endpoint's,
/// not one drain's: the hardware out and a host's own mailbox stamp through
/// here, and a per-drain origin would restart each at zero and interleave
/// stamps that walk backwards.
///
/// The sink is erased (`Box<dyn MidiOut>`), so there is one path and no
/// `#[cfg]`: a Linux ALSA seq-UMP output stamps exactly as a CoreMIDI one
/// does.
pub struct UmpOut {
    sink: Box<dyn tutti_midi_runtime::MidiOut>,
    stream: tutti_midi_runtime::JrStream,
    /// Whether to JR-stamp. Separate from "is there an output" on purpose:
    /// stamping off must still *send*, just without timestamps. Folding the
    /// two together would make disabling the stamper silently mute MIDI out.
    stamping: bool,
    /// Scratch for the stamped block, reused across sends: stamping runs
    /// once per block and the result is only borrowed, straight to
    /// `MidiOut::queue`. `JrStream::stamp` clears it on entry, so nothing
    /// carries between blocks.
    stamped: Vec<UmpAt>,
}

impl core::fmt::Debug for UmpOut {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("UmpOut")
            .field("origin_samples", &self.stream.origin_samples())
            .field("stamping", &self.stamping)
            .finish_non_exhaustive()
    }
}

impl UmpOut {
    /// Wraps a UMP output sink, stamping *and clocking* at `sample_rate`,
    /// with stamping off ([`set_stamping`](Self::set_stamping)).
    ///
    /// No group: JR Timestamps are groupless utility messages (M2-104-UM
    /// §2.1.2), so a stamp applies to the whole stream.
    ///
    /// This is a UMP-transport sender, so it owns the JR Clock cadence
    /// (§7.2.2.1). Without it the stamps this type exists to emit are inert:
    /// §7.2.2.3 has a receiver that has seen no JR Clock render messages "as
    /// soon as possible", discarding the timing entirely.
    pub fn new(
        sink: Box<dyn tutti_midi_runtime::MidiOut>,
        sample_rate: impl Into<tutti_types::SampleRate>,
    ) -> Self {
        let sample_rate = sample_rate.into();
        Self {
            sink,
            stream: tutti_midi_runtime::JrStream::new(sample_rate).with_clock(sample_rate),
            stamping: false,
            stamped: Vec::new(),
        }
    }

    /// Turns JR stamping on or off. It gates timestamps, not delivery: with
    /// it off the events still go out, unstamped.
    pub fn set_stamping(&mut self, stamping: bool) {
        self.stamping = stamping;
    }

    /// Whether it JR-stamps.
    #[must_use]
    pub fn is_stamping(&self) -> bool {
        self.stamping
    }

    /// JR-stamp `events` and send each resulting UMP message (the timestamp
    /// prefixes and the events) to the sink. The stream advances its own
    /// origin, so successive blocks stay monotonic whichever drain called.
    ///
    /// Prefer [`send_stamped_span`](Self::send_stamped_span): the JR Clock
    /// cadence advances with the *block*, and this entry point can only
    /// advance by the events it was handed, so a silent stream never
    /// clocks. With stamping off the events go out unchanged.
    pub fn send_stamped(&mut self, events: &[UmpAt]) {
        if !self.stamping {
            self.send_all(events);
            return;
        }
        // Taken out and put back so `stream` and the buffer can be borrowed
        // mutably at once; `stamp` clears it, so the swap carries no state.
        let mut buf = core::mem::take(&mut self.stamped);
        self.stream.stamp(events, &mut buf);
        self.send_all(&buf);
        self.stamped = buf;
    }

    /// Like [`send_stamped`](Self::send_stamped), but tells the stream how
    /// long the block was so the JR Clock cadence keeps running through
    /// silence (§7.2.2.1: the cadence is a property of elapsed time, not of
    /// traffic).
    pub fn send_stamped_span(&mut self, events: &[UmpAt], block_samples: u64) {
        if !self.stamping {
            self.send_all(events);
            return;
        }
        let mut buf = core::mem::take(&mut self.stamped);
        self.stream.stamp_span(events, block_samples, &mut buf);
        self.send_all(&buf);
        self.stamped = buf;
    }

    fn send_all(&mut self, events: &[UmpAt]) {
        // One `queue` for the batch: a backend may pack several UMP
        // messages into one packet.
        self.sink.queue(events);
    }

    /// The absolute sample position the next stamp starts from. For tests
    /// and diagnostics.
    pub fn origin_samples(&self) -> u64 {
        self.stream.origin_samples()
    }
}

/// How many outbound events went nowhere, and whether that has been said
/// aloud.
///
/// With no output connected, a drain has nowhere to send its events, and
/// nothing above debug level would show it: this counts what was lost and
/// warns once.
#[derive(Debug, Default)]
pub struct MidiOutDrops {
    dropped: AtomicU64,
    warned: AtomicBool,
}

impl MidiOutDrops {
    /// Events discarded for want of a connected output.
    pub fn count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Record `n` dropped events, warning the first time only.
    fn record(&self, n: usize) {
        if n == 0 {
            return;
        }
        let total = self.dropped.fetch_add(n as u64, Ordering::Relaxed) + n as u64;
        if !self.warned.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                "MIDI out has events but no output is connected; {total} dropped so far. \
                 Connect one (`Midi::set_hardware_out`)."
            );
        }
    }
}

/// Where drained MIDI-out events go: the open UMP output, else counted as
/// dropped.
///
/// There is no `#[cfg]` here: every backend yields a `Box<dyn MidiOut>`, so
/// a CoreMIDI port and an ALSA port are one static type, and which messages
/// survive is not a property of where the binary was built.
#[derive(Debug)]
pub struct MidiOutRouter<'a> {
    /// The open output, JR-stamping as it sends. `None` when nothing is
    /// connected: a sink that is present but silently discarding would
    /// leave "nothing is coming out" unanswerable.
    pub out: Option<&'a mut UmpOut>,
    /// Where events with nowhere to go are counted. `None` keeps the drop
    /// silent.
    pub drops: Option<&'a MidiOutDrops>,
}

impl MidiOutRouter<'_> {
    /// Routes a batch of drained events to the open output. An event with
    /// no output is dropped (the ring must not back up), counted first.
    pub fn route(&mut self, events: &[UmpAt]) {
        match self.out.as_mut() {
            Some(out) => out.send_stamped(events),
            None => {
                if let Some(drops) = self.drops {
                    drops.record(events.len());
                }
            }
        }
    }
}

/// Drains a [`MidiReceiver`] mailbox fully and routes every event.
///
/// The single output-drain primitive: the graph's hardware out (the clock
/// and whatever else is wired to it) and a host's own outbound mailbox push
/// into a mailbox, and this reads the paired receiver off the audio thread.
///
/// Each drained batch is rebased onto its first event (every stamp less the
/// first's, wrapping), so what the stamper reads is the events' spacing: the
/// hardware out stamps each with its frame on the graph's clock, which means
/// nothing to a stream stamped batch by batch. The batch is the "block" the
/// sink delivers: each rebased stamp becomes an [`Offset`] against the batch's
/// own span (the largest rebased stamp plus one), so an offset is the event's
/// spacing inside its batch.
pub fn drain_receiver_through(receiver: &MidiReceiver, router: &mut MidiOutRouter<'_>) {
    // Both on the stack, as the one buffer was before: this runs every frame.
    let mut buf = [(0u32, Ump([0; 4])); DRAIN_CHUNK];
    let mut batch = [(Offset::ZERO, Ump([0; 4])); DRAIN_CHUNK];
    loop {
        let n = receiver.poll_into(&mut buf);
        if n == 0 {
            break;
        }
        let base = buf[0].0;
        let span = buf[..n]
            .iter()
            .map(|e| e.0.wrapping_sub(base))
            .max()
            .map_or(1, |m| u64::from(m) + 1);
        let mut len = 0;
        for &(stamp, ump) in &buf[..n] {
            // Always `Some`: every rebased stamp is below the span.
            if let Some(at) = Offset::new(stamp.wrapping_sub(base) as usize, Samples(span as usize))
            {
                batch[len] = (at, ump);
                len += 1;
            }
        }
        router.route(&batch[..len]);
        if n < DRAIN_CHUNK {
            break;
        }
    }
}

/// A live input's queue node, fed into its target, and the sender into it:
/// what is queued plays from the next block, on its frame offset clamped
/// into the block. Cheap to clone; derefs to the [`MidiSender`].
#[derive(Clone, Debug)]
pub struct LiveInput {
    /// The queue node.
    pub node: NodeKey,
    /// The sender into it.
    pub sender: MidiSender,
}

impl std::ops::Deref for LiveInput {
    type Target = MidiSender;
    fn deref(&self) -> &MidiSender {
        &self.sender
    }
}

/// An id [`Midi::route`] returns, for [`Midi::unroute`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MidiRouteId(u64);

/// One inbound routing rule: which channel of the hardware input reaches
/// which targets, keyed by `T` (the session's own rules name nodes, a
/// host's its own keys: [`Midi::set_routes`]).
///
/// Several rules may name the same channel; every matching rule's targets
/// receive the event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MidiRouteRule<T = NodeKey> {
    /// Channel filter: `None` matches any channel, `Some(n)` only channel
    /// `n`.
    pub channel: Option<MidiChannel>,
    /// The targets this rule feeds.
    pub targets: Vec<T>,
    /// A disabled rule stays declared but routes nothing: a mute that does
    /// not lose the rule.
    pub enabled: bool,
}

impl<T> MidiRouteRule<T> {
    /// A rule matching every channel. Add targets with [`to`](Self::to).
    pub fn any_channel() -> Self {
        Self {
            channel: None,
            targets: Vec::new(),
            enabled: true,
        }
    }

    /// A rule matching one channel.
    pub fn for_channel(channel: MidiChannel) -> Self {
        Self {
            channel: Some(channel),
            targets: Vec::new(),
            enabled: true,
        }
    }

    /// Adds a destination.
    #[must_use]
    pub fn to(mut self, target: T) -> Self {
        self.targets.push(target);
        self
    }

    /// Declares the rule without arming it.
    #[must_use]
    pub fn disabled(mut self) -> Self {
        self.enabled = false;
        self
    }
}

/// One clip: its controls and the target it feeds.
#[derive(Debug)]
struct Clip {
    controls: MidiClipControls,
    target: NodeKey,
}

/// One live input: its sender and the target it feeds.
struct Live {
    sender: MidiSender,
    target: NodeKey,
}

/// The session's MIDI state.
#[derive(Default)]
pub(crate) struct MidiState {
    clips: BTreeMap<NodeKey, Clip>,
    live: BTreeMap<NodeKey, Live>,
    /// The session's own route rules, and the last id minted.
    rules: BTreeMap<MidiRouteId, MidiRouteRule>,
    fallback: Option<NodeKey>,
    minted: u64,
    /// The sinks the last route compile fed.
    routed: BTreeSet<NodeKey>,
    /// Whether a host's last `set_routes` declared any rule or fallback:
    /// the routes are then the host's, and the session's own are refused.
    host_routes: bool,
    out: Option<UmpOut>,
    stamping: bool,
    drops: MidiOutDrops,
    /// The drop count the last `MidiOut` phase reported, and when.
    drops_seen: u64,
    reported_at: Option<Instant>,
    /// The hardware input's drop total the last `Poll` phase reported, and
    /// when.
    #[cfg(feature = "midi-hardware")]
    input_drops_seen: u64,
    /// The session's subscription to the device list: `Phase::Poll` reads it
    /// and nothing else does.
    #[cfg(feature = "midi-hardware")]
    device_events: Option<tutti_midi_hardware::DeviceEvents>,
    /// Whether the device watch's failure has been reported.
    #[cfg(feature = "midi-hardware")]
    watch_failure_reported: bool,
    #[cfg(feature = "midi-hardware")]
    input_reported_at: Option<Instant>,
}

impl std::fmt::Debug for MidiState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MidiState")
            .field("clips", &self.clips)
            .field("rules", &self.rules)
            .field("fallback", &self.fallback)
            .field("minted", &self.minted)
            .field("routed", &self.routed)
            .field("host_routes", &self.host_routes)
            .field("out", &self.out)
            .field("stamping", &self.stamping)
            .field("drops", &self.drops)
            .field("drops_seen", &self.drops_seen)
            .field("reported_at", &self.reported_at)
            .finish_non_exhaustive()
    }
}

/// How often a rising drop count is reported at most
/// ([`SessionEvent::MidiOutDropped`](crate::SessionEvent::MidiOutDropped)):
/// a clock running with no output drops ticks on every update.
pub const MIDI_OUT_DROP_REPORT: Duration = Duration::from_secs(1);

/// How often a rising count of events the hardware input's rings dropped is
/// reported at most
/// (`SessionEvent::MidiInputDropped`, feature `midi-hardware`):
/// a stalled audio callback with a busy controller drops on every update.
pub const MIDI_INPUT_DROP_REPORT: Duration = Duration::from_secs(1);

impl MidiState {
    /// A state whose `Phase::Poll` reads `device_events`.
    #[cfg(feature = "midi-hardware")]
    pub(crate) fn with_device_events(
        device_events: Option<tutti_midi_hardware::DeviceEvents>,
    ) -> Self {
        Self {
            device_events,
            ..Self::default()
        }
    }

    /// `node` left the graph: a clip or live input of the session's own is
    /// forgotten (the graph took it out of every feed), and a routed sink
    /// is no longer one.
    pub(crate) fn forget_node(&mut self, node: NodeKey) {
        self.clips.remove(&node);
        self.live.remove(&node);
        self.routed.remove(&node);
    }

    /// `Phase::Poll`: reports the devices that came and went, retires a held
    /// input or output whose device vanished, and reports the events the
    /// hardware input's rings dropped when their total rose, at most once per
    /// [`MIDI_INPUT_DROP_REPORT`] of `now`.
    #[cfg(feature = "midi-hardware")]
    pub(crate) fn midi_input_phase(
        &mut self,
        io: Option<&tutti_midi_hardware::MidiSession>,
        now: Instant,
        out: &mut Vec<crate::SessionEvent>,
    ) {
        let Some(io) = io else { return };
        let events = self.device_events.as_ref();
        for event in events.into_iter().flat_map(|events| events.try_iter()) {
            out.push(crate::SessionEvent::MidiDevice(event.clone()));
            if let tutti_midi_hardware::DeviceEvent::Removed(device) = event {
                // Held ones are closed here, not left to fail on a dead
                // handle: the port goes and the lists agree.
                if io.disconnect_input(&device.id) {
                    out.push(crate::SessionEvent::MidiInputLost(device.clone()));
                }
                if io.output_device().as_ref() == Some(&device.id) && io.disconnect_output() {
                    out.push(crate::SessionEvent::MidiOutputLost(device));
                }
            }
        }
        if !self.watch_failure_reported
            && let Some(why) = events.and_then(tutti_midi_hardware::DeviceEvents::failure)
        {
            self.watch_failure_reported = true;
            out.push(crate::SessionEvent::MidiWatchFailed(why));
        }
        let total = io.ports().dropped_events();
        let due = self
            .input_reported_at
            .is_none_or(|at| now.saturating_duration_since(at) >= MIDI_INPUT_DROP_REPORT);
        if total != self.input_drops_seen && due {
            self.input_drops_seen = total;
            self.input_reported_at = Some(now);
            out.push(crate::SessionEvent::MidiInputDropped(total));
        }
    }

    /// `Phase::MidiOut`: drains the hardware out to the output, and reports
    /// the drops when their count rose, at most once per
    /// [`MIDI_OUT_DROP_REPORT`] of `now`.
    pub(crate) fn midi_out_phase(
        &mut self,
        nodes: Option<&MidiNodes>,
        now: Instant,
        out: &mut Vec<crate::SessionEvent>,
    ) {
        if let Some(nodes) = nodes {
            let mut router = MidiOutRouter {
                out: self.out.as_mut(),
                drops: Some(&self.drops),
            };
            drain_receiver_through(nodes.out_controls.receiver(), &mut router);
        }
        let count = self.drops.count();
        let due = self
            .reported_at
            .is_none_or(|at| now.saturating_duration_since(at) >= MIDI_OUT_DROP_REPORT);
        if count != self.drops_seen && due {
            self.drops_seen = count;
            self.reported_at = Some(now);
            out.push(crate::SessionEvent::MidiOutDropped(count));
        }
    }
}

/// The session's MIDI (design 015 §4.3): live inputs, clips, the hardware
/// input's routes and the hardware out. [`Session::midi`](crate::Session::midi)
/// hands it out; what it edits goes out with the next update's commit.
#[derive(Debug)]
pub struct Midi<'a> {
    pub(crate) graph: &'a mut Graph,
    pub(crate) state: &'a mut MidiState,
    pub(crate) nodes: Option<&'a MidiNodes>,
}

impl Midi<'_> {
    /// `target`, if it takes MIDI: it is in the graph with an event input.
    fn takes_midi(&self, target: NodeKey) -> Result<()> {
        if self.graph.contains(target) && self.graph.node_event_inputs(target) > 0 {
            Ok(())
        } else {
            Err(Error::NoEventInput(target))
        }
    }

    // --- Live input ---

    /// A live input for `target` (a synth, a plugin): a `MidiQueueNode` fed
    /// into its event input, for a keyboard, a preview or an all-notes-off
    /// the host sends. The one it has already when there is one.
    ///
    /// # Errors
    ///
    /// [`Error::NoEventInput`] when `target` is not in the graph or takes
    /// no MIDI.
    pub fn live_input(&mut self, target: NodeKey) -> Result<LiveInput> {
        self.takes_midi(target)?;
        if let Some((node, live)) = self.fed_by(target, Feeder::LiveInput, &self.state.live) {
            return Ok(LiveInput {
                node,
                sender: live.sender.clone(),
            });
        }
        let (node, sender) = self.graph.insert(MidiQueueNode::new());
        self.graph
            .set_feed(target, Feeder::LiveInput, vec![node.into()]);
        self.state.live.insert(
            node,
            Live {
                sender: sender.clone(),
                target,
            },
        );
        Ok(LiveInput { node, sender })
    }

    // --- Clips ---

    /// Plays `events` on `target` through its clip: the one it has, its
    /// events replaced in place (which ends the notes the old events left
    /// sounding), or a new `MidiClipNode` fed into its event input. Returns
    /// the clip's node.
    ///
    /// The events are absolute-beat positioned, in any order; the clip
    /// reads its block's `Env`, so a note lands on its frame through seeks
    /// and loop wraps, and an export forks it with the target.
    ///
    /// # Errors
    ///
    /// [`Error::NoEventInput`] when `target` is not in the graph or takes
    /// no MIDI.
    pub fn play_clip(
        &mut self,
        target: NodeKey,
        events: impl IntoIterator<Item = TimedMidiEvent>,
    ) -> Result<NodeKey> {
        self.takes_midi(target)?;
        let clip = match self.clip(target) {
            Some(clip) => {
                self.state.clips[&clip].controls.set_events(events);
                clip
            }
            None => {
                let (clip, controls) = self.graph.add(MidiClipNode::new(events));
                self.state.clips.insert(clip, Clip { controls, target });
                clip
            }
        };
        // Set on every call: `set_feed` leaves an unchanged feed (and the
        // edit flag) alone.
        self.graph
            .set_feed(target, Feeder::Sequencer, vec![clip.into()]);
        Ok(clip)
    }

    /// Empties `target`'s clip, ending the notes it left sounding. The clip
    /// stays, to play the next [`play_clip`](Self::play_clip). `false`
    /// when `target` has none.
    pub fn stop_clip(&mut self, target: NodeKey) -> bool {
        match self.clip(target) {
            Some(clip) => {
                self.state.clips[&clip].controls.clear();
                true
            }
            None => false,
        }
    }

    /// `target`'s clip node, if it has one.
    pub fn clip(&self, target: NodeKey) -> Option<NodeKey> {
        self.fed_by(target, Feeder::Sequencer, &self.state.clips)
            .map(|(clip, _)| clip)
    }

    /// The node of `ours` that `feeder` feeds `target` with: the feed is
    /// the truth (a host's `Graph::move_feeds` moves it), each entry's
    /// `target` only what [`retarget`](Self::retarget) unfeeds.
    fn fed_by<'s, V>(
        &self,
        target: NodeKey,
        feeder: Feeder,
        ours: &'s BTreeMap<NodeKey, V>,
    ) -> Option<(NodeKey, &'s V)> {
        self.graph
            .feed(target, feeder)
            .iter()
            .find_map(|s| ours.get_key_value(&s.node))
            .map(|(&node, v)| (node, v))
    }

    /// Feeds clip or live input `node` (one this session made) into
    /// `target` in place of the target it fed: a host whose target was
    /// re-bound to a new node keeps its clip and its keyboard. A target
    /// takes one clip and one live input: one it had is no longer fed.
    ///
    /// # Errors
    ///
    /// [`Error::NoEventInput`] when `target` is not in the graph or takes
    /// no MIDI; [`Error::NotSessionMidi`] when `node` is not a clip or live
    /// input of this session's.
    pub fn retarget(&mut self, node: NodeKey, target: NodeKey) -> Result<()> {
        self.takes_midi(target)?;
        let (old, feeder) = if let Some(clip) = self.state.clips.get_mut(&node) {
            (
                std::mem::replace(&mut clip.target, target),
                Feeder::Sequencer,
            )
        } else if let Some(live) = self.state.live.get_mut(&node) {
            (
                std::mem::replace(&mut live.target, target),
                Feeder::LiveInput,
            )
        } else {
            return Err(Error::NotSessionMidi(node));
        };
        if old != target && self.graph.feed(old, feeder).contains(&node.into()) {
            self.graph.set_feed(old, feeder, Vec::new());
        }
        self.graph.set_feed(target, feeder, vec![node.into()]);
        Ok(())
    }

    // --- Routes ---

    /// Adds a rule routing the hardware input to its targets. Returns its
    /// id, for [`unroute`](Self::unroute).
    ///
    /// The routes have one declarer: these rules and
    /// [`set_route_fallback`](Self::set_route_fallback), or a host's
    /// [`set_routes`](Self::set_routes).
    ///
    /// # Errors
    ///
    /// [`Error::NoMidiInput`] on a session with no MIDI input node (one
    /// built detached); [`Error::TwoRouteDeclarers`] while a host's
    /// `set_routes` declares the routes.
    pub fn route(&mut self, rule: MidiRouteRule) -> Result<MidiRouteId> {
        let input = self.input()?;
        self.own_may_declare()?;
        self.state.minted += 1;
        let id = MidiRouteId(self.state.minted);
        self.state.rules.insert(id, rule);
        self.compile_own(input);
        Ok(id)
    }

    /// Removes a rule [`route`](Self::route) added. `false` when there is
    /// none.
    pub fn unroute(&mut self, id: MidiRouteId) -> bool {
        let had = self.state.rules.remove(&id).is_some();
        if had && let Ok(input) = self.input() {
            self.compile_own(input);
        }
        had
    }

    /// Where an event matching no rule goes, if anywhere: every port no
    /// rule covers. `None` (the default) drops unmatched events.
    ///
    /// # Errors
    ///
    /// [`Error::NoMidiInput`] on a session with no MIDI input node;
    /// [`Error::TwoRouteDeclarers`] while a host's
    /// [`set_routes`](Self::set_routes) declares the routes.
    pub fn set_route_fallback(&mut self, fallback: Option<NodeKey>) -> Result<()> {
        let input = self.input()?;
        self.own_may_declare()?;
        self.state.fallback = fallback;
        self.compile_own(input);
        Ok(())
    }

    fn own_may_declare(&self) -> Result<()> {
        if self.state.host_routes {
            Err(Error::TwoRouteDeclarers("a host's `set_routes`"))
        } else {
            Ok(())
        }
    }

    fn input(&self) -> Result<NodeKey> {
        self.nodes.map(|n| n.input).ok_or(Error::NoMidiInput)
    }

    fn compile_own(&mut self, input: NodeKey) {
        let rules = std::mem::take(&mut self.state.rules);
        let fallback = self.state.fallback;
        self.compile_routes(input, rules.values(), fallback, Some);
        self.state.rules = rules;
    }

    /// Routes `input` (a `MidiInputNode`: one event output per channel and
    /// one for channelless messages) by `rules`, keyed by the host's own
    /// `T` and resolved to nodes through `resolve`, replacing every route
    /// before (the session's own rules included).
    ///
    /// A channel rule gives its targets that channel's port and the
    /// channelless one, an any-channel rule all of them, several rules the
    /// union (`tutti_midi_runtime::compile_routes`); the fallback takes
    /// every port no armed rule covers. A target `resolve` maps to no node,
    /// or to one not in the graph, takes nothing (and still covers its
    /// rule's ports); one fed before and not now is unwired.
    ///
    /// The routes have one declarer: while this declares any rule or a
    /// fallback, the session's own [`route`](Self::route) and
    /// [`set_route_fallback`](Self::set_route_fallback) are refused; a call
    /// with neither hands the routes back.
    ///
    /// # Errors
    ///
    /// [`Error::TwoRouteDeclarers`] while the session's own rules or
    /// fallback are declared ([`unroute`](Self::unroute) them first).
    /// Nothing is changed.
    pub fn set_routes<'r, T: Copy + Ord + 'r>(
        &mut self,
        input: NodeKey,
        rules: impl IntoIterator<Item = &'r MidiRouteRule<T>>,
        fallback: Option<T>,
        resolve: impl Fn(T) -> Option<NodeKey>,
    ) -> Result<()> {
        if !self.state.rules.is_empty() || self.state.fallback.is_some() {
            return Err(Error::TwoRouteDeclarers("the session's own `route`"));
        }
        let rules: Vec<&MidiRouteRule<T>> = rules.into_iter().collect();
        self.state.host_routes = !rules.is_empty() || fallback.is_some();
        self.compile_routes(input, rules, fallback, resolve);
        Ok(())
    }

    /// The compile both declarers share: `rules` onto `input`'s ports,
    /// replacing every route before.
    fn compile_routes<'r, T: Copy + Ord + 'r>(
        &mut self,
        input: NodeKey,
        rules: impl IntoIterator<Item = &'r MidiRouteRule<T>>,
        fallback: Option<T>,
        resolve: impl Fn(T) -> Option<NodeKey>,
    ) {
        let ports = compile_routes(
            rules.into_iter().filter(|r| r.enabled).map(|r| RouteRule {
                channel: r.channel,
                targets: &r.targets,
            }),
            fallback,
        );
        let mut wanted: BTreeMap<NodeKey, Vec<EventSource>> = BTreeMap::new();
        for (target, ports) in ports {
            let Some(sink) = resolve(target).filter(|&n| self.graph.contains(n)) else {
                continue;
            };
            wanted.entry(sink).or_default().extend(
                ports
                    .into_iter()
                    .filter_map(|p| u16::try_from(p).ok())
                    .map(|p| EventSource::new(input, p)),
            );
        }
        for &fed in &self.state.routed {
            if !wanted.contains_key(&fed) {
                self.graph.set_feed(fed, Feeder::Routes, Vec::new());
            }
        }
        self.state.routed = wanted.keys().copied().collect();
        for (sink, mut sources) in wanted {
            sources.sort_by_key(|s| (s.node, s.port));
            sources.dedup();
            self.graph.set_feed(sink, Feeder::Routes, sources);
        }
    }

    // --- Hardware out ---

    /// The output the hardware out (the clock, and whatever else is wired
    /// to its node) drains to at each update's [`MidiOut`](crate::Phase::MidiOut)
    /// phase. `None`: its events are counted as dropped
    /// ([`SessionEvent::MidiOutDropped`](crate::SessionEvent::MidiOutDropped)).
    /// The output stamps as [`set_jr_stamping`](Self::set_jr_stamping) says.
    pub fn set_hardware_out(&mut self, out: Option<UmpOut>) {
        self.state.out = out.map(|mut out| {
            out.set_stamping(self.state.stamping);
            out
        });
    }

    /// Whether the output JR-stamps (default off). Gates timestamps, not
    /// delivery.
    pub fn set_jr_stamping(&mut self, stamping: bool) {
        self.state.stamping = stamping;
        if let Some(out) = &mut self.state.out {
            out.set_stamping(stamping);
        }
    }

    /// The router onto the output: what a host writes its own outbound
    /// MIDI through (a MIDI-CI reply, a track's MIDI out), stamped and
    /// counted as the hardware out is.
    pub fn hardware_out(&mut self) -> MidiOutRouter<'_> {
        MidiOutRouter {
            out: self.state.out.as_mut(),
            drops: Some(&self.state.drops),
        }
    }

    /// Drains a host's own outbound mailbox through
    /// [`hardware_out`](Self::hardware_out).
    pub fn drain_out(&mut self, receiver: &MidiReceiver) {
        drain_receiver_through(receiver, &mut self.hardware_out());
    }

    /// Events dropped for want of an output, counted.
    pub fn out_drops(&self) -> u64 {
        self.state.drops.count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use tutti_ump::MidiGroup;

    /// Counts what reached the wire. Stands in for a real OS endpoint so
    /// these tests run on every platform.
    struct CountingSink(Arc<AtomicUsize>);
    impl tutti_midi_runtime::MidiOut for CountingSink {
        fn queue(&self, events: &[UmpAt]) -> usize {
            self.0.fetch_add(events.len(), Ordering::SeqCst);
            events.len()
        }
    }

    fn out_with_counter(stamping: bool) -> (UmpOut, Arc<AtomicUsize>) {
        let count = Arc::new(AtomicUsize::new(0));
        let mut out = UmpOut::new(Box::new(CountingSink(count.clone())), 48_000.0);
        out.set_stamping(stamping);
        (out, count)
    }

    fn note(offset: u32) -> UmpAt {
        (
            Offset::new(offset as usize, Samples(512)).unwrap(),
            tutti_fixtures::midi::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000),
        )
    }

    /// Both drains stamp through one stream, so the origin keeps climbing
    /// across them.
    #[test]
    fn both_producers_advance_one_origin() {
        let (mut out, _) = out_with_counter(true);

        out.send_stamped(&[note(511)]);
        assert_eq!(out.origin_samples(), 512, "one 512-frame block");

        out.send_stamped(&[note(511)]);
        assert_eq!(
            out.origin_samples(),
            1024,
            "the second producer continues from the first, it does not restart"
        );
    }

    /// **Stamping off must still send.** Stamping gates timestamps, not
    /// delivery: there is no second, unstamped transport to fall through
    /// to.
    #[test]
    fn stamping_disabled_still_delivers_events() {
        let (mut out, count) = out_with_counter(false);
        out.send_stamped(&[note(0), note(1)]);
        assert_eq!(
            count.load(Ordering::SeqCst),
            2,
            "events must reach the sink with stamping off"
        );
        assert_eq!(
            out.origin_samples(),
            0,
            "and the stream must not advance when it did not stamp"
        );
    }

    /// With stamping on, more messages reach the wire than were handed in
    /// (the JR Timestamp prefixes): what makes the test above non-vacuous.
    #[test]
    fn stamping_enabled_adds_timestamp_messages() {
        let (mut out, count) = out_with_counter(true);
        out.send_stamped(&[note(0)]);
        assert!(
            count.load(Ordering::SeqCst) > 1,
            "a stamped send carries its JR Timestamp prefix too, got {}",
            count.load(Ordering::SeqCst)
        );
    }

    /// A router with no output counts the events rather than losing them
    /// silently.
    #[test]
    fn routing_with_no_output_counts_the_drops() {
        let drops = MidiOutDrops::default();
        let mut router = MidiOutRouter {
            out: None,
            drops: Some(&drops),
        };
        router.route(&[note(0), note(1), note(2)]);
        assert_eq!(drops.count(), 3);
    }

    /// And a router *with* an output does not count drops.
    #[test]
    fn routing_to_an_open_output_drops_nothing() {
        let (mut out, count) = out_with_counter(false);
        let drops = MidiOutDrops::default();
        let mut router = MidiOutRouter {
            out: Some(&mut out),
            drops: Some(&drops),
        };
        router.route(&[note(0), note(1)]);
        assert_eq!(count.load(Ordering::SeqCst), 2, "both reached the sink");
        assert_eq!(drops.count(), 0, "nothing was dropped");
    }
}
