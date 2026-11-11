//! MIDI in the session (design 015 §4.3): clips, live inputs, the hardware
//! input's routes and the hardware out, on an offline session the test
//! renders. A [`Tap`] stands in for a synth: it logs every MIDI event its
//! event input receives with its absolute frame.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tutti_graph::UmpAt;
use tutti_types::MaxBlock;

use std::time::{Duration, Instant};
use tutti_fixtures::midi;
use tutti_graph::NodeKey;
use tutti_graph::{Cx, EventKind, IntoNode, Io, Node, NodeParts, Prepare, Shape, Status};
#[cfg(feature = "midi-hardware")]
use tutti_midi_hardware::{Direction, EndpointId};
use tutti_midi_runtime::{CHANNELLESS_PORT, LIVE_INLET_CAPACITY, MIDI_INPUT_PORTS, TimedMidiEvent};
use tutti_transport::MotionEvent;
use tutti_types::{
    At, Beat, BeatDuration, ChannelLayout, Frame, InterleavedMut, SampleRate, Samples,
};
use tutti_ump::Ump;
use tutti_ump::{KeyEvent, MidiChannel, MidiGroup, NoteNumber};

use tutti_session::{
    Error, EventSource, Feeder, MIDI_OUT_DROP_REPORT, MidiRouteRule, PHASES, PhaseCx, SendError,
    Session, SessionEvent, SessionNode, UmpOut,
};

/// Every phase at `now`: one update, at a time the test picks.
fn update_at(s: &mut Session, now: Instant) -> Vec<SessionEvent> {
    let mut out = Vec::new();
    for &phase in PHASES {
        s.run_phase(phase, PhaseCx::at(now), &mut out);
    }
    out
}

const RATE: f64 = 48_000.0;
const MF: u16 = 0x8000;

/// `(absolute frame, event)` of every MIDI event a [`Tap`] received.
type Seen = Arc<Mutex<Vec<(u64, Ump)>>>;

/// A stand-in synth: one MIDI event input, logged with each event's
/// absolute frame.
struct Tap(Seen);

