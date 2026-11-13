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
use tutti_midi_protocol::MpeMode;
use tutti_midi_runtime::{
    ClockMap, MidiClipControls, MidiClipNode, MidiInputPorts, MidiKeys, MidiReceiver, Outgoing,
    OutputLatency, OutputPlan, OutputTerms, Placement, RouteRule, SendAt, TimedMidiEvent,
    compile_routes,
};
use tutti_types::{Frame, Samples};
use tutti_ump::{MidiChannel, MidiGroup, Ump};

use crate::FollowTempo;
use crate::RouteDeclarer;
use crate::graph::{Feeder, Graph};
use crate::instrument::open_keys;
use crate::midi_follow::{FOLLOW_TAP_CAPACITY, Follow, FollowCx};
use crate::{EventOutPort, MidiError, MidiEvent, MidiNodes};

/// What a [`Midi`] call returns: the area's one error.
type Result<T> = std::result::Result<T, MidiError>;

/// Max events drained per chunk into the stack buffer: one frame of clock at
/// any sane tempo is a handful of events, so this is generous headroom.
const DRAIN_CHUNK: usize = 256;

/// How many hardware MIDI devices may be connected at once by default
/// ([`SessionBuilder::midi_devices`](crate::SessionBuilder::midi_devices)).
pub const MIDI_INPUT_DEVICES: usize = 8;

/// Events one connected device's inlet holds: the ring its thread pushes into,
/// and as many waiting for their frame. It is the one ring of the hardware
/// input path (`DRAIN_CHUNK` above is the clock's drain, and the 256 of
/// `SessionEvent::EventsDropped` a log of losses, neither a MIDI input ring).
///
/// **The figure is one second of a saturated MIDI 1.0 cable.** The hardware
/// interface sends a byte every 320 microseconds (MIDI 1.0 Detailed
/// Specification, Hardware: 31.25 kbaud, a start bit, 8 data bits and a stop
/// bit), so the shortest message there is, a two-byte one under running
/// status, comes at most every 640 microseconds: 1,563 a second. 2,048 holds
/// that for 1.3 s, where an event waits for its frame only the lead (one block,
/// [`input_lead`]) and a ring is emptied every block, so a full ring means a
/// stalled graph, which the inlet reports as lost
/// (`MidiEvent::InputsDropped`) and does not block on. A device faster than a
/// DIN cable (USB) fills it sooner, at its own rate, for the same stall.
#[cfg(feature = "midi-hardware")]
pub(crate) const MIDI_INPUT_INLET_CAPACITY: tutti_graph::EventCapacity =
    tutti_graph::EventCapacity::of(2048);

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
    std::sync::Arc::new(move || {
        let reading = clock.read()?;
        Some((
            os_instant(reading.os_ns)?,
            tutti_types::Moment::new(reading.frame, reading.rate),
        ))
    })
}

/// The `Instant` of a reading of [`tutti_rt::monotonic_ns`]: the one pairing of
/// the two clocks, read once per process, so the conversion is an offset.
/// `None` where it is beyond what an `Instant` holds.
#[cfg(any(feature = "device", feature = "midi-hardware"))]
fn os_instant(ns: i64) -> Option<Instant> {
    use std::sync::OnceLock;
    static PAIRING: OnceLock<(Instant, i64)> = OnceLock::new();
    let (instant, base) = *PAIRING.get_or_init(|| (Instant::now(), tutti_rt::monotonic_ns()));
    let delta = ns.saturating_sub(base);
    let span = Duration::from_nanos(delta.unsigned_abs());
    if delta >= 0 {
        instant.checked_add(span)
    } else {
        instant.checked_sub(span)
    }
}

/// What the session reads of the engine to time the hardware out: its block
/// clock and the audio output's latency (the stream's cell, which the device's
/// callback writes, `None` until it reports, and a restart resets in place).
#[cfg(feature = "device")]
#[derive(Clone, Debug)]
pub(crate) struct OutputClockSource {
    clock: tutti_engine::BlockClockReader,
    audio_latency: std::sync::Arc<tutti_cpal::StreamLatency>,
}

#[cfg(feature = "device")]
impl OutputClockSource {
    pub(crate) fn new(
        clock: tutti_engine::BlockClockReader,
        audio_latency: std::sync::Arc<tutti_cpal::StreamLatency>,
    ) -> Self {
        Self {
            clock,
            audio_latency,
        }
    }

    /// The audio output's latency as the device reports it now: read on every
    /// drain, for it can arrive or change after a restart.
    fn audio_output(&self) -> Option<Duration> {
        self.audio_latency.get()
    }

    /// The timing of a drain now: the latest block's map, `Instant::now()` and
    /// the terms as the device reports them.
    fn timing(&self, latency: OutputLatency) -> OutputTiming {
        OutputTiming {
            map: self.map(),
            now: Instant::now(),
            terms: OutputTerms {
                audio_output: self.audio_output(),
                latency,
            },
        }
    }

    /// The latest block reading as a map from frames to instants: `None`
    /// before the first block.
    fn map(&self) -> Option<ClockMap> {
        let reading = self.clock.read()?;
        Some(ClockMap::new(
            reading.rate,
            tutti_midi_runtime::ClockReading::new(os_instant(reading.os_ns)?, reading.frame),
        ))
    }
}

