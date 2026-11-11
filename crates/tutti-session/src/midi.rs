//! MIDI in the session (feature `midi`; design 015 §4.3, §5.2): a live
//! input per target, one clip per target, the hardware input's route rules,
//! and the hardware out, an event tap on the clock drained by
//! [`Phase::MidiOut`].
//!
//! Every piece is wiring. A clip is a `MidiClipNode`, a live input a
//! an event inlet, each the session's own node fed into its target's event
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

use tutti_graph::{EventTapId, EventTapReader, NodeKey, Offset, TapEvent};
use tutti_midi_runtime::{
    MidiClipControls, MidiClipNode, MidiInputPorts, MidiReceiver, RouteRule, TimedMidiEvent,
    compile_routes,
};
use tutti_types::{At, Frame, Moment, Samples};
use tutti_ump::{MidiChannel, MidiGroup, NoteNumber, Ump};

use crate::SendError;
use crate::graph::{Feeder, Graph};
use crate::instrument::SharedKeys;
use crate::{Error, EventSource, MidiNodes, Result};

/// Max events drained per chunk into the stack buffer: one frame of clock at
/// any sane tempo is a handful of events, so this is generous headroom.
const DRAIN_CHUNK: usize = 256;

/// How many hardware MIDI devices may be connected at once by default
/// ([`SessionBuilder::midi_devices`](crate::SessionBuilder::midi_devices)).
pub const MIDI_INPUT_DEVICES: usize = 8;

/// Events one connected device's inlet holds: the ring its thread pushes into,
/// and as many waiting for their frame.
#[cfg(feature = "midi-hardware")]
pub(crate) const MIDI_INPUT_INLET_CAPACITY: usize = 2048;

/// How long after an event arrives the hardware inlets place it: one block of
/// the device's callback (its `quantum` when known, else the largest block), so
/// an event that arrived between two blocks is on a frame of the next and is
/// not late (doc 017 stage E5). The spacing between events is kept; the
/// whole is that much later than the OS delivered it.
#[cfg(feature = "midi-hardware")]
pub(crate) fn input_lead(block: Samples, rate: tutti_types::SampleRate) -> Duration {
    Duration::from_secs_f64(block.to_seconds(rate).get())
}

/// The engine's block clock as the device threads read it
/// ([`tutti_midi_runtime::ArrivalClock`]). The engine stamps with
/// [`tutti_rt::monotonic_ns`], the OS's monotonic clock; the backends stamp
/// arrivals with `Instant`, the same clock (`CLOCK_MONOTONIC` on Linux,
/// `mach_absolute_time` on macOS, `QueryPerformanceCounter` on Windows). One
/// pairing of the two, read once per process, relates them, so there is one
/// timebase end to end and the conversion is an offset, never a second clock.
#[cfg(feature = "midi-hardware")]
pub(crate) fn arrival_clock(
    clock: tutti_engine::BlockClockReader,
) -> tutti_midi_runtime::ArrivalClock {
    use std::sync::OnceLock;
    static PAIRING: OnceLock<(Instant, i64)> = OnceLock::new();
    let (instant, ns) = *PAIRING.get_or_init(|| (Instant::now(), tutti_rt::monotonic_ns()));
    std::sync::Arc::new(move || {
        let reading = clock.read()?;
        let delta = reading.os_ns.saturating_sub(ns);
        let span = Duration::from_nanos(delta.unsigned_abs());
        let os = if delta >= 0 {
            instant.checked_add(span)?
        } else {
            instant.checked_sub(span)?
        };
        Some((os, Moment::new(reading.frame, reading.rate)))
    })
}

/// The hardware out's tap, in events: the ring the clock's output is copied
/// into between two updates (a note-off is kept up to
/// [`note_off_room`](tutti_graph::note_off_room) of it).
pub(crate) const MIDI_OUT_TAP_CAPACITY: usize = 1024;