impl Node for Tap {
    fn shape(&self) -> Shape {
        Shape::EMPTY.with_events(1, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, io: Io<'_>) -> Status {
        let mut seen = self.0.lock().unwrap();
        for e in io.events(0) {
            if let EventKind::Midi(Ump(data)) = e.kind {
                let at = cx.env.frame.get() + u64::from(e.offset.get());
                seen.push((at, Ump(data)));
            }
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

impl IntoNode for Tap {
    type Controls = ();
    fn into_parts(self) -> NodeParts<()> {
        IntoNode::into_parts(tutti_graph::Unforkable(self))
    }
}

impl SessionNode for Tap {}

/// An offline session at 48 kHz, stereo: the builder's MIDI nodes in.
fn offline() -> Session {
    Session::builder()
        .build_offline(
            SampleRate(RATE),
            ChannelLayout::STEREO,
            MaxBlock::of(Samples(1_024)),
        )
        .expect("builds")
}

/// A tap in `s`'s graph, and what it hears.
fn tap(s: &mut Session) -> (NodeKey, Seen) {
    let seen = Seen::default();
    let (node, ()) = s.graph().add(Tap(Arc::clone(&seen)));
    (node, seen)
}

/// A note-on/note-off pair as timed events.
fn note(number: u8, start: Beat, duration: BeatDuration) -> [TimedMidiEvent; 2] {
    [
        TimedMidiEvent::new(
            start,
            midi::note_on(MidiGroup::FIRST, MidiChannel::FIRST, number, MF),
        ),
        TimedMidiEvent::new(
            start + duration,
            midi::note_off(MidiGroup::FIRST, MidiChannel::FIRST, number, 0),
        ),
    ]
}

/// Plays the transport from beat 0 and renders `frames` frames in 512-frame
/// blocks.
fn roll(s: &Session, frames: usize) {
    s.transport()
        .motion
        .try_send(MotionEvent::Play)
        .expect("room");
    let mut buf = vec![0.0f32; 512 * 2];
    for _ in 0..frames.div_ceil(512) {
        s.render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO));
    }
}

fn note_ons(seen: &Seen) -> Vec<(u64, u8)> {
    seen.lock()
        .unwrap()
        .iter()
        .filter_map(|&(f, e)| match e.key_event() {
            Some(KeyEvent::On(key)) => Some((f, key.note())),
            _ => None,
        })
        .collect()
}

/// The nodes the last plan sent holds.
fn plan_nodes(s: &Session) -> usize {
    s.graph_ref().sent_plan().expect("a plan").order().len()
}

/// **A clip lands its note on its frame**: a third of a beat in at 120 BPM
/// / 48 kHz is frame 8 000, inside a block.
#[test]
fn a_clip_plays_its_note_on_its_frame() {
    let mut s = offline();
    let (synth, seen) = tap(&mut s);
    s.midi()
        .play_clip(synth, note(60, Beat(1.0 / 3.0), BeatDuration(1.0)))
        .expect("the tap takes MIDI");
    s.update();
    roll(&s, 9_000);
    assert_eq!(note_ons(&seen), vec![(8_000, 60)]);
}

/// **A clip edit updates in place**: a second `play_clip` on one target
/// replaces its clip's events, and the graph's node count stays the same.
/// `stop_clip` empties it and keeps it.
#[test]
fn a_clip_edit_updates_in_place() {
    let mut s = offline();
    let (synth, seen) = tap(&mut s);
    let clip = s
        .midi()
        .play_clip(synth, note(60, Beat(0.0), BeatDuration(1.0)))
        .unwrap();
    s.update();
    let nodes = plan_nodes(&s);

    let edited = s
        .midi()
        .play_clip(synth, note(64, Beat(1.0 / 3.0), BeatDuration(1.0)))
        .unwrap();
    s.update();
    assert_eq!(edited, clip, "the same clip");
    assert_eq!(plan_nodes(&s), nodes, "and no node more");
    roll(&s, 9_000);
    assert_eq!(note_ons(&seen), vec![(8_000, 64)], "playing the new events");

    assert!(s.midi().stop_clip(synth));
    s.update();
    assert_eq!(s.midi().clip(synth), Some(clip), "the emptied clip stays");
    assert!(s.graph_ref().contains(clip));
}

/// **A target that takes no MIDI is refused**: a node with no event input,
/// and one not in the graph.
#[test]
fn a_target_without_an_event_input_takes_no_clip() {
    let mut s = offline();
    let (dc, _) = s
        .graph()
        .add(tutti_graph::ForkByClone(tutti_nodes::testing::Const::mono(
            0.5,
        )));
    assert!(matches!(
        s.midi().play_clip(dc, []),
        Err(Error::NoEventInput(n)) if n == dc
    ));
    s.remove(dc);
    assert!(matches!(
        s.midi().live_input(dc),
        Err(Error::NoEventInput(_))
    ));
}

/// **A clip and a live input survive their target's re-bind** (a past
/// regression, at the session): the target is removed, a new node takes
/// its place, and the host re-aims both at it. The same clip and queue
/// nodes play the new node, and the removed one hears nothing.
#[test]
fn a_clip_and_a_live_input_survive_a_re_bind() {
    let mut s = offline();
    let (old, old_seen) = tap(&mut s);
    let clip = s
        .midi()
        .play_clip(old, note(60, Beat(1.0 / 3.0), BeatDuration(1.0)))
        .unwrap();
    let live = s.midi().live_input(old).unwrap();
    s.update();

    assert!(s.remove(old));
    let (new, new_seen) = tap(&mut s);
    s.midi().retarget(clip, new).expect("the clip re-aims");
    s.midi()
        .retarget(live.node, new)
        .expect("the queue re-aims");
    s.update();
    assert_eq!(s.midi().clip(new), Some(clip), "the same clip");
    assert_eq!(s.midi().live_input(new).unwrap().node, live.node);
    let mut fed = s.graph_ref().event_sources(new, 0);
    fed.sort_by_key(|e| e.node);
    let mut want = vec![EventSource::from(clip), EventSource::from(live.node)];
    want.sort_by_key(|e| e.node);
    assert_eq!(fed, want);

    live.send(
        At::NextBlock,
        midi::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 72, MF),
    )
    .unwrap();
    roll(&s, 9_000);
    let heard = note_ons(&new_seen);
    assert!(heard.contains(&(8_000, 60)), "the clip plays it: {heard:?}");
    assert!(
        heard.iter().any(|&(_, n)| n == 72),
        "the keyboard plays it: {heard:?}"
    );
    assert!(
        note_ons(&old_seen).is_empty(),
        "the removed node hears nothing"
    );
}

/// **A live input's clones share one writer and send in order**: the key
/// helpers build group-1 MIDI 2.0 notes, a frame is held until its block, a
/// beat is refused and a full ring says so.
#[test]
fn a_live_inputs_clones_send_in_order_through_one_writer() {
    let mut s = offline();
    let (target, seen) = tap(&mut s);
    let a = s.midi().live_input(target).unwrap();
    let b = s.midi().live_input(target).unwrap();
    assert_eq!(a.node, b.node, "one inlet");
    s.update();

    a.note_on(At::NextBlock, MidiChannel::FIRST, NoteNumber::of(60), MF)
        .unwrap();
    b.note_on(At::NextBlock, MidiChannel::FIRST, NoteNumber::of(62), MF)
        .unwrap();
    b.note_on(
        At::Frame(Frame(3_000)),
        MidiChannel::FIRST,
        NoteNumber::of(64),
        MF,
    )
    .unwrap();
    assert_eq!(
        a.send(
            At::Beat(Beat(1.0)),
            midi::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 66, MF)
        ),
        Err(SendError::Beat)
    );
    roll(&s, 4_000);
    let heard = note_ons(&seen);
    assert_eq!(
        heard.iter().map(|&(_, n)| n).collect::<Vec<_>>(),
        [60, 62, 64]
    );
    assert!(heard[0].0 < 512 && heard[1].0 < 512, "{heard:?}");
    assert_eq!(heard[2].0, 3_000);