/// What [`UmpOut::send_frames`] times a batch by: the frames' map to the OS
/// clock, the caller's reading of that clock and every other term of the send
/// time ([`OutputPlan`]).
#[derive(Clone, Copy, Debug)]
pub struct OutputTiming {
    /// Places a frame on the OS clock; `None` before the engine's first block,
    /// when every frame is sent at once.
    pub map: Option<ClockMap>,
    /// The OS clock now: what a send time is compared with.
    pub now: Instant,
    /// The audio output's latency and the output's own.
    pub terms: OutputTerms,
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
    /// Scratch: when each offset of the batch is sent, by offset.
    sends: Vec<(Offset, SendAt)>,
    /// Scratch for the batch as the sink takes it, with its send times.
    outgoing: Vec<Outgoing>,
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
            sends: Vec::new(),
            outgoing: Vec::new(),
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
        self.send_batch(events);
    }

    /// Like [`send_stamped`](Self::send_stamped), but tells the stream how
    /// long the block was so the JR Clock cadence keeps running through
    /// silence (§7.2.2.1: the cadence is a property of elapsed time, not of
    /// traffic).
    pub fn send_stamped_span(&mut self, events: &[UmpAt], block: Samples) {
        if !self.stamping {
            self.send_batch(events);
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
    ///
    /// With `timing`, each event is sent at the time its frame has on the OS
    /// clock ([`OutputPlan`]: the frame's instant, plus the audio output's
    /// latency, less the device's, plus the offset), and the sink schedules it;
    /// without, every event goes at once. An event whose time is more than
    /// [`MAX_OUTPUT_AHEAD`](tutti_midi_runtime::MAX_OUTPUT_AHEAD) ahead is not
    /// sent, and the call returns how many were refused. The JR stamps keep the
    /// events' spacing whether or not they are scheduled.
    pub fn send_frames(&mut self, events: &[(Frame, Ump)], timing: Option<&OutputTiming>) -> usize {
        let (Some(first), Some(last)) = (
            events.iter().map(|e| e.0).min(),
            events.iter().map(|e| e.0).max(),
        ) else {
            return 0;
        };
        let span = Samples((last.get() - first.get()) as usize + 1);
        let plan = timing.map(|t| OutputPlan::new(t.terms));
        let mut batch = core::mem::take(&mut self.rebased);
        let mut sends = core::mem::take(&mut self.sends);
        batch.clear();
        sends.clear();
        let mut refused = 0;
        for &(frame, ump) in events {
            let send = match (plan.as_ref(), timing) {
                (Some(plan), Some(t)) => match plan.place(frame, t.map.as_ref(), t.now) {
                    Placement::Send(send) => send,
                    Placement::TooFar => {
                        refused += 1;
                        continue;
                    }
                },
                _ => SendAt::Now,
            };
            // Always `Some`: every rebased frame is below the span.
            if let Some(at) = Offset::new((frame.get() - first.get()) as usize, span) {
                batch.push((at, ump));
                sends.push((at, send));
            }
        }
        sends.sort_unstable_by_key(|&(at, _)| at);
        self.sends = sends;
        self.send_batch(&batch);
        self.sends.clear();
        self.rebased = batch;
        refused
    }

    /// Stamps `events` (when stamping) and sends them, each at the time
    /// `self.sends` has for its offset, else at once.
    fn send_batch(&mut self, events: &[UmpAt]) {
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

    fn send_all(&mut self, events: &[UmpAt]) {
        let mut out = core::mem::take(&mut self.outgoing);
        out.clear();
        // A JR stamp is at its event's offset, so it is sent with it.
        out.extend(events.iter().map(|&(at, ump)| {
            Outgoing {
                at,
                send: self
                    .sends
                    .binary_search_by_key(&at, |&(o, _)| o)
                    .map_or(SendAt::Now, |i| self.sends[i].1),
                ump,
            }
        }));
        // One `queue_timed` for the batch: a backend may pack several UMP
        // messages into one packet.
        self.sink.queue_timed(&out);
        self.outgoing = out;
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
    /// Events discarded for want of a connected output, refused by the hardware
    /// out's tap for want of room, or refused as scheduled more than
    /// [`MAX_OUTPUT_AHEAD`](tutti_midi_runtime::MAX_OUTPUT_AHEAD) ahead.
    pub fn count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Record `n` events the hardware out's tap refused for want of room, or
    /// the plan refused as too far ahead: a count, with no warning of its own
    /// (there is an output to blame).
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
    /// What the events are timed by ([`UmpOut::send_frames`]); `None` sends
    /// them at once. An event refused as too far ahead is counted in `drops`.
    pub timing: Option<&'a OutputTiming>,
}

impl MidiOutRouter<'_> {
    /// Routes a batch of drained events, each on its frame on the graph's
    /// clock, to the open output ([`UmpOut::send_frames`]). An event with
    /// no output is dropped (the ring must not back up), counted first.
    pub fn route(&mut self, events: &[(Frame, Ump)]) {
        match self.out.as_mut() {
            Some(out) => {
                let refused = out.send_frames(events, self.timing);
                if let Some(drops) = self.drops {
                    drops.record_refused(refused as u64);
                }
            }
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

/// What [`Midi::unroute`] found missing: the rule `id` named.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("no MIDI route {0:?}")]
pub struct NoSuchMidiRoute(pub MidiRouteId);

/// Where a take's events belong on the timeline (doc 030 §5.2): the figure
/// [`Midi::record`]'s readers are placed by. A note played at graph frame `F`
/// was answered by output the device played `output` after it was rendered, and
/// reached the graph `input` plus the inlet's `lead` after it was played, so
/// it was meant for `F - (input + output + lead) + trim`.
///
/// A pure function of the figures a host reads (the device's reported
/// latencies, the inlet's lead) and sets (the trim); the session keeps none of
/// them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TakePlacement {
    /// How long a played event took to reach the graph (the device's input latency).
    pub input: Samples,
    /// How long the graph's output took to be heard (the device's output latency).
    pub output: Samples,
    /// The inlet's fixed lead: the distance a hardware event is placed after it arrived.
    pub lead: Samples,
    /// The user's manual trim for a device that misreports, added after.
    pub trim: Samples,
}

impl TakePlacement {
    /// The frame an event the tap read at `frame` was played for, at 0 where the
    /// compensation reaches before the start of the clock.
    #[must_use]
    pub fn place(self, frame: Frame) -> Frame {
        let back = self
            .input
            .0
            .saturating_add(self.output.0)
            .saturating_add(self.lead.0);
        let back = u64::try_from(back).unwrap_or(u64::MAX);
        Frame::new(frame.get().saturating_sub(back)) + self.trim
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
///
/// **Persisting a rule** (feature `serde`): a [`NodeKey`] names a node of one
/// session and means nothing in the next, so a rule that is saved is keyed by
/// the app's own stable id, `MidiRouteRule<AppId>` (`AppId: Serialize`), and
/// the app maps it to a key when it loads: [`set_routes`](Midi::set_routes)
/// takes the rules keyed by `T` and a `resolve` from `T` to a node, which is
/// where the app's table lives. `MidiRouteRule<NodeKey>` itself has no serde.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
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

    /// Adds a destination: a `T`, or anything that converts into one (a
    /// session's own rule takes a graph [`Key`](crate::Key) as it takes a
    /// [`NodeKey`]).
    #[must_use]
    pub fn to(mut self, target: impl Into<T>) -> Self {
        self.targets.push(target.into());
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

/// One live input: the target it feeds, and its keys until [`Midi::input`]
/// hands them out (a take can open the inlet before anyone plays it).
struct Live {
    keys: Option<MidiKeys>,
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
    /// The last load id minted ([`MidiLoad`]).
    loads: u64,
    /// The sinks the last route compile fed.
    routed: BTreeSet<NodeKey>,
    /// Whether a host's last `set_routes` declared any rule or fallback:
    /// the routes are then the host's, and the session's own are refused.
    host_routes: bool,
    pub(crate) out: Option<UmpOut>,
    /// The session's one MPE mode ([`Midi::set_mpe`]).
    mpe: MpeMode,
    /// The hardware out's tap on the clock, which `Phase::MidiOut` drains;
    /// `None` when the session has no engine MIDI (or the clock left).
    hardware_tap: Option<EventTapReader>,
    drops: MidiOutDrops,
    /// The tempo the session follows, if it does ([`Midi::follow_tempo`]).
    follow: Option<Follow>,
    /// Events [`Midi::unfollow_tempo`] queued for the next `Poll` phase, which
    /// reports them.
    follow_events: Vec<MidiEvent>,
    /// What the engine gives the hardware out's timing: its block clock and the
    /// audio output's latency. `None` with no audio device, where every event
    /// is sent at once.
    #[cfg(feature = "device")]
    output_clock: Option<OutputClockSource>,
    /// The hardware out's own latency and offset ([`Midi::set_output_latency`]).
    output_latency: OutputLatency,
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
/// ([`MidiEvent::OutputDropped`]):
/// a clock running with no output drops ticks on every update.
pub const MIDI_OUT_DROP_REPORT: Duration = Duration::from_secs(1);

/// How often a rising count of events the hardware inputs' inlets dropped is
/// reported at most
/// (`MidiEvent::InputsDropped`, feature `midi-hardware`):
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

    /// The hardware out is timed by `clock` from now on (a session with an
    /// audio device).
    #[cfg(feature = "device")]
    pub(crate) fn set_output_clock(&mut self, clock: OutputClockSource) {
        self.output_clock = Some(clock);
    }

    /// The state with the session's MPE mode as the builder declared it.
    pub(crate) fn with_mpe(mut self, mpe: MpeMode) -> Self {
        self.mpe = mpe;
        self
    }

    /// The session's MPE mode.
    pub(crate) fn mpe(&self) -> &MpeMode {
        &self.mpe
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
    /// reports those that settled ([`MidiEvent::StreamSettled`]), reports the
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
            out.push(MidiEvent::of_device(event.clone()).into());
            if let tutti_midi_hardware::DeviceEvent::Removed(device) = event {
                // Held ones are closed here, not left to fail on a dead
                // handle: the port goes and the lists agree.
                if io.disconnect_input(&device.id) {
                    out.push(
                        MidiEvent::InputLost {
                            info: device.clone(),
                        }
                        .into(),
                    );
                }
                if io.output().is_some_and(|open| open.id == device.id) && io.disconnect_output() {
                    out.push(MidiEvent::OutputLost { info: device }.into());
                }
            }
        }
        // The devices' UMP Stream negotiations take their step at `now`: the
        // session's control tick is their timer.
        for (device, stream) in io.poll(now) {
            out.push(
                MidiEvent::StreamSettled {
                    device,
                    stream: Box::new(stream),
                }
                .into(),
            );
        }
        // What the poll settled or changed is heard by the output at once: the
        // `MidiOut` phase that drains next stamps by it.
        follow_output_config(self.out.as_mut(), Some(io));
        let wired = wire_inlets(&mut self.inlet_keys, io.ports().inlets(), nodes, graph);
        if !self.watch_failure_reported
            && let Some(why) = events.and_then(tutti_midi_hardware::DeviceEvents::failure)
        {
            self.watch_failure_reported = true;
            out.push(MidiEvent::WatchFailed(why).into());
        }
        let total = io.ports().losses().dropped();
        let due = self
            .input_reported_at
            .is_none_or(|at| now.saturating_duration_since(at) >= MIDI_INPUT_DROP_REPORT);
        if total != self.input_drops_seen && due {
            self.input_drops_seen = total;
            self.input_reported_at = Some(now);
            out.push(MidiEvent::InputsDropped { total }.into());
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
        #[cfg(feature = "device")]
        let timing = self
            .output_clock
            .as_ref()
            .map(|source| source.timing(self.output_latency));
        #[cfg(not(feature = "device"))]
        let timing: Option<OutputTiming> = None;
        let mut router = MidiOutRouter {
            out: self.out.as_mut(),
            drops: Some(&self.drops),
            timing: timing.as_ref(),
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

    /// Whether the session follows a live tempo now.
    pub(crate) fn is_following(&self) -> bool {
        self.follow.is_some()
    }

    /// `Phase::Poll`, for the tempo the session follows: reports what
    /// `unfollow_tempo` queued, reads the follower's tap and applies what it
    /// heard (the project's tempo map and meter, overlaid at the next block
    /// start; see `midi_follow`). Whether the graph was edited, which the
    /// caller commits.
    pub(crate) fn follow_phase(
        &mut self,
        cx: &FollowCx<'_>,
        graph: &mut Graph,
        out: &mut Vec<crate::SessionEvent>,
    ) -> bool {
        out.extend(self.follow_events.drain(..).map(Into::into));
        let Some(follow) = self.follow.as_mut() else {
            return false;
        };
        // What the tap holds is read at the scale it was opened at, then the
        // tap is reopened if the graph has been re-prepared at another rate.
        let heard = follow.heard();
        let mut edited = false;
        if follow.tap_rate() != graph.prepared().sample_rate() {
            edited = true;
            if let Some(tap) = graph.taps().reopen_event(follow.tap_id()) {
                follow.set_tap(tap);
            }
        }
        follow.apply(&heard, cx, out);
        edited
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
        self.hardware_tap = graph.taps().reopen_event(id);
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
            out.push(MidiEvent::OutputDropped { total: count }.into());
        }
    }
}

/// Sets `out` to stamp iff the session's output device receives JR Timestamps.
#[cfg(feature = "midi-hardware")]
fn follow_output_config(out: Option<&mut UmpOut>, io: Option<&tutti_midi_hardware::MidiSession>) {
    if let (Some(out), Some(config)) = (out, io.and_then(tutti_midi_hardware::MidiSession::output))
    {
        out.set_stamping(config.config.rx_jr);
    }
}

/// A file load a [`Midi`] started, named so the event that lands it can be
/// matched to the call: [`Midi::load_file`] and `Midi::load_soundfont`
/// return one, and [`MidiEvent::FileLoaded`] and
/// `SoundFontLoaded` carry it. The session mints it; it names nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MidiLoad(u64);

/// The session's MIDI (design 015 §4.3): live inputs, clips, the hardware
/// input's routes and the hardware out. [`Session::midi`](crate::Session::midi)
/// hands it out; what it edits goes out with the next update's commit.
#[derive(Debug)]
pub struct Midi<'a> {
    pub(crate) graph: &'a mut Graph,
    pub(crate) state: &'a mut MidiState,
    pub(crate) nodes: Option<&'a MidiNodes>,
    pub(crate) jobs: &'a mut crate::jobs::Jobs,
    /// The transport's settings (the tempo map a follower overlays).
    pub(crate) settings: &'a tutti_transport::TransportSettings,
    /// The project meter a follower overlays.
    pub(crate) meter: tutti_transport::ProjectMeter,
    /// The OS MIDI layer, when the builder opened it.
    #[cfg(feature = "midi-hardware")]
    pub(crate) io: Option<&'a tutti_midi_hardware::MidiSession>,
}

/// Applies the devices' inlet changes: an inlet added to the graph and
/// wired to its slot's event input of the ingest node (which starts the
/// slot over by itself when a new device takes it, so the device before it
/// leaves no half-sent run), or taken out. Whether the graph was edited. A session with no ingest node
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
    let Some(nodes) = nodes else {
        // Dropped with the session that has no ingest node.
        changes.for_each(drop);
        return false;
    };
    let mut edited = false;
    for change in changes {
        edited = true;
        match change {
            InletChange::Attach { slot, node } => {
                let key = graph.add_event_inlet(node).erase();
                if let Err(e) = graph.set_sources(
                    crate::EventInPort::new(nodes.input, slot),
                    [EventOutPort::new(key, 0)],
                ) {
                    tracing::warn!("midi: device slot {slot} has no ingest port: {e}");
                }
                if inlet_keys.len() <= slot {
                    inlet_keys.resize(slot + 1, None);
                }
                inlet_keys[slot] = Some(key);
            }
            InletChange::Detach { slot } => {
                if let Some(key) = inlet_keys.get_mut(slot).and_then(Option::take) {
                    let _ = graph.set_sources(crate::EventInPort::new(nodes.input, slot), []); // a slot with no port has no source to take out
                    let _ = graph.remove(key); // a host that removed the inlet itself leaves nothing to take
                }
            }
        }
    }
    edited
}

impl Midi<'_> {
    fn next_load(&mut self) -> MidiLoad {
        self.state.loads += 1;
        MidiLoad(self.state.loads)
    }

    /// Reads the MIDI file at `path` on the session's spawner. Returns at
    /// once; the result lands as [`MidiEvent::FileLoaded`] with the returned
    /// id in a later [`Session::update`](crate::Session::update), on the
    /// thread that calls `update()`. A file that cannot be read lands as
    /// [`MidiError::File`], a job that did not finish as [`MidiError::Job`].
    #[must_use = "the id matches the event that lands the load"]
    pub fn load_file(&mut self, path: impl Into<std::path::PathBuf>) -> MidiLoad {
        let load = self.next_load();
        let path = path.into();
        self.jobs.submit(
            move || tutti_midi_file::MidiFile::read(path),
            move |_, done, events| {
                let file = match done {
                    Ok(read) => read.map_err(MidiError::from),
                    Err(failed) => Err(failed.into()),
                };
                events.push(MidiEvent::FileLoaded { load, file }.into());
            },
        );
        load
    }

    /// Opens the SoundFont at `path` and builds a unit from it with
    /// `settings`, on the session's spawner. Returns at once; the result
    /// lands as [`MidiEvent::SoundFontLoaded`] with the returned id in a later
    /// [`Session::update`](crate::Session::update), on the thread that calls
    /// `update()`. The host adds the unit (`add_instrument`,
    /// `add_general_midi`) when it lands.
    #[must_use = "the id matches the event that lands the load"]
    #[cfg(feature = "soundfont")]
    pub fn load_soundfont(
        &mut self,
        path: impl Into<std::path::PathBuf>,
        settings: tutti_soundfont::Settings,
    ) -> MidiLoad {
        let load = self.next_load();
        let path = path.into();
        self.jobs.submit(
            move || -> Result<Box<tutti_soundfont::SoundFontUnit>> {
                let font = tutti_sf2::SoundFont::open(path)?;
                Ok(Box::new(tutti_soundfont::SoundFontUnit::new(
                    std::sync::Arc::new(font),
                    settings,
                )?))
            },
            move |_, done, events| {
                let unit = match done {
                    Ok(built) => built,
                    Err(failed) => Err(failed.into()),
                };
                events.push(MidiEvent::SoundFontLoaded { load, unit }.into());
            },
        );
        load
    }

    /// `target`, if it takes MIDI: it is in the graph with an event input.
    fn takes_midi(&self, target: NodeKey) -> Result<()> {
        if self.graph.node_event_inputs(target).is_ok_and(|n| n > 0) {
            Ok(())
        } else {
            Err(MidiError::NoEventInput(target))
        }
    }

    // --- Live input ---

    /// The keys of `target`'s live input (a synth, a plugin): an event inlet
    /// fed into its main event input, for a keyboard, a preview or an
    /// all-notes-off the host sends. What is sent plays from the next block
    /// ([`Stamp::NextBlock`](tutti_graph::Stamp::NextBlock)) or on a frame of the engine's clock.
    ///
    /// A target has one live input and its keys are the inlet's one
    /// producer: they are handed out once. [`record`](Self::record) opens the
    /// inlet too, and leaves the keys for this call.
    ///
    /// # Errors
    ///
    /// [`MidiError::NoEventInput`] when `target` is not in the graph or takes
    /// no MIDI; [`MidiError::InputTaken`] when the keys were handed out already.
    pub fn input(&mut self, target: impl Into<NodeKey>) -> Result<MidiKeys> {
        let target = target.into();
        let node = self.live_inlet(target)?;
        self.state
            .live
            .get_mut(&node)
            .and_then(|live| live.keys.take())
            .ok_or(MidiError::InputTaken(target))
    }

    /// The node of `target`'s live input, opened with its keys held if it has
    /// none.
    fn live_inlet(&mut self, target: NodeKey) -> Result<NodeKey> {
        self.takes_midi(target)?;
        if let Some((node, _)) = self.fed_by(target, Feeder::LiveInput, &self.state.live) {
            return Ok(node);
        }
        let (node, keys) = open_keys(self.graph);
        let node = node.erase();
        self.graph
            .set_feed(target, Feeder::LiveInput, vec![EventOutPort::new(node, 0)]);
        self.state.live.insert(
            node,
            Live {
                keys: Some(keys),
                target,
            },
        );
        Ok(node)
    }

    // --- Clips ---

    /// Plays `events` on `target` through its clip: the one it has, its
    /// events replaced in place (which ends the notes the old events left
    /// sounding), or a new `MidiClipNode` fed into its event input. Returns
    /// the clip's node.
    ///
    /// The events are [`TimedMidiEvent`]s (each beat finite: it is checked
    /// where one is built, `TimedMidiEvent::new`); a file's
    /// `MidiFile::timeline()` is `(Beat, Ump)` pairs a caller builds them from.
    /// They are absolute-beat positioned, in any order; the clip
    /// reads its block's `Env`, so a note lands on its frame through seeks
    /// and loop wraps, and an export forks it with the target.
    ///
    /// # Errors
    ///
    /// [`MidiError::NoEventInput`] when `target` is not in the graph or takes
    /// no MIDI.
    pub fn play_clip(
        &mut self,
        target: impl Into<NodeKey>,
        events: impl IntoIterator<Item = TimedMidiEvent>,
    ) -> Result<NodeKey> {
        let target = target.into();
        self.takes_midi(target)?;
        let clip = match self.clip(target) {
            Some(clip) => {
                self.state.clips[&clip].controls.set(events);
                clip
            }
            None => {
                let (clip, controls) = self.graph.add(MidiClipNode::new(events));
                let clip = clip.erase();
                self.state.clips.insert(clip, Clip { controls, target });
                clip
            }
        };
        // Set on every call: `set_feed` leaves an unchanged feed (and the
        // edit flag) alone.
        self.graph
            .set_feed(target, Feeder::Sequencer, vec![EventOutPort::new(clip, 0)]);
        Ok(clip)
    }

    /// Empties `target`'s clip, ending the notes it left sounding. The clip
    /// stays, to play the next [`play_clip`](Self::play_clip). `false`
    /// when `target` has none.
    pub fn stop_clip(&mut self, target: impl Into<NodeKey>) -> bool {
        let target = target.into();
        match self.clip(target) {
            Some(clip) => {
                self.state.clips[&clip].controls.clear();
                true
            }
            None => false,
        }
    }

    /// `target`'s clip node, if it has one.
    pub fn clip(&self, target: impl Into<NodeKey>) -> Option<NodeKey> {
        let target = target.into();
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
            .find_map(|s| ours.get_key_value(&s.node()))
            .map(|(&node, v)| (node, v))
    }

    /// Feeds clip `node` (one this session made) into `target` in place of
    /// the target it fed: a host whose target was re-bound to a new node keeps
    /// its clip. A target takes one clip: one it had is no longer fed. A live
    /// input moves with [`retarget_input`](Self::retarget_input).
    ///
    /// # Errors
    ///
    /// [`MidiError::NoEventInput`] when `target` is not in the graph or takes
    /// no MIDI; [`MidiError::NotSessionMidi`] when `node` is not a clip of
    /// this session's.
    pub fn retarget(&mut self, node: impl Into<NodeKey>, target: impl Into<NodeKey>) -> Result<()> {
        let node = node.into();
        let target = target.into();
        self.takes_midi(target)?;
        let Some(clip) = self.state.clips.get_mut(&node) else {
            return Err(MidiError::NotSessionMidi(node));
        };
        let old = std::mem::replace(&mut clip.target, target);
        self.refeed(node, old, target, Feeder::Sequencer);
        Ok(())
    }

    /// Feeds the live input of `from` into `to` in place of it, a host's
    /// re-bind of the target (`from` may be gone from the graph already): the
    /// same inlet and keys, now playing `to`. A target takes one live input:
    /// one `to` had is no longer fed.
    ///
    /// # Errors
    ///
    /// [`MidiError::NoEventInput`] when `to` is not in the graph or takes no
    /// MIDI; [`MidiError::NoLiveInput`] when `from` has no live input.
    pub fn retarget_input(
        &mut self,
        from: impl Into<NodeKey>,
        to: impl Into<NodeKey>,
    ) -> Result<()> {
        let from = from.into();
        let to = to.into();
        self.takes_midi(to)?;
        let Some((&node, live)) = self.state.live.iter_mut().find(|(_, l)| l.target == from) else {
            return Err(MidiError::NoLiveInput(from));
        };
        live.target = to;
        self.refeed(node, from, to, Feeder::LiveInput);
        Ok(())
    }

    /// Moves the feed of `node`'s event output from `old` to `target`.
    fn refeed(&mut self, node: NodeKey, old: NodeKey, target: NodeKey, feeder: Feeder) {
        if old != target
            && self
                .graph
                .feed(old, feeder)
                .contains(&EventOutPort::new(node, 0))
        {
            self.graph.set_feed(old, feeder, Vec::new());
        }
        self.graph
            .set_feed(target, feeder, vec![EventOutPort::new(node, 0)]);
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
    /// [`MidiError::NoMidiInput`] on a session with no MIDI input node (one
    /// built detached); [`MidiError::TwoRouteDeclarers`] while a host's
    /// `set_routes` declares the routes.
    pub fn route(&mut self, rule: MidiRouteRule) -> Result<MidiRouteId> {
        let input = self.ingest_node()?;
        self.own_may_declare()?;
        self.state.minted += 1;
        let id = MidiRouteId(self.state.minted);
        self.state.rules.insert(id, rule);
        self.compile_own(input);
        Ok(id)
    }

    /// Removes a rule [`route`](Self::route) added.
    ///
    /// # Errors
    ///
    /// [`MidiError::NoSuchRoute`] ([`NoSuchMidiRoute`]) when there is no rule `id` (removed already, or
    /// never added); nothing changes.
    pub fn unroute(&mut self, id: MidiRouteId) -> Result<()> {
        if self.state.rules.remove(&id).is_none() {
            return Err(NoSuchMidiRoute(id).into());
        }
        if let Ok(input) = self.ingest_node() {
            self.compile_own(input);
        }
        Ok(())
    }

    /// Where an event matching no rule goes, if anywhere: every port no
    /// rule covers. `None` (the default) drops unmatched events.
    ///
    /// # Errors
    ///
    /// [`MidiError::NoMidiInput`] on a session with no MIDI input node;
    /// [`MidiError::TwoRouteDeclarers`] while a host's
    /// [`set_routes`](Self::set_routes) declares the routes.
    pub fn set_route_fallback(&mut self, fallback: Option<impl Into<NodeKey>>) -> Result<()> {
        let input = self.ingest_node()?;
        self.own_may_declare()?;
        self.state.fallback = fallback.map(Into::into);
        self.compile_own(input);
        Ok(())
    }

    fn own_may_declare(&self) -> Result<()> {
        if self.state.host_routes {
            Err(MidiError::TwoRouteDeclarers(RouteDeclarer::Host))
        } else {
            Ok(())
        }
    }

    fn ingest_node(&self) -> Result<NodeKey> {
        self.nodes.map(|n| n.input).ok_or(MidiError::NoMidiInput)
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
    /// [`MidiError::TwoRouteDeclarers`] while the session's own rules or
    /// fallback are declared ([`unroute`](Self::unroute) them first).
    /// Nothing is changed.
    pub fn set_routes<'r, T: Copy + Ord + 'r>(
        &mut self,
        input: impl Into<NodeKey>,
        rules: impl IntoIterator<Item = &'r MidiRouteRule<T>>,
        fallback: Option<T>,
        resolve: impl Fn(T) -> Option<NodeKey>,
    ) -> Result<()> {
        let input = input.into();
        if !self.state.rules.is_empty() || self.state.fallback.is_some() {
            return Err(MidiError::TwoRouteDeclarers(RouteDeclarer::Session));
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
        let mut wanted: BTreeMap<NodeKey, Vec<EventOutPort>> = BTreeMap::new();
        for (target, ports) in ports {
            let Some(sink) = resolve(target).filter(|&n| self.graph.contains(n)) else {
                continue;
            };
            wanted
                .entry(sink)
                .or_default()
                .extend(ports.into_iter().map(|p| EventOutPort::new(input, p)));
        }
        for &fed in &self.state.routed {
            if !wanted.contains_key(&fed) {
                self.graph.set_feed(fed, Feeder::Routes, Vec::new());
            }
        }
        self.state.routed = wanted.keys().copied().collect();
        for (sink, mut sources) in wanted {
            sources.sort();
            sources.dedup();
            self.graph.set_feed(sink, Feeder::Routes, sources);
        }
    }

    // --- MPE ---

    /// The session's one MPE mode: how the hardware input is ingested and how a
    /// synth added through [`Session::add_instrument`](crate::Session::add_instrument)
    /// occupies the channels. Begins as the builder's
    /// ([`SessionBuilder::mpe_mode`](crate::SessionBuilder::mpe_mode)),
    /// `Disabled` by default.
    #[must_use]
    pub fn mpe(&self) -> MpeMode {
        *self.state.mpe()
    }

    /// Sets the session's MPE mode, once for everything that follows it.
    ///
    /// - The hardware input ingests classic MPE in `mode` from the next block
    ///   ([`MidiIngestControls::set_mpe_mode`](tutti_midi_runtime::MidiIngestControls::set_mpe_mode)),
    ///   on a session that has the input node.
    /// - A synth added after this through
    ///   [`Session::add_instrument`](crate::Session::add_instrument) is built
    ///   in `mode`: **the session's mode wins over the synth's own
    ///   `SynthConfig::mpe`**, so the ingest and the voices cannot disagree.
    ///   A synth already in the graph keeps the mode it was added with (a
    ///   node in a graph cannot be reached from here); add it again to move
    ///   it. A synth inserted with `Graph::add` is the caller's own and keeps
    ///   its config.
    pub fn set_mpe(&mut self, mode: MpeMode) {
        self.state.mpe = mode;
        if let Some(nodes) = self.nodes {
            nodes.input_controls.set_mpe_mode(mode);
        }
    }

    // --- Recording a take ---

    /// Records what is played into `target`: an event tap on the event output
    /// of its live input ([`input`](Self::input), opened when it has
    /// none), which every note sent through the live input passes. A reader of
    /// `capacity` events (a non-note-off is refused once that many are held, a
    /// note-off is kept up to [`note_off_room`](tutti_graph::note_off_room) of
    /// it; both counted by [`EventTapReader::take_lost`]).
    ///
    /// Read it with [`EventTapReader::drain`] from a thread of your own. Each
    /// event's [`frame`](TapEvent::frame) is the frame of the graph's clock it
    /// was due on, as it left the inlet and before the target heard it; where
    /// a take places it on the timeline is
    /// [`TakePlacement::place`] (doc 030 §5.2: the frame minus the input and
    /// output latency and the inlet's lead, plus the user's trim).
    ///
    /// The tap is on the **live input**: notes a hardware input routes straight
    /// to `target` ([`route`](Self::route)) do not pass it. To record a
    /// hardware player, send what its device pushes through the live input, or
    /// tap the ingest node's port with `Graph::taps`.
    ///
    /// The tap is closed when its reader is dropped (the next
    /// [`Reclaim`](crate::Phase::Reclaim) phase) or with
    /// `Graph::taps().close_event`. A re-prepare at another rate reopens the
    /// session's own taps only (the hardware out's); reopen this one with
    /// `Graph::taps().reopen_event`.
    ///
    /// # Errors
    ///
    /// [`MidiError::NoEventInput`] when `target` is not in the graph or takes no MIDI.
    pub fn record(
        &mut self,
        target: impl Into<NodeKey>,
        capacity: tutti_graph::EventCapacity,
    ) -> Result<EventTapReader> {
        let node = self.live_inlet(target.into())?;
        let source = EventOutPort::new(node, 0);
        self.graph
            .taps()
            .open_event(source, tutti_graph::TapAlign::Raw, capacity.get() as usize)
            // The live input was just found or opened in this graph, with its port.
            .map_err(|_| MidiError::NoEventInput(node))
    }

    // --- Following an outside tempo ---

    /// Follows the tempo (and, if `follow.meter`, the time signature) the session's MIDI
    /// input's `follow.group` sends as Flex Data Set Tempo and Set Time Signature
    /// (M2-104 §7.5.3, §7.5.4): the session reads them from an event tap on
    /// that Group's channelless port in each
    /// [`Poll`](crate::Phase::Poll) phase and publishes them to the transport,
    /// reported as [`MidiEvent::TempoFollowed`] and [`MidiEvent::MeterFollowed`].
    /// **Off by default**: until this is called, a Flex tempo changes nothing.
    ///
    /// **It overlays the project's maps; it does not replace them.** The tempo
    /// map and meter the project has now are kept; from the beat of each change,
    /// the published map is the project's up to there and the live steps after,
    /// so a project point or ramp ahead of the playhead is not played while
    /// following (a ramp a step lands in is cut at its beat). The session has no
    /// project tempo apart from the published map, so **the map it finds now is
    /// the one [`unfollow_tempo`](Self::unfollow_tempo) restores**. While
    /// following, `Session::set_tempo` and `set_tempo_map` refuse with
    /// [`TempoError::Following`](crate::TempoError::Following), so nothing else
    /// publishes in between.
    ///
    /// A change takes effect at the start of the next block (a meter at the next
    /// bar line at or after it), at the latest one block and one update after it
    /// was sent; a device that needs a tempo change on a beat sends it ahead,
    /// which Flex allows (a Flex message carries no time of its own). The
    /// transport's beat stays continuous when a publish lands late (design 020,
    /// "Following an outside tempo").
    ///
    /// **One follower at a time**: following again replaces the first. The
    /// project's maps stay the ones found by the first call, and the tempo in
    /// force stays until the new source sends one.
    ///
    /// # Errors
    ///
    /// [`MidiError::NoMidiInput`] when the session has no MIDI input node, and
    /// [`MidiError::GroupNotHeard`] for a Group the input does not hear. Nothing
    /// changes.
    pub fn follow_tempo(&mut self, follow: FollowTempo) -> Result<()> {
        let nodes = self.nodes.ok_or(MidiError::NoMidiInput)?;
        let port = nodes
            .input_ports
            .channelless(follow.group)
            .ok_or(MidiError::GroupNotHeard(follow.group))?;
        let tap = self
            .graph
            .taps()
            .open_event(
                EventOutPort::new(nodes.input, port),
                tutti_graph::TapAlign::Raw,
                FOLLOW_TAP_CAPACITY,
            )
            // The Group's port was just found on the input: a refusal is a port it lacks.
            .map_err(|_| MidiError::GroupNotHeard(follow.group))?;
        let core = tutti_midi_runtime::TempoFollower::new(
            tutti_midi_runtime::TempoSource::Flex(follow.group),
            follow.meter,
        );
        self.state.follow = Some(match self.state.follow.take() {
            Some(first) => first.replaced_by(core, tap),
            None => Follow::new(core, tap, self.settings.tempo_map(), self.meter.current()),
        });
        Ok(())
    }

    /// Stops following ([`follow_tempo`](Self::follow_tempo)): publishes the
    /// project's tempo map and meter again, for the ones the follower changed.
    /// The transport takes them at the next block at its current beat, so the
    /// playhead does not jump, and the tempo from there on is the project's.
    /// Reported as [`MidiEvent::TempoUnfollowed`] by the next update.
    ///
    /// # Errors
    ///
    /// [`MidiError::NotFollowing`] when nothing is followed.
    pub fn unfollow_tempo(&mut self) -> Result<()> {
        let follow = self.state.follow.take().ok_or(MidiError::NotFollowing)?;
        let (tempo, meter) = follow.restore();
        if let Some(tempo) = tempo {
            self.settings.set_tempo_map(tempo);
        }
        if let Some(meter) = meter {
            self.meter.set_meter(meter);
        }
        self.state.follow_events.push(MidiEvent::TempoUnfollowed);
        Ok(())
    }

    // --- Hardware out ---

    /// The output the hardware out (an event tap on the clock, aligned to the
    /// graph's outputs) drains to at each update's
    /// [`MidiOut`](crate::Phase::MidiOut) phase. `None`: its events are counted as dropped
    /// ([`MidiEvent::OutputDropped`]).
    ///
    /// **Whether it JR-stamps is the device's.** With the `midi-hardware`
    /// feature, each `MidiOut` phase sets the output's stamping from the
    /// configuration of the output device the session holds
    /// (`tutti_midi_hardware::MidiSession::output`):
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
            timing: None,
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

    /// Sets the hardware out's own latency and offset: what its MIDI device
    /// takes to sound an event (it is sent that much earlier) and a trim on top
    /// (positive sends later), added to the terms every event is timed by
    /// ([`output_terms`](Self::output_terms)). Applies from the next drain; the
    /// session has one output, and the setting stays when another device is
    /// connected to it.
    pub fn set_output_latency(&mut self, latency: OutputLatency) {
        self.state.output_latency = latency;
    }

    /// The hardware out's own latency and offset, as set.
    #[must_use]
    pub fn output_latency(&self) -> OutputLatency {
        self.state.output_latency
    }

    /// The terms the hardware out's events are timed by now: the audio
    /// output's latency as the device reports it, and the output's own
    /// ([`set_output_latency`](Self::set_output_latency)). `None` on a session
    /// with no audio device, which sends every event at once.
    ///
    /// **While [`is_complete`](OutputTerms::is_complete) is false the audio
    /// output's latency is not reported yet** (before the stream's first
    /// callback, and again after a restart): events are timed with the terms
    /// known, so they leave early by that latency.
    #[must_use]
    pub fn output_terms(&self) -> Option<OutputTerms> {
        #[cfg(feature = "device")]
        {
            let source = self.state.output_clock.as_ref()?;
            Some(OutputTerms {
                audio_output: source.audio_output(),
                latency: self.state.output_latency,
            })
        }
        #[cfg(not(feature = "device"))]
        None
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

    /// **A device's inlet holds at least a second of a saturated MIDI 1.0
    /// cable** (MIDI 1.0 Detailed Specification, Hardware: a byte every 320
    /// microseconds, so a two-byte message every 640 at best): the figure of
    /// [`MIDI_INPUT_INLET_CAPACITY`], derived here from the spec's timing.
    #[cfg(feature = "midi-hardware")]
    #[test]
    fn the_inlet_holds_a_second_of_a_saturated_midi1_cable() {
        let shortest_message = Duration::from_micros(2 * 320);
        // 1,563 messages.
        let in_a_second = Duration::from_secs(1).as_micros() / shortest_message.as_micros() + 1;
        assert!(u128::from(MIDI_INPUT_INLET_CAPACITY.get()) >= in_a_second);
    }

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
        let mut out = UmpOut::new(Box::new(CountingSink(count.clone())), SampleRate::SR_48K);
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
            timing: None,
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
            timing: None,
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
        let mut out = UmpOut::new(Box::new(Recording(log.clone())), SampleRate::SR_48K);
        let mut router = MidiOutRouter {
            out: Some(&mut out),
            drops: None,
            timing: None,
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
            timing: None,
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
        let mut out = UmpOut::new(Box::new(Recording(log.clone())), SampleRate::SR_48K);
        let (tx, rx) = tutti_midi_runtime::MidiMailbox::pair();
        tx.queue_stamped(&[(u32::MAX - 4, note(0).1), (5, note(0).1)]);
        let mut router = MidiOutRouter {
            out: Some(&mut out),
            drops: None,
            timing: None,
        };
        drain_receiver_through(&rx, &mut router);
        let sent = log.lock().unwrap().remove(0);
        let span = Samples(11);
        assert_eq!(
            sent.iter().map(|e| e.0).collect::<Vec<_>>(),
            [0, 10].map(|o| Offset::new(o, span).unwrap())
        );
    }

    /// Keeps every batch's events with the time each was sent at.
    struct Timed(Arc<std::sync::Mutex<Vec<Outgoing>>>);
    impl tutti_midi_runtime::MidiOut for Timed {
        fn queue(&self, events: &[UmpAt]) -> usize {
            events.len()
        }
        fn queue_timed(&self, events: &[Outgoing]) -> usize {
            self.0.lock().unwrap().extend_from_slice(events);
            events.len()
        }
    }

    fn timing_at(t0: Instant, terms: OutputTerms) -> OutputTiming {
        OutputTiming {
            map: Some(ClockMap::new(
                SampleRate::SR_48K,
                tutti_midi_runtime::ClockReading::new(t0, Frame::ZERO),
            )),
            now: t0 - Duration::from_millis(10),
            terms,
        }
    }

    fn taken(log: &Arc<std::sync::Mutex<Vec<Outgoing>>>) -> Vec<Outgoing> {
        log.lock().unwrap().clone()
    }

    fn timed_out() -> (UmpOut, Arc<std::sync::Mutex<Vec<Outgoing>>>) {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let out = UmpOut::new(Box::new(Timed(log.clone())), SampleRate::SR_48K);
        (out, log)
    }

    /// **Events are sent at the instants their frames have, spaced as the
    /// frames are.** Ticks 1 000 frames apart at 48 kHz are 20.833 ms apart in
    /// `SendAt::At`, and the first is where the clock puts frame 1 000.
    #[test]
    fn frames_are_sent_at_their_instants_spaced_by_the_rate() {
        let t0 = Instant::now() + Duration::from_secs(60);
        let (mut out, log) = timed_out();
        let timing = timing_at(t0, OutputTerms::default());
        let refused = out.send_frames(&[at(1_000), at(2_000), at(3_000)], Some(&timing));
        assert_eq!(refused, 0);
        let sends: Vec<SendAt> = log.lock().unwrap().iter().map(|e| e.send).collect();
        let SendAt::At(first) = sends[0] else {
            panic!("not scheduled: {sends:?}")
        };
        assert_eq!(Some(first), timing.map.unwrap().os_at(Frame::new(1_000)));
        for pair in sends.windows(2) {
            let (SendAt::At(a), SendAt::At(b)) = (pair[0], pair[1]) else {
                panic!("not scheduled: {pair:?}")
            };
            let gap = b.duration_since(a).as_secs_f64();
            assert!((gap - 1_000.0 / 48_000.0).abs() < 1e-6, "{gap}");
        }
    }

    /// **An output's offset shifts every event by exactly it**, and the audio
    /// latency and the device's go in as the plan says.
    #[test]
    fn the_terms_shift_every_event() {
        let t0 = Instant::now() + Duration::from_secs(60);
        let sent_with = |terms: OutputTerms| {
            let (mut out, log) = timed_out();
            out.send_frames(&[at(1_000), at(2_000)], Some(&timing_at(t0, terms)));
            taken(&log)
        };
        let base = sent_with(OutputTerms::default());
        let terms = OutputTerms {
            audio_output: Some(Duration::from_millis(30)),
            latency: OutputLatency::new(
                Duration::from_millis(5),
                tutti_midi_runtime::OutputOffset::new(tutti_types::Seconds(0.25)).unwrap(),
            )
            .unwrap(),
        };
        let shifted = sent_with(terms);
        for (a, b) in base.iter().zip(&shifted) {
            let (SendAt::At(a), SendAt::At(b)) = (a.send, b.send) else {
                panic!("not scheduled")
            };
            assert_eq!(b.duration_since(a), Duration::from_millis(30 - 5 + 250));
        }
    }

    /// **A JR stamp is sent with its event**, so a stamping output schedules
    /// the pair together and the stamps keep the spacing.
    #[test]
    fn a_jr_stamp_is_sent_at_its_events_time() {
        let t0 = Instant::now() + Duration::from_secs(60);
        let (mut out, log) = timed_out();
        out.set_stamping(true);
        let timing = timing_at(t0, OutputTerms::default());
        out.send_frames(&[at(1_000), at(2_000)], Some(&timing));
        let sent = log.lock().unwrap().clone();
        let events: Vec<&Outgoing> = sent
            .iter()
            .filter(|e| e.ump == note(0).1 || e.ump.message_type() == 0)
            .collect();
        assert!(events.len() >= 4, "a stamp for each event");
        for pair in sent.windows(2) {
            if pair[0].ump.message_type() == 0 && pair[1].ump == note(0).1 {
                assert_eq!(pair[0].send, pair[1].send, "a stamp rides with its event");
            }
        }
        assert!(
            sent.iter()
                .all(|e| matches!(e.send, SendAt::At(_)) || e.ump.message_type() == 0),
            "every note is scheduled"
        );
    }

    /// **An event beyond the cap is not sent and is counted**; one within it
    /// goes.
    #[test]
    fn an_event_beyond_the_cap_is_refused_and_counted() {
        let t0 = Instant::now() + Duration::from_secs(60);
        let (mut out, log) = timed_out();
        let drops = MidiOutDrops::default();
        let timing = timing_at(t0, OutputTerms::default());
        let beyond = 48_000 * 3;
        MidiOutRouter {
            out: Some(&mut out),
            drops: Some(&drops),
            timing: Some(&timing),
        }
        .route(&[at(1_000), at(beyond)]);
        assert_eq!(drops.count(), 1);
        assert_eq!(log.lock().unwrap().len(), 1, "only the one within the cap");
    }

    /// With no timing, every event is sent at once, as before.
    #[test]
    fn without_timing_every_event_is_sent_now() {
        let (mut out, log) = timed_out();
        out.send_frames(&[at(1_000), at(2_000)], None);
        assert!(log.lock().unwrap().iter().all(|e| e.send == SendAt::Now));
    }
}