/// A UMP MIDI output and its JR stamp stream: where the session's hardware
/// out goes ([`Midi::set_hardware_out`]).
///
/// The sink carries UMP words to the wire, so JR Timestamps and other
/// MIDI-2-only messages survive, which is the whole reason this type exists:
/// a hardware sink puts them in its device's wire form (`OpenOutput`), and
/// the session turns stamping on only when the device receives them.
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
    /// Scratch for a batch of frames rebased onto offsets, reused likewise.
    rebased: Vec<UmpAt>,
}

impl core::fmt::Debug for UmpOut {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("UmpOut")
            .field("origin", &self.stream.origin())
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
            rebased: Vec::new(),
        }
    }

    /// Turns JR stamping on or off. It gates timestamps, not delivery: with
    /// it off the events still go out, unstamped. For an output that is a
    /// hardware device's the session sets it each update from the device's
    /// configuration (`Midi::set_hardware_out`).
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
    pub fn send_stamped_span(&mut self, events: &[UmpAt], block: Samples) {
        if !self.stamping {
            self.send_all(events);
            return;
        }
        let mut buf = core::mem::take(&mut self.stamped);
        self.stream.stamp_span(events, block, &mut buf);
        self.send_all(&buf);
        self.stamped = buf;
    }

    /// Sends a batch of events, each on its frame on the graph's clock, as
    /// [`send_stamped_span`](Self::send_stamped_span) sends a block: the batch
    /// is the block, its first frame (the earliest) is its start, and its span
    /// runs to the last frame inclusive, so what the stamper reads is the
    /// events' spacing and the JR Clock advances by the span. An empty batch
    /// sends nothing.
    pub fn send_frames(&mut self, events: &[(Frame, Ump)]) {
        let (Some(first), Some(last)) = (
            events.iter().map(|e| e.0).min(),
            events.iter().map(|e| e.0).max(),
        ) else {
            return;
        };
        let span = Samples((last.get() - first.get()) as usize + 1);
        let mut batch = core::mem::take(&mut self.rebased);
        batch.clear();
        batch.extend(events.iter().filter_map(|&(frame, ump)| {
            // Always `Some`: every rebased frame is below the span.
            Offset::new((frame.get() - first.get()) as usize, span).map(|at| (at, ump))
        }));
        self.send_stamped(&batch);
        self.rebased = batch;
    }

    fn send_all(&mut self, events: &[UmpAt]) {
        // One `queue` for the batch: a backend may pack several UMP
        // messages into one packet.
        self.sink.queue(events);
    }

    /// The absolute frame the next stamp starts from. For tests and
    /// diagnostics.
    pub fn origin(&self) -> Frame {
        self.stream.origin()
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

    /// Record `n` events the hardware out's tap refused for want of room: a
    /// count, with no warning of its own (there is an output to blame).
    fn record_refused(&self, n: u64) {
        self.dropped.fetch_add(n, Ordering::Relaxed);
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
    /// Routes a batch of drained events, each on its frame on the graph's
    /// clock, to the open output ([`UmpOut::send_frames`]). An event with
    /// no output is dropped (the ring must not back up), counted first.
    pub fn route(&mut self, events: &[(Frame, Ump)]) {
        match self.out.as_mut() {
            Some(out) => out.send_frames(events),
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
/// For a host's own outbound mailbox (a MIDI-CI reply, a track's MIDI out);
/// the graph's hardware out is a tap and reads its frames directly. A
/// mailbox stamp is a raw `u32` whose meaning is the host's, so each drained
/// batch is read as frames from its first event (every stamp less the
/// first's, wrapping): the spacing, which is what the router needs.
pub fn drain_receiver_through(receiver: &MidiReceiver, router: &mut MidiOutRouter<'_>) {
    // Both on the stack, as the one buffer was before: this runs every frame.
    let mut buf = [(0u32, Ump([0; 4])); DRAIN_CHUNK];
    let mut batch = [(Frame::ZERO, Ump([0; 4])); DRAIN_CHUNK];
    loop {
        let n = receiver.poll_into(&mut buf);
        if n == 0 {
            break;
        }
        let base = buf[0].0;
        for (slot, &(stamp, ump)) in batch.iter_mut().zip(&buf[..n]) {
            *slot = (Frame::new(u64::from(stamp.wrapping_sub(base))), ump);
        }
        router.route(&batch[..n]);
        if n < DRAIN_CHUNK {
            break;
        }
    }
}

/// A live input's event inlet, fed into its target, and the handle that
/// sends into it: what is sent plays from the next block
/// ([`At::NextBlock`]) or on a frame of the engine's clock, held until the
/// block that contains it. Cheap to clone; every clone shares the one writer
/// behind a lock only control threads take.
#[derive(Clone, Debug)]
pub struct LiveInput {
    /// The inlet's node.
    pub node: NodeKey,
    keys: SharedKeys,
}

impl LiveInput {
    /// The engine's clock where the inlet's last block ended
    /// ([`InstrumentHandle::clock`](crate::InstrumentHandle::clock) reads the
    /// same); `None` before it is prepared.
    #[must_use]
    pub fn clock(&self) -> Option<Moment> {
        self.keys.clock()
    }

    /// Sends `ump` at `at` ([`At::Frame`] or [`At::NextBlock`]).
    ///
    /// # Errors
    ///
    /// [`SendError::Beat`] for [`At::Beat`]; [`SendError::Full`] when the
    /// ring is full (a note-off dropped there leaves a note stuck).
    pub fn send(&self, at: At, ump: Ump) -> std::result::Result<(), SendError> {
        self.keys.send(at, ump)
    }

    /// Sends `ump` at the instant `moment` names, whatever the rate has become.
    ///
    /// # Errors
    ///
    /// [`SendError::Full`].
    pub fn send_at(&self, moment: Moment, ump: Ump) -> std::result::Result<(), SendError> {
        self.keys.send_at(moment, ump)
    }

    /// A MIDI 2.0 Note On at `velocity` (16 bits) on group 1. A velocity of 0
    /// is a Note On (M2-104 §7.4.2).
    ///
    /// # Errors
    ///
    /// As [`send`](Self::send).
    pub fn note_on(
        &self,
        at: At,
        channel: MidiChannel,
        note: NoteNumber,
        velocity: u16,
    ) -> std::result::Result<(), SendError> {
        self.keys.note_on(at, channel, note, velocity)
    }

    /// A MIDI 2.0 Note Off at release velocity 0 on group 1.
    ///
    /// # Errors
    ///
    /// As [`send`](Self::send).
    pub fn note_off(
        &self,
        at: At,
        channel: MidiChannel,
        note: NoteNumber,
    ) -> std::result::Result<(), SendError> {
        self.keys.note_off(at, channel, note)
    }

    /// How many events the inlet dropped from its waiting list for want of
    /// room ([`InstrumentHandle::dropped`](crate::InstrumentHandle::dropped)).
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.keys.dropped()
    }
}

/// An id [`Midi::route`] returns, for [`Midi::unroute`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MidiRouteId(u64);

/// One inbound routing rule: which Group and channel of the hardware input
/// reach which targets, keyed by `T` (the session's own rules name nodes, a
/// host's its own keys: [`Midi::set_routes`]).
///
/// A channel is the channel of one Group (M2-104 Appendix B.1): the rules
/// below match the first Group unless told otherwise
/// ([`in_group`](MidiRouteRule::in_group),
/// [`in_any_group`](MidiRouteRule::in_any_group)).
///
/// Several rules may name the same channel; every matching rule's targets
/// receive the event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MidiRouteRule<T = NodeKey> {
    /// Group filter: `None` matches every Group the input hears, `Some(g)`
    /// only Group `g`.
    pub group: Option<MidiGroup>,
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
            group: Some(MidiGroup::FIRST),
            channel: None,
            targets: Vec::new(),
            enabled: true,
        }
    }

    /// A rule matching one channel.
    pub fn for_channel(channel: MidiChannel) -> Self {
        Self {
            group: Some(MidiGroup::FIRST),
            channel: Some(channel),
            targets: Vec::new(),
            enabled: true,
        }
    }

    /// Matches `group` instead of the first.
    #[must_use]
    pub fn in_group(mut self, group: MidiGroup) -> Self {
        self.group = Some(group);
        self
    }

    /// Matches the channel in every Group the input hears.
    #[must_use]
    pub fn in_any_group(mut self) -> Self {
        self.group = None;
        self
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

/// One live input: its handle and the target it feeds.
struct Live {
    keys: SharedKeys,
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
    /// The hardware out's tap on the clock, which `Phase::MidiOut` drains;
    /// `None` when the session has no engine MIDI (or the clock left).
    hardware_tap: Option<EventTapReader>,
    drops: MidiOutDrops,
    /// The drop count the last `MidiOut` phase reported, and when.
    drops_seen: u64,
    reported_at: Option<Instant>,
    /// The hardware input's drop total the last `Poll` phase reported, and
    /// when.
    #[cfg(feature = "midi-hardware")]
    input_drops_seen: u64,
    /// The inlet node each device slot of the ingest node is fed from, `None`
    /// where no device holds the slot.
    #[cfg(feature = "midi-hardware")]
    inlet_keys: Vec<Option<NodeKey>>,
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

/// How often a rising count of events the hardware inputs' inlets dropped is
/// reported at most
/// (`SessionEvent::MidiInputDropped`, feature `midi-hardware`):
/// a stalled audio callback with a busy controller drops on every update.
pub const MIDI_INPUT_DROP_REPORT: Duration = Duration::from_secs(1);

impl MidiState {
    /// A state whose hardware out drains `tap`.
    pub(crate) fn with_hardware_tap(tap: Option<EventTapReader>) -> Self {
        Self {
            hardware_tap: tap,
            ..Self::default()
        }
    }

    /// A state whose `Phase::Poll` reads `device_events` and whose hardware
    /// out drains `tap`.
    #[cfg(feature = "midi-hardware")]
    pub(crate) fn with_device_events(
        device_events: Option<tutti_midi_hardware::DeviceEvents>,
        tap: Option<EventTapReader>,
    ) -> Self {
        Self {
            device_events,
            ..Self::with_hardware_tap(tap)
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

    /// `Phase::Poll`: steps the UMP Stream negotiation of each held input and
    /// reports those that settled (`SessionEvent::MidiStream`), reports the
    /// devices that came and went, retires a held
    /// input or output whose device vanished, wires the event inlets of the
    /// devices that connected to their slots of the ingest node (and takes out
    /// those of devices that closed), and reports the events the hardware
    /// inputs' inlets dropped when their total rose, at most once per
    /// [`MIDI_INPUT_DROP_REPORT`] of `now`. Whether the graph was edited.
    #[cfg(feature = "midi-hardware")]
    pub(crate) fn midi_input_phase(
        &mut self,
        io: Option<&tutti_midi_hardware::MidiSession>,
        nodes: Option<&MidiNodes>,
        graph: &mut Graph,
        now: Instant,
        out: &mut Vec<crate::SessionEvent>,
    ) -> bool {
        let Some(io) = io else { return false };
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
        // The devices' UMP Stream negotiations take their step at `now`: the
        // session's control tick is their timer.
        for (device, stream) in io.poll(now) {
            out.push(crate::SessionEvent::MidiStream {
                device,
                stream: Box::new(stream),
            });
        }
        // What the poll settled or changed is heard by the output at once: the
        // `MidiOut` phase that drains next stamps by it.
        follow_output_config(self.out.as_mut(), Some(io));
        let wired = wire_inlets(&mut self.inlet_keys, io.ports().inlets(), nodes, graph);
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
        wired
    }

    /// Reads the hardware out's tap dry and routes what it held to the
    /// output, each event on its frame, and counts what the tap refused for
    /// want of room ([`EventTapReader::take_lost`]) among the drops.
    pub(crate) fn drain_tap(&mut self) {
        let Some(tap) = self.hardware_tap.as_mut() else {
            return;
        };
        let mut events = [TapEvent {
            frame: Frame::ZERO,
            ump: Ump([0; 4]),
        }; DRAIN_CHUNK];
        let mut batch = [(Frame::ZERO, Ump([0; 4])); DRAIN_CHUNK];
        let mut router = MidiOutRouter {
            out: self.out.as_mut(),
            drops: Some(&self.drops),
        };
        loop {
            let n = tap.pop_into(&mut events);
            for (slot, e) in batch.iter_mut().zip(&events[..n]) {
                *slot = (e.frame, e.ump);
            }
            if n > 0 {
                router.route(&batch[..n]);
            }
            if n < DRAIN_CHUNK {
                break;
            }
        }
        self.drops.record_refused(tap.take_lost());
    }

    /// The hardware out's tap is on one scale of frames, the rate it was
    /// opened at: when the graph has been re-prepared at another, drains the
    /// old reader, reopens that one tap (and no other: a caller's own are the
    /// caller's) and takes the new reader. Whether it reopened; the graph is
    /// then edited and the next commit compiles the new tap.
    pub(crate) fn follow_rate(&mut self, graph: &mut Graph) -> bool {
        let Some(tap) = &self.hardware_tap else {
            return false;
        };
        if tap.rate() == graph.prepared().sample_rate() {
            return false;
        }
        let id: EventTapId = tap.id();
        self.drain_tap();
        self.hardware_tap = graph.reopen_event_tap(id);
        true
    }

    /// Makes the hardware out stamp as its device's configuration says: when
    /// the session holds an output device, the output JR-stamps iff it receives
    /// JR Timestamps. A session with no output device leaves the output as it
    /// is.
    #[cfg(feature = "midi-hardware")]
    pub(crate) fn follow_output_config(&mut self, io: Option<&tutti_midi_hardware::MidiSession>) {
        follow_output_config(self.out.as_mut(), io);
    }

    /// `Phase::MidiOut`: drains the hardware out to the output, and reports
    /// the drops when their count rose, at most once per
    /// [`MIDI_OUT_DROP_REPORT`] of `now`.
    pub(crate) fn midi_out_phase(
        &mut self,
        graph: &mut Graph,
        now: Instant,
        out: &mut Vec<crate::SessionEvent>,
    ) {
        // A graph re-rated outside the device hook (`Graph::set_sample_rate`)
        // is followed here, before this update's events are read.
        self.follow_rate(graph);
        self.drain_tap();
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

/// Sets `out` to stamp iff the session's output device receives JR Timestamps.
#[cfg(feature = "midi-hardware")]
fn follow_output_config(out: Option<&mut UmpOut>, io: Option<&tutti_midi_hardware::MidiSession>) {
    if let (Some(out), Some(config)) = (
        out,
        io.and_then(tutti_midi_hardware::MidiSession::output_config),
    ) {
        out.set_stamping(config.rx_jr);
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

/// Applies the devices' inlet changes: an inlet added to the graph and
/// wired to its slot's event input of the ingest node (whose slot
/// then starts over, so the device before it leaves no half-sent run), or
/// taken out. Whether the graph was edited. A session with no ingest node
/// has nothing to wire, and the changes are dropped with it.
#[cfg(feature = "midi-hardware")]
fn wire_inlets(
    inlet_keys: &mut Vec<Option<NodeKey>>,
    inlets: &tutti_midi_runtime::DeviceInlets,
    nodes: Option<&MidiNodes>,
    graph: &mut Graph,
) -> bool {
    use tutti_midi_runtime::InletChange;
    let changes = inlets.take_changes();
    let Some(nodes) = nodes else { return false };
    let edited = !changes.is_empty();
    for change in changes {
        match change {
            InletChange::Attach { slot, node } => {
                let key = graph.add_event_inlet(node);
                nodes.input_controls.restart_slot(slot);
                graph.set_event_sources(nodes.input, slot as u16, &[EventSource::new(key, 0)]);
                if inlet_keys.len() <= slot {
                    inlet_keys.resize(slot + 1, None);
                }
                inlet_keys[slot] = Some(key);
            }
            InletChange::Detach { slot } => {
                if let Some(key) = inlet_keys.get_mut(slot).and_then(Option::take) {
                    graph.set_event_sources(nodes.input, slot as u16, &[]);
                    graph.remove(key);
                }
            }
        }
    }
    edited
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

    /// A live input for `target` (a synth, a plugin): an event inlet fed
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
                keys: live.keys.clone(),
            });
        }
        let (node, keys) = SharedKeys::open(self.graph);
        self.graph
            .set_feed(target, Feeder::LiveInput, vec![node.into()]);
        self.state.live.insert(
            node,
            Live {
                keys: keys.clone(),
                target,
            },
        );
        Ok(LiveInput { node, keys })
    }

    // --- Clips ---

    /// Plays `events` on `target` through its clip: the one it has, its
    /// events replaced in place (which ends the notes the old events left
    /// sounding), or a new `MidiClipNode` fed into its event input. Returns
    /// the clip's node.
    ///
    /// The events are anything that is `Into<TimedMidiEvent>`: a
    /// [`TimedMidiEvent`] or a `(Beat, Ump)` pair, so a file's
    /// `MidiFile::timeline()` plays as it is. They are absolute-beat
    /// positioned, in any order; the clip
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
        events: impl IntoIterator<Item = impl Into<TimedMidiEvent>>,
    ) -> Result<NodeKey> {
        self.takes_midi(target)?;
        let events = events.into_iter().map(Into::into);
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

    /// The layout of the input's ports; the first Group's with no input,
    /// where nothing is compiled.
    fn input_ports(&self) -> MidiInputPorts {
        self.nodes
            .map_or(MidiInputPorts::FIRST_GROUP, |n| n.input_ports)
    }

    fn compile_own(&mut self, input: NodeKey) {
        let rules = std::mem::take(&mut self.state.rules);
        let fallback = self.state.fallback;
        self.compile_routes(input, rules.values(), fallback, Some);
        self.state.rules = rules;
    }

    /// Routes `input` (a `MidiIngestNode`: one event output per channel and
    /// one for channelless messages, in each Group it hears) by `rules`, keyed by the host's own
    /// `T` and resolved to nodes through `resolve`, replacing every route
    /// before (the session's own rules included).
    ///
    /// A channel rule gives its targets that channel's port in its Group and
    /// the Group's channelless one, an any-channel rule all of them, several rules the
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
            self.input_ports(),
            rules.into_iter().filter(|r| r.enabled).map(|r| RouteRule {
                group: r.group,
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

    /// The output the hardware out (an event tap on the clock, aligned to the
    /// graph's outputs) drains to at each update's
    /// [`MidiOut`](crate::Phase::MidiOut) phase. `None`: its events are counted as dropped
    /// ([`SessionEvent::MidiOutDropped`](crate::SessionEvent::MidiOutDropped)).
    ///
    /// **Whether it JR-stamps is the device's.** With the `midi-hardware`
    /// feature, each `MidiOut` phase sets the output's stamping from the
    /// configuration of the output device the session holds
    /// (`tutti_midi_hardware::MidiSession::output_config`):
    /// on only when the device receives JR Timestamps (`rx_jr`, which the
    /// session asks for off, so a device that does not turn it on itself is
    /// sent none), and the device's own output refuses a JR message otherwise.
    /// An output that is not that device's (a host's own sink) stamps as
    /// [`UmpOut::set_stamping`] says. There is no switch of the session's own.
    pub fn set_hardware_out(&mut self, out: Option<UmpOut>) {
        self.state.out = out;
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

    /// Events dropped for want of an output, or refused by the hardware out's
    /// tap for want of room (1 024 events between two updates), counted.
    pub fn out_drops(&self) -> u64 {
        self.state.drops.count()
    }

    /// The sample rate the hardware out's frames are on: the rate the graph
    /// was prepared at when its tap was opened, which the session reopens
    /// when the graph is re-rated. `None` when the session has no engine
    /// MIDI.
    #[must_use]
    pub fn hardware_out_rate(&self) -> Option<tutti_types::SampleRate> {
        self.state.hardware_tap.as_ref().map(EventTapReader::rate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use tutti_types::SampleRate;
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
        let mut out = UmpOut::new(
            Box::new(CountingSink(count.clone())),
            SampleRate::new(48_000.0),
        );
        out.set_stamping(stamping);
        (out, count)
    }

    fn note(offset: u32) -> UmpAt {
        (
            Offset::new(offset as usize, Samples(512)).unwrap(),
            tutti_fixtures::midi::note_on(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                60,
                tutti_fixtures::midi::MID_VELOCITY,
            ),
        )
    }

    /// Both drains stamp through one stream, so the origin keeps climbing
    /// across them.
    #[test]
    fn both_producers_advance_one_origin() {
        let (mut out, _) = out_with_counter(true);

        out.send_stamped(&[note(511)]);
        assert_eq!(out.origin(), Frame(512), "one 512-frame block");

        out.send_stamped(&[note(511)]);
        assert_eq!(
            out.origin(),
            Frame(1024),
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
            out.origin(),
            Frame(0),
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
        router.route(&[at(0), at(1), at(2)]);
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
        router.route(&[at(0), at(1)]);
        assert_eq!(count.load(Ordering::SeqCst), 2, "both reached the sink");
        assert_eq!(drops.count(), 0, "nothing was dropped");
    }

    /// An event on a frame of the graph's clock.
    fn at(frame: u64) -> (Frame, Ump) {
        (Frame::new(frame), note(0).1)
    }

    /// Keeps every batch it is handed.
    struct Recording(Arc<std::sync::Mutex<Vec<Vec<UmpAt>>>>);
    impl tutti_midi_runtime::MidiOut for Recording {
        fn queue(&self, events: &[UmpAt]) -> usize {
            self.0.lock().unwrap().push(events.to_vec());
            events.len()
        }
    }

    /// **A batch is read as a block from its earliest frame to its last,
    /// inclusive.** Frames 1 000, 1 250 and 1 999 (given out of order) reach
    /// the sink as offsets 250, 0 and 999 of a 1 000-frame span, and a
    /// stamping output advances its origin by that span: 1 000 frames.
    #[test]
    fn a_batch_of_frames_is_a_block_from_its_first_frame() {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut out = UmpOut::new(Box::new(Recording(log.clone())), SampleRate::new(48_000.0));
        let mut router = MidiOutRouter {
            out: Some(&mut out),
            drops: None,
        };
        router.route(&[at(1_250), at(1_000), at(1_999)]);
        let sent = log.lock().unwrap().remove(0);
        let span = Samples(1_000);
        let offsets: Vec<_> = sent.iter().map(|e| e.0).collect();
        assert_eq!(
            offsets,
            [250, 0, 999].map(|o| Offset::new(o, span).unwrap()),
            "each event's spacing from the earliest"
        );
        out.set_stamping(true);
        let before = out.origin();
        MidiOutRouter {
            out: Some(&mut out),
            drops: None,
        }
        .route(&[at(1_250), at(1_000), at(1_999)]);
        assert_eq!(out.origin().since(before), Some(Samples(1_000)), "the span");
    }

    /// **A host's mailbox is drained with its stamps' spacing kept, across
    /// the `u32` wrap.** Stamps `u32::MAX - 4` and 5 are 10 frames apart (the
    /// counter wrapped between them): the sink gets offsets 0 and 10.
    #[test]
    fn a_mailbox_drain_keeps_the_stamps_spacing() {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut out = UmpOut::new(Box::new(Recording(log.clone())), SampleRate::new(48_000.0));
        let (tx, rx) = tutti_midi_runtime::MidiMailbox::pair();
        tx.queue_stamped(&[(u32::MAX - 4, note(0).1), (5, note(0).1)]);
        let mut router = MidiOutRouter {
            out: Some(&mut out),
            drops: None,
        };
        drain_receiver_through(&rx, &mut router);
        let sent = log.lock().unwrap().remove(0);
        let span = Samples(11);
        assert_eq!(
            sent.iter().map(|e| e.0).collect::<Vec<_>>(),
            [0, 10].map(|o| Offset::new(o, span).unwrap())
        );
    }
}