    for _ in 0..LIVE_INLET_CAPACITY {
        a.note_off(At::NextBlock, MidiChannel::FIRST, NoteNumber::of(60))
            .unwrap();
    }
    assert_eq!(
        b.note_off(At::NextBlock, MidiChannel::FIRST, NoteNumber::of(60)),
        Err(SendError::Full)
    );
}

/// **A route rule feeds its targets the hardware input's ports**: an
/// any-channel rule all of them, a channel rule its channel's and the
/// channelless one; the fallback the ports no rule covers; unrouting
/// unwires.
#[test]
fn route_rules_wire_the_hardware_input() {
    let mut s = offline();
    let input = s.midi_nodes().expect("the builder's MIDI nodes").input;
    let (a, _) = tap(&mut s);
    let (b, _) = tap(&mut s);
    let ports = |s: &Session, n: NodeKey| -> Vec<u16> {
        let mut p: Vec<u16> = s
            .graph_ref()
            .feed(n, Feeder::Routes)
            .iter()
            .map(|e| {
                assert_eq!(e.node, input);
                e.port
            })
            .collect();
        p.sort();
        p
    };

    let all = s.midi().route(MidiRouteRule::any_channel().to(a)).unwrap();
    assert_eq!(
        ports(&s, a),
        (0..MIDI_INPUT_PORTS as u16).collect::<Vec<_>>()
    );

    let ch2 = MidiChannel::of(2);
    s.midi()
        .route(MidiRouteRule::for_channel(ch2).to(b))
        .unwrap();
    assert_eq!(ports(&s, b), vec![2, CHANNELLESS_PORT as u16]);

    assert!(s.midi().unroute(all));
    assert_eq!(ports(&s, a), Vec::<u16>::new(), "unrouted");

    s.midi().set_route_fallback(Some(a)).unwrap();
    let rest: Vec<u16> = (0..MIDI_INPUT_PORTS as u16)
        .filter(|&p| p != 2 && p != CHANNELLESS_PORT as u16)
        .collect();
    assert_eq!(ports(&s, a), rest, "the fallback takes what no rule covers");
}

/// **A route rule names a Group, and the engine's input hears the Groups the
/// builder asks for** (M2-104 Appendix B.1): channel 2 of Group 2 reaches its
/// own listener through Group 2's ports, not Group 1's channel 2, and a rule
/// for a Group the input does not hear wires nothing.
#[test]
fn a_route_rule_names_a_group() {
    let mut s = Session::builder()
        .midi_groups(MidiGroup::of(1))
        .build_offline(
            SampleRate(RATE),
            ChannelLayout::STEREO,
            MaxBlock::of(Samples(1_024)),
        )
        .expect("builds");
    let input = s.midi_nodes().expect("the builder's MIDI nodes").input;
    let (a, _) = tap(&mut s);
    let (b, _) = tap(&mut s);
    let (c, _) = tap(&mut s);
    let ports = |s: &Session, n: NodeKey| -> Vec<u16> {
        let mut p: Vec<u16> = s
            .graph_ref()
            .feed(n, Feeder::Routes)
            .iter()
            .map(|e| {
                assert_eq!(e.node, input);
                e.port
            })
            .collect();
        p.sort();
        p
    };
    let ch2 = MidiChannel::of(2);
    let group = MIDI_INPUT_PORTS as u16;
    s.midi()
        .route(
            MidiRouteRule::for_channel(ch2)
                .in_group(MidiGroup::of(1))
                .to(a),
        )
        .unwrap();
    assert_eq!(
        ports(&s, a),
        vec![group + 2, group + CHANNELLESS_PORT as u16]
    );
    s.midi()
        .route(MidiRouteRule::for_channel(ch2).to(b))
        .unwrap();
    assert_eq!(ports(&s, b), vec![2, CHANNELLESS_PORT as u16]);
    s.midi()
        .route(
            MidiRouteRule::for_channel(ch2)
                .in_group(MidiGroup::of(2))
                .to(c),
        )
        .unwrap();
    assert_eq!(ports(&s, c), Vec::<u16>::new(), "a Group not heard");
}

