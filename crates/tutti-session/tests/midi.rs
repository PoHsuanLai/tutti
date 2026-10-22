//! MIDI in the session (design 015 §4.3): clips, live inputs, the hardware
//! input's routes and the hardware out, on an offline session the test
//! renders. A [`Tap`] stands in for a synth: it logs every MIDI event its
//! event input receives with its absolute frame.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use std::time::{Duration, Instant};
use tutti_core::{
    AudioNode, Beat, BeatDuration, ChannelLayout, InterleavedMut, MotionEvent, SampleRate, Samples,
};
use tutti_graph::{Cx, EventKind, IntoNode, Io, Node, NodeParts, Prepare, Shape, Status, Ump};
use tutti_midi_runtime::{TimedMidiEvent, CHANNELLESS_PORT, MIDI_INPUT_PORTS};
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::{MidiChannel, MidiGroup};

use tutti_session::{
    Error, EventSource, MidiRouteRule, PhaseCx, Session, SessionEvent, SessionNode, UmpOut,
    MIDI_OUT_DROP_REPORT, PHASES,
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
type Seen = Arc<Mutex<Vec<(u64, MidiEvent)>>>;

/// A stand-in synth: one MIDI event input, logged with each event's
/// absolute frame.
struct Tap(Seen);

impl Node for Tap {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY).with_events(1, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, io: Io<'_>) -> Status {
        let mut seen = self.0.lock().unwrap();
        for e in io.events(0) {
            if let EventKind::Midi(Ump(data)) = e.kind {
                let at = cx.env.frame.get() + u64::from(e.offset.get());
                seen.push((
                    at,
                    MidiEvent {
                        frame_offset: e.offset.get(),
                        data,
                    },
                ));
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
        .build_offline(SampleRate(RATE), ChannelLayout::STEREO, Samples(1_024))
        .expect("builds")
}

/// A tap in `s`'s graph, and what it hears.
fn tap(s: &mut Session) -> (AudioNode, Seen) {
    let seen = Seen::default();
    let (node, ()) = s.graph().add(Tap(Arc::clone(&seen)));
    (node, seen)
}

/// A note-on/note-off pair as timed events.
fn note(number: u8, start: Beat, duration: BeatDuration) -> [TimedMidiEvent; 2] {
    [
        TimedMidiEvent::new(
            start,
            MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, number, MF),
        ),
        TimedMidiEvent::new(
            start + duration,
            MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, number, 0),
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
        .filter(|(_, e)| e.is_note_on())
        .map(|(f, e)| (*f, e.note().unwrap()))
        .collect()
}

/// The nodes the last plan sent holds.
fn plan_nodes(s: &Session) -> usize {
    s.graph_ref().sent_plan().expect("a plan").order().len()
}

/// **A clip lands its note on its frame**: a third of a beat in at 120 BPM
/// / 48 kHz is frame 8 000, inside a block.
///
/// Mutation (run): `Midi::play_clip` not feeding the target (`set_feed`
/// dropped) → nothing arrives → fails.
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
///
/// Mutation (run): `play_clip` inserting a new clip every call (ignoring
/// the one the target has) → one more node in the plan → fails.
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
///
/// Mutation (run): `Midi::takes_midi` checking only that the node is in
/// the graph → the `Const` takes a clip → fails.
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
///
/// Mutations (run), each fails:
/// - `Session::remove` removing the clip and queue feeding the removed
///   node → `retarget` refuses them (`NotSessionMidi`);
/// - `Midi::retarget` not feeding the new target (its `set_feed` dropped)
///   → the new tap hears nothing.
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

    live.queue(&[
        MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 72, MF).with_frame_offset(0),
    ]);
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

/// **A route rule feeds its targets the hardware input's ports**: an
/// any-channel rule all of them, a channel rule its channel's and the
/// channelless one; the fallback the ports no rule covers; unrouting
/// unwires.
///
/// Mutation (run): the route compile (`Midi::compile_routes`, which
/// `set_routes` and the session's own rules share) not unwiring a sink no
/// rule names any more → the unrouted tap keeps its ports → fails.
#[test]
fn route_rules_wire_the_hardware_input() {
    let mut s = offline();
    let input = s.midi_nodes().expect("the builder's MIDI nodes").input;
    let (a, _) = tap(&mut s);
    let (b, _) = tap(&mut s);
    let ports = |s: &Session, n: AudioNode| -> Vec<u16> {
        let mut p: Vec<u16> = s
            .graph_ref()
            .feed(n, "midi routes")
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

    let ch2 = MidiChannel::new(2);
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

/// **The routes have one declarer.** While the session's own rules are
/// declared, a host's `set_routes` is refused and changes nothing; once
/// they are unrouted it applies, and then the session's own `route` and
/// `set_route_fallback` are refused, until a `set_routes` with no rule and
/// no fallback hands the routes back.
///
/// Mutations (run), each fails:
/// - `set_routes` not refusing while own rules exist → the host's rule
///   replaces the session's (`a` loses its ports);
/// - `own_may_declare` always `Ok` → the session's `route` is accepted
///   while the host declares;
/// - `host_routes` not cleared by an empty `set_routes` → `route` is
///   still refused after the hand-back.
#[test]
fn the_routes_have_one_declarer() {
    let mut s = offline();
    let input = s.midi_nodes().expect("the builder's MIDI nodes").input;
    let (a, _) = tap(&mut s);
    let (b, _) = tap(&mut s);
    let fed = |s: &Session, n: AudioNode| !s.graph_ref().feed(n, "midi routes").is_empty();

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
impl tutti_midi_types::MidiOut for CountingSink {
    fn queue(&self, events: &[MidiEvent]) -> usize {
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
///
/// Mutations (run), each fails:
/// - `MidiState::midi_out_phase` routing to no output while stamping is
///   off (`self.out.as_mut().filter(|o| o.is_stamping())`) → nothing
///   reaches the sink;
/// - the `MidiOutDropped` report dropped → no event.
#[test]
fn the_hardware_out_drains_with_stamping_off() {
    let mut s = clocked();
    let count = Arc::new(AtomicUsize::new(0));
    s.midi().set_hardware_out(Some(UmpOut::new(
        Box::new(CountingSink(Arc::clone(&count))),
        RATE,
    )));
    let events = s.update();
    assert!(
        count.load(Ordering::SeqCst) > 0,
        "the clock reached the sink, unstamped"
    );
    assert!(!events
        .iter()
        .any(|e| matches!(e, SessionEvent::MidiOutDropped(_))));

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
///
/// Mutations (run), each fails:
/// - the rate limit dropped (`due` always true) → the second update
///   reports again;
/// - `reported_at` never set → the same;
/// - `>=` as `>` on the interval → nothing at exactly one second.
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
///
/// Mutations (run): `Midi::play_clip` inserting its clip with
/// `Graph::insert` → no reclaimer kept, fails; the builder inserting the
/// live input with `Graph::insert` → fails; `MidiClipNode`'s `SessionNode`
/// impl returning `None` from `reclaimer` → fails.
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

/// **The update frees a port set a drain held across a port's removal**:
/// with the OS MIDI layer open, a port removed while the engine's drain
/// borrows the port set retires the set, and the next update's `Reclaim`
/// phase frees it (`HardwareMidiInputs::collect`) with no further change.
///
/// Mutation (run): `Session::reclaim` leaving out the hardware ports
/// (`io.ports().collect()`) → the set is still retired after the update,
/// fails.
#[cfg(feature = "midi-hardware")]
#[test]
fn an_update_frees_a_port_set_a_drain_held_across_a_removal() {
    let mut s = Session::builder()
        .midi_hardware(true)
        .build_offline(SampleRate(RATE), ChannelLayout::STEREO, Samples(1_024))
        .expect("builds");
    let ports = Arc::clone(s.midi_io().expect("the OS MIDI layer").ports());
    let mut handle = ports.create_input_port("Gone");
    let id = handle.port_id();
    assert!(handle.push(MidiEvent::noop(), Instant::now()));
    drop(handle);

    // A block in progress: the drain borrows the set while the port goes.
    tutti_midi_hardware::HardwareMidiDrain::new(Arc::clone(&ports))
        .cycle_start_read_all_inputs(512, |_, _| assert!(ports.remove_input_port(id)));
    assert_eq!(ports.retired_sets(), 1, "the drain held the set");

    s.update();
    assert_eq!(ports.retired_sets(), 0, "the update freed it");
}

/// **The feeds are partial, as audio's declarations are: an edge a host
/// wired on a fed event input stays.** A host wires its own clip into the
/// tap's event input 0 by hand; the session's sequencer then feeds the tap
/// too, and both notes play. A host's later `set_event_sources` on that
/// input sets its own edges beside the sequencer's; the sequencer's clip
/// re-aimed elsewhere leaves the host's edges where they are.
///
/// Mutations (run), each fails:
/// - `Graph::write_feeds` writing the feeds' union over the whole input
///   (the host's edges not kept) → the hand clip is unwired by `play_clip`
///   and its note never plays;
/// - `Graph::set_event_sources` on a fed input not keeping the feeds'
///   sources → the sequencer's clip is unwired by the host's call.
#[test]
fn a_hand_wired_event_edge_survives_the_sequencers_feed() {
    use tutti_midi_runtime::MidiClipNode;

    let sorted = |mut v: Vec<EventSource>| {
        v.sort_by_key(|e| (e.node.0 .0, e.port));
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
