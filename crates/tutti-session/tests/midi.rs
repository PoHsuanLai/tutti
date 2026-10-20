//! MIDI in the session (design 015 §4.3): clips, live inputs, the hardware
//! input's routes and the hardware out, on an offline session the test
//! renders. A [`Tap`] stands in for a synth: it logs every MIDI event its
//! event input receives with its absolute frame.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tutti_core::{
    AudioNode, Beat, BeatDuration, ChannelLayout, InterleavedMut, MotionEvent, SampleRate, Samples,
};
use tutti_graph::{Cx, EventKind, IntoNode, Io, Node, NodeParts, Prepare, Shape, Status, Ump};
use tutti_midi_runtime::{TimedMidiEvent, CHANNELLESS_PORT, MIDI_INPUT_PORTS};
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::{MidiChannel, MidiGroup};
use tutti_session::{
    Error, EventSource, MidiRouteRule, Session, SessionEvent, SessionNode, UmpOut,
};

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

/// **A clip and a live input survive their target's re-bind** (the PR 4
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
/// Mutation (run): `Midi::set_routes` not unwiring a sink no rule names
/// any more → the unrouted tap keeps its ports → fails.
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
/// output it counts them as dropped and says so once per rise.
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