/// **The routes have one declarer.** While the session's own rules are
/// declared, a host's `set_routes` is refused and changes nothing; once
/// they are unrouted it applies, and then the session's own `route` and
/// `set_route_fallback` are refused, until a `set_routes` with no rule and
/// no fallback hands the routes back.
#[test]
fn the_routes_have_one_declarer() {
    let mut s = offline();
    let input = s.midi_nodes().expect("the builder's MIDI nodes").input;
    let (a, _) = tap(&mut s);
    let (b, _) = tap(&mut s);
    let fed = |s: &Session, n: NodeKey| !s.graph_ref().feed(n, Feeder::Routes).is_empty();

    let own = s.midi().route(MidiRouteRule::any_channel().to(a)).unwrap();
    let host = [MidiRouteRule::<u8>::any_channel().to(1)];
    let resolve = |k: u8| (k == 1).then_some(b);
    assert!(matches!(
        s.midi().set_routes(input, &host, None, resolve),
        Err(Error::TwoRouteDeclarers(_))
    ));
    assert!(fed(&s, a) && !fed(&s, b), "the refusal changed nothing");

    assert!(s.midi().unroute(own));
    s.midi().set_routes(input, &host, None, resolve).unwrap();
    assert!(fed(&s, b), "the host's routes apply");
    assert!(matches!(
        s.midi().route(MidiRouteRule::any_channel().to(a)),
        Err(Error::TwoRouteDeclarers(_))
    ));
    assert!(matches!(
        s.midi().set_route_fallback(Some(a)),
        Err(Error::TwoRouteDeclarers(_))
    ));
    assert!(!fed(&s, a) && fed(&s, b), "the refusals changed nothing");

    s.midi()
        .set_routes(input, &[] as &[MidiRouteRule<u8>], None, resolve)
        .unwrap();
    assert!(!fed(&s, b), "handed back: nothing routed");
    s.midi().route(MidiRouteRule::any_channel().to(a)).unwrap();
    assert!(fed(&s, a), "the session declares again");
}

/// Counts what reached the wire.
struct CountingSink(Arc<AtomicUsize>);
impl tutti_midi_runtime::MidiOut for CountingSink {
    fn queue(&self, events: &[UmpAt]) -> usize {
        self.0.fetch_add(events.len(), Ordering::SeqCst);
        events.len()
    }
}

/// The builder's clock, enabled, rolling for half a second: its Beat Clock
/// reaches the hardware out node.
fn clocked() -> Session {
    let mut s = offline();
    s.update(); // the builder's nodes, committed
    s.midi_nodes().unwrap().clock_master.set_enabled(true);
    roll(&s, 24_000);
    s
}

/// **The hardware out drains to the output with stamping off**: the
/// `MidiOut` phase routes the clock's ticks to the output whether or not
/// it JR-stamps (stamping gates timestamps, not delivery), and with no
/// output it counts them as dropped and says so when the count rises
/// (rate-limited: `dropped_ticks_are_reported_at_most_once_a_second`).
#[test]
fn the_hardware_out_drains_with_stamping_off() {
    let mut s = clocked();
    let count = Arc::new(AtomicUsize::new(0));
    s.midi().set_hardware_out(Some(UmpOut::new(
        Box::new(CountingSink(Arc::clone(&count))),
        SampleRate::new(RATE),
    )));
    let events = s.update();
    assert!(
        count.load(Ordering::SeqCst) > 0,
        "the clock reached the sink, unstamped"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SessionEvent::MidiOutDropped(_)))
    );

    let mut s = clocked();
    let events = s.update();
    let dropped = s.midi().out_drops();
    assert!(dropped > 0, "no output: the ticks are dropped, counted");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SessionEvent::MidiOutDropped(n) if *n == dropped)),
        "{events:?}"
    );
    assert!(
        !s.update()
            .iter()
            .any(|e| matches!(e, SessionEvent::MidiOutDropped(_))),
        "reported when the count rises, not every update"
    );
}

/// **A clock dropping ticks with no output is reported once per
/// `MIDI_OUT_DROP_REPORT`, not every update.** The first update that drops
/// reports the total; later updates within the second drop more and report
/// nothing; the first update a second after the report reports the new
/// total.
#[test]
fn dropped_ticks_are_reported_at_most_once_a_second() {
    let mut s = clocked();
    let now = Instant::now();
    let drops = |events: &[SessionEvent]| -> Vec<u64> {
        events
            .iter()
            .filter_map(|e| match e {
                SessionEvent::MidiOutDropped(n) => Some(*n),
                _ => None,
            })
            .collect()
    };
    let first = drops(&update_at(&mut s, now));
    assert_eq!(
        first,
        vec![s.midi().out_drops()],
        "the first drop, reported"
    );

    for at in [
        Duration::ZERO,
        Duration::from_millis(500),
        Duration::from_millis(999),
    ] {
        roll(&s, 24_000);
        let before = s.midi().out_drops();
        assert!(drops(&update_at(&mut s, now + at)).is_empty(), "at {at:?}");
        assert!(
            s.midi().out_drops() > before,
            "precondition: it dropped more"
        );
    }

    roll(&s, 24_000);
    let later = drops(&update_at(&mut s, now + MIDI_OUT_DROP_REPORT));
    assert_eq!(
        later,
        vec![s.midi().out_drops()],
        "a second on, the new total"
    );
}

/// **The session's own MIDI nodes go in through `add`**, so the reclaim
/// phase sweeps their published cells: a clip `play_clip` made (its events)
/// and the builder's live input (its MPE mode). What each reclaimer frees is
/// pinned in `tutti-midi-runtime` with a held loan, and the sweep of the
/// graph's reclaimers in `tests/graph.rs`.
#[test]
fn the_sessions_midi_nodes_keep_their_reclaimers() {
    let mut s = offline();
    let (synth, _) = tap(&mut s);
    let clip = s
        .midi()
        .play_clip(synth, note(60, Beat(0.0), BeatDuration(1.0)))
        .expect("the tap takes MIDI");
    assert!(s.graph_ref().keeps_reclaimer(clip), "the clip's");
    let input = s.midi_nodes().expect("the builder's MIDI nodes").input;
    assert!(s.graph_ref().keeps_reclaimer(input), "the live input's");
    assert!(
        !s.graph_ref().keeps_reclaimer(synth),
        "a tap publishes nothing"
    );
}

/// A session over a fake OS with no devices, and the handle that plugs some.
#[cfg(feature = "midi-hardware")]
fn hot_plug_session() -> (Session, tutti_midi_hardware::test_support::FakeCounters) {
    let (backend, fake) =
        tutti_midi_hardware::test_support::FakeBackend::build_with(vec![], vec![], usize::MAX);
    let s = Session::builder()
        .midi_hardware_over(backend)
        .build_offline(
            SampleRate(RATE),
            ChannelLayout::STEREO,
            MaxBlock::of(Samples(1_024)),
        )
        .expect("builds");
    (s, fake)
}

/// The `MidiDevice` events of an update, as `(kind, device name)`.
#[cfg(feature = "midi-hardware")]
fn device_events(events: &[SessionEvent]) -> Vec<(&'static str, String)> {
    use tutti_midi_hardware::DeviceEvent;
    events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::MidiDevice(DeviceEvent::Added(d)) => Some(("added", d.name.clone())),
            SessionEvent::MidiDevice(DeviceEvent::Removed(d)) => Some(("removed", d.name.clone())),
            SessionEvent::MidiDevice(DeviceEvent::Changed(d)) => Some(("changed", d.name.clone())),
            SessionEvent::MidiInputLost(d) => Some(("input lost", d.name.clone())),
            SessionEvent::MidiOutputLost(d) => Some(("output lost", d.name.clone())),
            _ => None,
        })
        .collect()
}

#[cfg(feature = "midi-hardware")]
fn plug(
    fake: &tutti_midi_hardware::test_support::FakeCounters,
    raw: u64,
    name: &str,
    d: Direction,
) {
    fake.plug(tutti_midi_hardware::test_support::endpoint(raw, name), d);
}

/// **A device that comes and goes reaches `Session::update` as an event**,
/// in the order the OS reported, on a session with no hardware and a fake OS.
#[cfg(feature = "midi-hardware")]
#[test]
fn a_device_that_comes_and_goes_is_a_session_event() {
    let (mut s, fake) = hot_plug_session();
    assert!(device_events(&s.update()).is_empty(), "nothing yet");

    plug(&fake, 1, "Keys", Direction::Input);
    assert_eq!(device_events(&s.update()), [("added", "Keys".to_string())]);
    assert!(device_events(&s.update()).is_empty(), "reported once");

    fake.unplug(EndpointId::from_raw(1));
    assert_eq!(
        device_events(&s.update()),
        [("removed", "Keys".to_string())],
        "unheld, so nothing is retired"
    );
}

/// **A held input whose device vanishes is retired** (audit #4): before this
/// the device stayed listed as connected and nothing fired. Now the update
/// reports the removal and then the lost input, the connection is closed and
/// its port is gone.
#[cfg(feature = "midi-hardware")]
#[test]
fn a_held_input_that_vanishes_is_retired_and_reported() {
    let (mut s, fake) = hot_plug_session();
    plug(&fake, 1, "Keys", Direction::Input);
    s.update();
    let io = s.midi_io().expect("the OS MIDI layer").clone();
    io.connect_input_by_name("keys").expect("connects");
    assert_eq!(io.connected_input_names(), ["Keys"]);
    assert_eq!(io.ports().list_input_ports().len(), 1);

    fake.unplug(EndpointId::from_raw(1));
    assert_eq!(
        device_events(&s.update()),
        [
            ("removed", "Keys".to_string()),
            ("input lost", "Keys".to_string())
        ]
    );
    assert!(
        !io.is_any_input_connected(),
        "no longer listed as connected"
    );
    assert!(io.connected_input_names().is_empty());
    assert!(io.ports().list_input_ports().is_empty(), "the port is gone");
}

/// A session over a fake OS with the device `Keys` (id 1) and `Pads` (id 2)
/// present, and a stand-in synth that every channel of the hardware input
/// routes to.
#[cfg(feature = "midi-hardware")]
fn keyboards(
    devices: usize,
) -> (
    Session,
    tutti_midi_hardware::test_support::FakeCounters,
    Seen,
) {
    let (backend, fake) = tutti_midi_hardware::test_support::FakeBackend::build_with(
        vec![
            tutti_midi_hardware::test_support::endpoint(1, "Keys"),
            tutti_midi_hardware::test_support::endpoint(2, "Pads"),
        ],
        vec![],
        usize::MAX,
    );
    let mut s = Session::builder()
        .midi_hardware_over(backend)
        .midi_devices(devices)
        .build_offline(
            SampleRate(RATE),
            ChannelLayout::STEREO,
            MaxBlock::of(Samples(1_024)),
        )
        .expect("builds");
    let (synth, seen) = tap(&mut s);
    s.midi()
        .route(MidiRouteRule::any_channel().to(synth))
        .expect("routes");
    s.update();
    (s, fake, seen)
}

/// Connects `name`, and updates: the `Poll` phase wires the new inlet.
#[cfg(feature = "midi-hardware")]
fn connect(s: &mut Session, name: &str) {
    s.midi_io()
        .expect("the OS MIDI layer")
        .connect_input_by_name(name)
        .expect("connects");
    s.update();
}

/// A note-on on channel `channel`.
#[cfg(feature = "midi-hardware")]
fn key(channel: u8, n: u8) -> Ump {
    midi::note_on(MidiGroup::FIRST, MidiChannel::of(channel), n, MF)
}

/// **A connected device's note reaches the synth its channel routes to**
/// through the device's own event inlet, once: the update after the connection
/// wires the inlet to the ingest node (doc 017 stage E5), the device's thread
/// pushes, and the synth hears it.
#[cfg(feature = "midi-hardware")]
#[test]
fn a_connected_devices_note_reaches_the_synth_through_its_inlet() {
    let (mut s, fake, seen) = keyboards(4);
    connect(&mut s, "keys");
    roll(&s, 1_024);
    assert!(fake.send_input(EndpointId::from_raw(1), key(2, 60), Instant::now()));
    roll(&s, 8_192);
    let heard = note_ons(&seen);
    assert_eq!(heard.len(), 1, "once: {heard:?}");
    assert_eq!(heard[0].1, 60);
}

/// **Two devices are two inlets, merged at the synth**: each device's note is
/// heard, and one device going leaves the other's inlet in the graph.
#[cfg(feature = "midi-hardware")]
#[test]
fn two_devices_are_two_inlets_and_one_leaving_takes_only_its_own() {
    let (mut s, fake, seen) = keyboards(4);
    let before = plan_nodes(&s);
    connect(&mut s, "keys");
    connect(&mut s, "pads");
    assert_eq!(plan_nodes(&s), before + 2, "an inlet for each device");
    roll(&s, 1_024);
    assert!(fake.send_input(EndpointId::from_raw(1), key(0, 60), Instant::now()));
    assert!(fake.send_input(EndpointId::from_raw(2), key(0, 64), Instant::now()));
    roll(&s, 8_192);
    let mut notes: Vec<u8> = note_ons(&seen).iter().map(|n| n.1).collect();
    notes.sort_unstable();
    assert_eq!(notes, vec![60, 64], "both devices are heard");

    s.midi_io()
        .expect("the OS MIDI layer")
        .disconnect_input_by_name("keys");
    s.update();
    assert_eq!(
        plan_nodes(&s),
        before + 1,
        "only the closed device's inlet left"
    );
    assert!(
        fake.send_input(EndpointId::from_raw(2), key(0, 65), Instant::now()),
        "the other device is still connected"
    );
}

/// **An arrival is placed on the engine's clock**: a note the OS stamps one
/// second ahead of now sounds a second of frames on, not at the next block.
/// An event pushed for the next block (the path of a session with no clock)
/// sounds within one.
#[cfg(feature = "midi-hardware")]
#[test]
fn a_devices_arrival_is_placed_on_the_engines_clock() {
    let (mut s, fake, seen) = keyboards(4);
    connect(&mut s, "keys");
    roll(&s, 1_024);
    let ahead = Instant::now() + Duration::from_secs(1);
    assert!(fake.send_input(EndpointId::from_raw(1), key(0, 60), ahead));
    roll(&s, 4_096);
    assert!(note_ons(&seen).is_empty(), "not yet: it is a second away");
    roll(&s, 60_000);
    let heard = note_ons(&seen);
    assert_eq!(heard.len(), 1);
    assert!(
        heard[0].0 >= 48_000,
        "a second of frames on, not at the next block: frame {}",
        heard[0].0
    );
}

/// **A device whose inlet overflows is reported**: 3,000 events pushed before
/// any block reads them against an inlet of 2,048 lose 952, and the next update
/// reports `MidiInputDropped` with that total, once.
#[cfg(feature = "midi-hardware")]
#[test]
fn a_device_whose_inlet_overflows_is_reported() {
    let (mut s, fake, _seen) = keyboards(4);
    connect(&mut s, "keys");
    for n in 0..3_000u32 {
        fake.send_input(
            EndpointId::from_raw(1),
            key(0, (n % 128) as u8),
            Instant::now(),
        );
    }
    let dropped = |events: Vec<SessionEvent>| {
        events
            .into_iter()
            .filter_map(|e| match e {
                SessionEvent::MidiInputDropped(total) => Some(total),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(dropped(s.update()), vec![952]);
    assert!(dropped(s.update()).is_empty(), "reported once");
}

/// **A device past the slots is refused and leaves nothing**: with one slot,
/// the second connection fails (`NoFreeSlot`) and holds no port; the first is
/// still heard.
#[cfg(feature = "midi-hardware")]
#[test]
fn a_device_past_the_slots_is_refused() {
    let (mut s, fake, seen) = keyboards(1);
    connect(&mut s, "keys");
    let io = s.midi_io().expect("the OS MIDI layer").clone();
    let refused = io.connect_input_by_name("pads").unwrap_err();
    assert!(
        matches!(refused, tutti_midi_hardware::Error::NoFreeSlot(_)),
        "{refused:?}"
    );
    assert_eq!(io.ports().list_input_ports().len(), 1);
    roll(&s, 1_024);
    assert!(fake.send_input(EndpointId::from_raw(1), key(0, 60), Instant::now()));
    roll(&s, 8_192);
    assert_eq!(note_ons(&seen).len(), 1);
}

/// **A held output whose device vanishes is closed and reported.**
#[cfg(feature = "midi-hardware")]
#[test]
fn a_held_output_that_vanishes_is_closed_and_reported() {
    let (mut s, fake) = hot_plug_session();
    plug(&fake, 10, "Synth", Direction::Output);
    s.update();
    let io = s.midi_io().expect("the OS MIDI layer").clone();
    io.connect_output_by_name("synth").expect("connects");
    assert!(io.is_output_connected());

    fake.unplug(EndpointId::from_raw(10));
    assert_eq!(
        device_events(&s.update()),
        [
            ("removed", "Synth".to_string()),
            ("output lost", "Synth".to_string())
        ]
    );
    assert!(!io.is_output_connected());
}

/// **A watch that dies is reported once**, with its reason: the list is frozen
/// and the host is told so instead of seeing a quiet table.
#[cfg(feature = "midi-hardware")]
#[test]
fn a_dead_device_watch_is_reported_once() {
    let (mut s, fake) = hot_plug_session();
    let failed = |events: &[SessionEvent]| {
        events
            .iter()
            .filter_map(|e| match e {
                SessionEvent::MidiWatchFailed(why) => Some(why.to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert!(failed(&s.update()).is_empty());
    fake.fail_watch("the sequencer went away");
    assert_eq!(
        failed(&s.update()),
        ["MIDI port error: the sequencer went away"]
    );
    assert!(failed(&s.update()).is_empty(), "once");
}

/// **An app holding a session restores a remembered device through it.** The
/// fake OS names devices as ALSA does, so the saved device comes back as a
/// same-name match, and as `Missing` once it is gone.
#[cfg(feature = "midi-hardware")]
#[test]
fn a_remembered_device_resolves_through_the_session() {
    use tutti_midi_hardware::Resolved;
    let (mut s, fake) = hot_plug_session();
    plug(&fake, 1, "Keys", Direction::Input);
    s.update();
    let io = s.midi_io().expect("the OS MIDI layer").clone();
    let saved = io.devices().list().remove(0);
    assert_eq!(
        io.devices().resolve(&saved),
        Resolved::SameName(saved.clone())
    );
    fake.unplug(EndpointId::from_raw(1));
    s.update();
    assert_eq!(io.devices().resolve(&saved), Resolved::Missing);
}

/// **Another device's removal leaves a held input and output alone.**
#[cfg(feature = "midi-hardware")]
#[test]
fn another_devices_removal_leaves_a_held_input_open() {
    let (mut s, fake) = hot_plug_session();
    plug(&fake, 1, "Keys", Direction::Input);
    plug(&fake, 2, "Pads", Direction::Input);
    plug(&fake, 10, "Synth", Direction::Output);
    s.update();
    let io = s.midi_io().expect("the OS MIDI layer").clone();
    io.connect_input_by_name("keys").expect("connects");
    io.connect_output_by_name("synth").expect("connects");

    fake.unplug(EndpointId::from_raw(2));
    assert_eq!(
        device_events(&s.update()),
        [("removed", "Pads".to_string())]
    );
    assert_eq!(io.connected_input_names(), ["Keys"]);
    assert_eq!(io.ports().list_input_ports().len(), 1);
    assert!(io.is_output_connected());
}

/// **The feeds are partial, as audio's declarations are: an edge a host
/// wired on a fed event input stays.** A host wires its own clip into the
/// tap's event input 0 by hand; the session's sequencer then feeds the tap
/// too, and both notes play. A host's later `set_event_sources` on that
/// input sets its own edges beside the sequencer's; the sequencer's clip
/// re-aimed elsewhere leaves the host's edges where they are.
#[test]
fn a_hand_wired_event_edge_survives_the_sequencers_feed() {
    use tutti_midi_runtime::MidiClipNode;

    let sorted = |mut v: Vec<EventSource>| {
        v.sort_by_key(|e| (e.node.0, e.port));
        v
    };
    let mut s = offline();
    let (synth, seen) = tap(&mut s);
    let (other, _) = tap(&mut s);
    let (hand, _) = s.graph().add(MidiClipNode::new(note(
        62,
        Beat(1.0 / 3.0),
        BeatDuration(1.0),
    )));
    s.graph().set_event_sources(synth, 0, &[hand.into()]);
    let clip = s
        .midi()
        .play_clip(synth, note(60, Beat(1.0 / 3.0), BeatDuration(1.0)))
        .unwrap();
    s.update();
    assert_eq!(
        sorted(s.graph_ref().event_sources(synth, 0)),
        sorted(vec![hand.into(), clip.into()]),
        "the sequencer's clip joins the hand-wired one"
    );
    roll(&s, 9_000);
    let mut ons = note_ons(&seen);
    ons.sort_unstable();
    assert_eq!(ons, vec![(8_000, 60), (8_000, 62)], "both play");

    let (hand2, _) = s.graph().add(MidiClipNode::new([]));
    s.graph().set_event_sources(synth, 0, &[hand2.into()]);
    assert_eq!(
        sorted(s.graph_ref().event_sources(synth, 0)),
        sorted(vec![hand2.into(), clip.into()]),
        "the host's call sets its own edges, beside the feed's"
    );

    s.midi().retarget(clip, other).unwrap();
    assert_eq!(
        s.graph_ref().event_sources(synth, 0),
        vec![hand2.into()],
        "the feed's edge goes, the host's stays"
    );
}

/// **A port that loses events says so, as a session event**: a chord of 1,100
/// notes on beat 0 against a port of 1,024 (the session's block) has its last
/// 76 note-ons refused. The next update reports `EventsDropped` once, for the
/// clip's port 0, with count 76; a second update with no further loss reports
/// nothing.
#[test]
fn a_port_that_loses_events_is_reported() {
    let mut s = offline();
    let (synth, _seen) = tap(&mut s);
    let chord = (0..1_100u32).map(|i| {
        TimedMidiEvent::new(
            Beat(0.0),
            midi::note_on(
                MidiGroup::FIRST,
                MidiChannel::of(((i / 128) % 16) as u8),
                (i % 128) as u8,
                MF,
            ),
        )
    });
    s.midi().play_clip(synth, chord).expect("takes MIDI");
    s.update();
    roll(&s, 512);
    let lost: Vec<_> = s
        .update()
        .into_iter()
        .filter_map(|e| match e {
            SessionEvent::EventsDropped { port, count, .. } => Some((port, count)),
            _ => None,
        })
        .collect();
    assert_eq!(lost, vec![(Some(0), 76)]);
    assert!(
        s.update()
            .iter()
            .all(|e| !matches!(e, SessionEvent::EventsDropped { .. }))
    );
}
