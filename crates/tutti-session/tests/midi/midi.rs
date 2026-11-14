//! MIDI in the session: clips, live inputs, the hardware
//! input's routes and the hardware out, on an offline session the test
//! renders. A [`Tap`] stands in for a synth: it logs every MIDI event its
//! event input receives with its absolute frame.

use crate::common::{MF, Seen, blocks, note, offline, play, tap};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tutti_graph::UmpAt;
use tutti_session::Stamp;
use tutti_types::MaxBlock;

use std::time::{Duration, Instant};
use tutti_fixtures::midi;
use tutti_graph::NodeKey;
use tutti_graph::Refused;
#[cfg(feature = "midi-hardware")]
use tutti_midi_hardware::{EndpointId, PortDirection};
use tutti_midi_runtime::{CHANNELLESS_PORT, LIVE_INLET_CAPACITY, MIDI_INPUT_PORTS, TimedMidiEvent};
use tutti_types::{Beat, BeatDuration, ChannelLayout, Frame, SampleRate, Samples};
use tutti_ump::Ump;
use tutti_ump::{KeyEvent, MidiChannel, MidiGroup, NoteNumber, NoteVelocity};

use tutti_session::{
    EventInPort, EventOutPort, Feeder, MIDI_OUT_DROP_REPORT, MidiError, MidiEvent, MidiRouteRule,
    NoSuchMidiRoute, PHASES, PhaseCx, RouteDeclarer, Session, SessionEvent, UmpOut,
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

/// Plays the transport from beat 0 and renders `frames` frames in 512-frame
/// blocks.
fn roll(s: &Session, frames: usize) {
    play(s);
    blocks(s, frames.div_ceil(512));
}

fn note_ons(seen: &Seen) -> Vec<(u64, u8)> {
    seen.lock()
        .unwrap()
        .iter()
        .filter_map(|&(f, e)| match e.key_event() {
            Some(KeyEvent::On(key)) => Some((f, key.note().get())),
            _ => None,
        })
        .collect()
}

/// The nodes the last plan sent holds.
fn plan_nodes(s: &Session) -> usize {
    s.graph_ref()
        .testing()
        .sent_plan()
        .expect("a plan")
        .order()
        .len()
}

/// **A clip lands its note on its frame**: a third of a beat in at 120 BPM
/// / 48 kHz is frame 8 000, inside a block.
#[test]
fn a_clip_plays_its_note_on_its_frame() {
    let mut s = offline();
    let (synth, seen) = tap(&mut s);
    s.midi()
        .play_clip(
            synth,
            Beat(0.0),
            note(60, MF, Beat(1.0 / 3.0), BeatDuration(1.0)),
        )
        .expect("the tap takes MIDI");
    let _ = s.update();
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
    s.midi()
        .play_clip(synth, Beat(0.0), note(60, MF, Beat(0.0), BeatDuration(1.0)))
        .unwrap();
    let clip = s.midi().clip(synth).expect("the target's clip");
    let _ = s.update();
    let nodes = plan_nodes(&s);

    s.midi()
        .play_clip(
            synth,
            Beat(0.0),
            note(64, MF, Beat(1.0 / 3.0), BeatDuration(1.0)),
        )
        .unwrap();
    let _ = s.update();
    assert_eq!(s.midi().clip(synth), Some(clip), "the same clip");
    assert_eq!(plan_nodes(&s), nodes, "and no node more");
    roll(&s, 9_000);
    assert_eq!(note_ons(&seen), vec![(8_000, 64)], "playing the new events");

    s.midi().stop_clip(synth).expect("the target has a clip");
    let _ = s.update();
    assert_eq!(s.midi().clip(synth), Some(clip), "the emptied clip stays");
    assert!(s.graph_ref().contains(clip));
}

/// **A clip's events land `at` beats later**: a third of a beat into the
/// clip, started at beat 1, is a third of a beat past beat 1, frame 32 000 at
/// 120 BPM / 48 kHz.
#[test]
fn a_clip_starts_at_the_beat_it_is_given() {
    let mut s = offline();
    let (synth, seen) = tap(&mut s);
    s.midi()
        .play_clip(
            synth,
            Beat(1.0),
            note(60, MF, Beat(1.0 / 3.0), BeatDuration(1.0)),
        )
        .expect("the tap takes MIDI");
    let _ = s.update();
    roll(&s, 33_000);
    assert_eq!(note_ons(&seen), vec![(32_000, 60)]);
}

/// **A start that is not a finite beat is refused, and nothing is made.**
#[test]
fn a_clip_start_that_is_not_finite_is_refused() {
    let mut s = offline();
    let (synth, _) = tap(&mut s);
    let nan = Beat(0.0 / std::hint::black_box(0.0));
    for bad in [nan, Beat(f64::INFINITY)] {
        assert!(matches!(
            s.midi()
                .play_clip(synth, bad, note(60, MF, Beat(0.0), BeatDuration(1.0))),
            Err(MidiError::BeatNotFinite(_))
        ));
    }
    assert_eq!(s.midi().clip(synth), None, "no clip was made");
}

/// **Stopping a clip a target does not have names the target.**
#[test]
fn stopping_a_clip_that_is_not_there_names_the_target() {
    let mut s = offline();
    let (synth, _) = tap(&mut s);
    assert!(matches!(
        s.midi().stop_clip(synth),
        Err(MidiError::NoClip(n)) if n == synth
    ));
}

/// **A file plays as a clip at the beat it is given**: a note on its beat 1,
/// in a file played from beat 1, sounds at beat 2 (frame 48 000), and a format
/// 2 file, which has no one timeline, is refused with that reason.
#[test]
fn a_file_plays_at_a_beat_and_a_format_2_file_is_refused() {
    use tutti_midi_file::{MidiFile, SmfFormat, Tick, Track};
    use tutti_ump::U7;
    let track = Track::new().note(
        Tick(480),
        240,
        MidiChannel::FIRST,
        NoteNumber::of(62),
        U7::of(100),
    );
    let file = MidiFile::smf(SmfFormat::SingleTrack, 480, vec![track.clone()]).unwrap();
    let mut s = offline();
    let (synth, seen) = tap(&mut s);
    s.midi().play_file(synth, Beat(1.0), &file).unwrap();
    let _ = s.update();
    roll(&s, 49_000);
    assert_eq!(note_ons(&seen), vec![(48_000, 62)]);

    let sequential = MidiFile::smf(SmfFormat::Sequential, 480, vec![track]).unwrap();
    assert!(matches!(
        s.midi().play_file(synth, Beat(0.0), &sequential),
        Err(MidiError::File(tutti_midi_file::Error::NoCommonTimeline(_)))
    ));
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
        s.midi().play_clip(dc, Beat(0.0), std::iter::empty::<TimedMidiEvent>()),
        Err(MidiError::NoEventInput(n)) if n == dc.erase()
    ));
    s.remove(dc).unwrap();
    assert!(matches!(
        s.midi().input(dc),
        Err(MidiError::NoEventInput(_))
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
    s.midi()
        .play_clip(
            old,
            Beat(0.0),
            note(60, MF, Beat(1.0 / 3.0), BeatDuration(1.0)),
        )
        .unwrap();
    let clip = s.midi().clip(old).expect("the target's clip");
    let mut live = s.midi().input(old).unwrap();
    let inlet = s.graph_ref().feed(old, Feeder::LiveInput)[0].node();
    let _ = s.update();

    s.remove(old).unwrap();
    let (new, new_seen) = tap(&mut s);
    s.midi().retarget(clip, new).expect("the clip re-aims");
    s.midi()
        .retarget_input(old, new)
        .expect("the queue re-aims");
    let _ = s.update();
    assert_eq!(s.midi().clip(new), Some(clip), "the same clip");
    assert_eq!(
        s.graph_ref().feed(new, Feeder::LiveInput)[0].node(),
        inlet,
        "the same inlet"
    );
    assert!(
        matches!(s.midi().input(new), Err(MidiError::InputTaken(n)) if n == new),
        "its keys were handed out once"
    );
    let mut fed = s.graph_ref().sources(EventInPort::new(new, 0)).unwrap();
    fed.sort_by_key(|e| e.node());
    // The clip feeds the target through its MPE transform.
    let transform = s.graph_ref().feed(new, Feeder::Sequencer)[0];
    let mut want = vec![transform, EventOutPort::new(inlet, 0)];
    want.sort_by_key(|e| e.node());
    assert_eq!(fed, want);

    live.push(
        Stamp::NextBlock,
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

/// **A live input's keys are the inlet's one producer and send in order**:
/// the key helpers build group-1 MIDI 2.0 notes, a frame is held until its
/// block, a full ring says so, and a second `input` for the target is refused
/// rather than a second writer on the ring.
#[test]
fn a_live_inputs_keys_send_in_order_and_are_handed_out_once() {
    let mut s = offline();
    let (target, seen) = tap(&mut s);
    let mut keys = s.midi().input(target).unwrap();
    assert!(
        matches!(s.midi().input(target), Err(MidiError::InputTaken(n)) if n == target),
        "one producer"
    );
    let _ = s.update();

    let on = |keys: &mut tutti_midi_runtime::MidiKeys, at: Stamp, n: u8| {
        keys.note_on(
            at,
            MidiChannel::FIRST,
            NoteNumber::of(n),
            NoteVelocity::new(MF),
        )
    };
    on(&mut keys, Stamp::NextBlock, 60).unwrap();
    on(&mut keys, Stamp::NextBlock, 62).unwrap();
    on(&mut keys, Frame::new(3_000).into(), 64).unwrap();
    roll(&s, 4_000);
    let heard = note_ons(&seen);
    assert_eq!(
        heard.iter().map(|&(_, n)| n).collect::<Vec<_>>(),
        [60, 62, 64]
    );
    assert!(heard[0].0 < 512 && heard[1].0 < 512, "{heard:?}");
    assert_eq!(heard[2].0, 3_000);

    for _ in 0..LIVE_INLET_CAPACITY.slots() {
        keys.note_off(Stamp::NextBlock, MidiChannel::FIRST, NoteNumber::of(60))
            .unwrap();
    }
    assert_eq!(
        keys.note_off(Stamp::NextBlock, MidiChannel::FIRST, NoteNumber::of(60)),
        Err(Refused::Full)
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
                assert_eq!(e.node(), input);
                u16::try_from(e.index()).unwrap()
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

    s.midi().unroute(all).unwrap();
    assert!(
        matches!(
            s.midi().unroute(all),
            Err(MidiError::NoSuchRoute(NoSuchMidiRoute(id))) if id == all
        ),
        "a removed rule is not there"
    );
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
            SampleRate::of(RATE),
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
                assert_eq!(e.node(), input);
                u16::try_from(e.index()).unwrap()
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
        Err(MidiError::TwoRouteDeclarers(RouteDeclarer::Session))
    ));
    assert!(fed(&s, a) && !fed(&s, b), "the refusal changed nothing");

    s.midi().unroute(own).unwrap();
    s.midi().set_routes(input, &host, None, resolve).unwrap();
    assert!(fed(&s, b), "the host's routes apply");
    assert!(matches!(
        s.midi().route(MidiRouteRule::any_channel().to(a)),
        Err(MidiError::TwoRouteDeclarers(RouteDeclarer::Host))
    ));
    assert!(matches!(
        s.midi().set_route_fallback(Some(a)),
        Err(MidiError::TwoRouteDeclarers(RouteDeclarer::Host))
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
    let _ = s.update(); // the builder's nodes, committed
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
        SampleRate::of(RATE),
    )));
    let events = s.update();
    assert!(
        count.load(Ordering::SeqCst) > 0,
        "the clock reached the sink, unstamped"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SessionEvent::Midi(MidiEvent::OutputDropped { .. })))
    );

    let mut s = clocked();
    let events = s.update();
    let dropped = s.midi().out_drops();
    assert!(dropped > 0, "no output: the ticks are dropped, counted");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SessionEvent::Midi(MidiEvent::OutputDropped { total }) if *total == dropped)),
        "{events:?}"
    );
    assert!(
        !s.update()
            .iter()
            .any(|e| matches!(e, SessionEvent::Midi(MidiEvent::OutputDropped { .. }))),
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
                SessionEvent::Midi(MidiEvent::OutputDropped { total }) => Some(*total),
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
/// graph's reclaimers in `tests/wiring/graph.rs`.
#[test]
fn the_sessions_midi_nodes_keep_their_reclaimers() {
    let mut s = offline();
    let (synth, _) = tap(&mut s);
    s.midi()
        .play_clip(synth, Beat(0.0), note(60, MF, Beat(0.0), BeatDuration(1.0)))
        .expect("the tap takes MIDI");
    let clip = s.midi().clip(synth).expect("the target's clip");
    assert!(s.graph_ref().testing().keeps_reclaimer(clip), "the clip's");
    let input = s.midi_nodes().expect("the builder's MIDI nodes").input;
    assert!(
        s.graph_ref().testing().keeps_reclaimer(input),
        "the live input's"
    );
    assert!(
        !s.graph_ref().testing().keeps_reclaimer(synth),
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
            SampleRate::of(RATE),
            ChannelLayout::STEREO,
            MaxBlock::of(Samples(1_024)),
        )
        .expect("builds");
    (s, fake)
}

/// The device events of an update, as `(kind, device name)`.
#[cfg(feature = "midi-hardware")]
fn device_events(events: &[SessionEvent]) -> Vec<(&'static str, String)> {
    events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::Midi(MidiEvent::Added { info }) => Some(("added", info.name.clone())),
            SessionEvent::Midi(MidiEvent::Removed { info }) => Some(("removed", info.name.clone())),
            SessionEvent::Midi(MidiEvent::Changed { info }) => Some(("changed", info.name.clone())),
            SessionEvent::Midi(MidiEvent::InputLost { info }) => {
                Some(("input lost", info.name.clone()))
            }
            SessionEvent::Midi(MidiEvent::OutputLost { info }) => {
                Some(("output lost", info.name.clone()))
            }
            _ => None,
        })
        .collect()
}

/// The session's OS MIDI layer, read.
#[cfg(feature = "midi-hardware")]
fn hw(s: &Session) -> &tutti_midi_hardware::MidiSession {
    s.testing_midi_io().expect("the OS MIDI layer")
}

#[cfg(feature = "midi-hardware")]
fn plug(
    fake: &tutti_midi_hardware::test_support::FakeCounters,
    raw: u64,
    name: &str,
    d: PortDirection,
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

    plug(&fake, 1, "Keys", PortDirection::Input);
    assert_eq!(device_events(&s.update()), [("added", "Keys".to_string())]);
    assert!(device_events(&s.update()).is_empty(), "reported once");

    fake.unplug(EndpointId::from_raw(1));
    assert_eq!(
        device_events(&s.update()),
        [("removed", "Keys".to_string())],
        "unheld, so nothing is retired"
    );
}

/// **A held input whose device vanishes is retired**: the update
/// reports the removal and then the lost input, the connection is closed and
/// its port is gone.
#[cfg(feature = "midi-hardware")]
#[test]
fn a_held_input_that_vanishes_is_retired_and_reported() {
    let (mut s, fake) = hot_plug_session();
    plug(&fake, 1, "Keys", PortDirection::Input);
    let _ = s.update();
    s.midi().connect_input("keys").expect("connects");
    assert_eq!(hw(&s).connected_input_names(), ["Keys"]);
    assert_eq!(hw(&s).ports().list_input_ports().len(), 1);

    fake.unplug(EndpointId::from_raw(1));
    assert_eq!(
        device_events(&s.update()),
        [
            ("removed", "Keys".to_string()),
            ("input lost", "Keys".to_string())
        ]
    );
    assert!(
        !hw(&s).is_any_input_connected(),
        "no longer listed as connected"
    );
    assert!(hw(&s).connected_input_names().is_empty());
    assert!(
        hw(&s).ports().list_input_ports().is_empty(),
        "the port is gone"
    );
}

/// A session over a fake OS with the device `Keys` (id 1) and `Pads` (id 2)
/// present, and a stand-in synth that every channel of the hardware input
/// routes to.
#[cfg(feature = "midi-hardware")]
fn keyboards(
    devices: std::num::NonZeroUsize,
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
            SampleRate::of(RATE),
            ChannelLayout::STEREO,
            MaxBlock::of(Samples(1_024)),
        )
        .expect("builds");
    let (synth, seen) = tap(&mut s);
    s.midi()
        .route(MidiRouteRule::any_channel().to(synth))
        .expect("routes");
    let _ = s.update();
    (s, fake, seen)
}

/// Connects `name`, and updates: the `Poll` phase wires the new inlet.
#[cfg(feature = "midi-hardware")]
fn connect(s: &mut Session, name: &str) {
    s.midi().connect_input(name).expect("connects");
    let _ = s.update();
}

/// A note-on on channel `channel`.
#[cfg(feature = "midi-hardware")]
fn key(channel: u8, n: u8) -> Ump {
    midi::note_on(MidiGroup::FIRST, MidiChannel::of(channel), n, MF)
}

/// **A connected device's note reaches the synth its channel routes to**
/// through the device's own event inlet, once: the update after the connection
/// wires the inlet to the ingest node, the device's thread
/// pushes, and the synth hears it.
#[cfg(feature = "midi-hardware")]
#[test]
fn a_connected_devices_note_reaches_the_synth_through_its_inlet() {
    let (mut s, fake, seen) = keyboards(std::num::NonZeroUsize::new(4).expect("four"));
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
    let (mut s, fake, seen) = keyboards(std::num::NonZeroUsize::new(4).expect("four"));
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

    let _ = s.midi().disconnect_input("keys");
    let _ = s.update();
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

/// **A device that is disconnected with a key held ends the key downstream,
/// once** (MIDI 1.0 Detailed Specification p. 24: individual Note Offs): the
/// device's input dropping gives its slot back, the ingest node releases what
/// the device held at its next block, and the executor's close of the inlet's
/// edge, which would send the same Note Off at the next update, arrives at a
/// slot nobody holds and is not heard.
#[cfg(feature = "midi-hardware")]
#[test]
fn a_disconnected_device_ends_its_held_key_once() {
    let (mut s, fake, seen) = keyboards(std::num::NonZeroUsize::new(4).expect("four"));
    connect(&mut s, "keys");
    roll(&s, 1_024);
    assert!(fake.send_input(EndpointId::from_raw(1), key(0, 60), Instant::now()));
    roll(&s, 8_192);
    assert_eq!(note_ons(&seen).len(), 1);
    let _ = s.midi().disconnect_input("keys");
    let _ = s.update();
    roll(&s, 8_192);
    let offs: Vec<u8> = seen
        .lock()
        .unwrap()
        .iter()
        .filter_map(|&(_, e)| match e.key_event() {
            Some(KeyEvent::Off(key)) => Some(key.note().get()),
            _ => None,
        })
        .collect();
    assert_eq!(offs, vec![60]);
}

/// **An arrival is placed on the engine's clock**: a note the OS stamps one
/// second ahead of now sounds a second of frames on, not at the next block.
/// An event pushed for the next block (the path of a session with no clock)
/// sounds within one.
#[cfg(feature = "midi-hardware")]
#[test]
fn a_devices_arrival_is_placed_on_the_engines_clock() {
    let (mut s, fake, seen) = keyboards(std::num::NonZeroUsize::new(4).expect("four"));
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
/// reports `InputsDropped` with that total, once.
#[cfg(feature = "midi-hardware")]
#[test]
fn a_device_whose_inlet_overflows_is_reported() {
    let (mut s, fake, _seen) = keyboards(std::num::NonZeroUsize::new(4).expect("four"));
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
                SessionEvent::Midi(MidiEvent::InputsDropped { total }) => Some(total),
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
    let (mut s, fake, seen) = keyboards(std::num::NonZeroUsize::MIN);
    connect(&mut s, "keys");
    let refused = s.midi().connect_input("pads").unwrap_err();
    assert!(
        matches!(
            refused,
            tutti_session::MidiError::Hardware(tutti_midi_hardware::Error::NoFreeSlot(_))
        ),
        "{refused:?}"
    );
    assert_eq!(hw(&s).ports().list_input_ports().len(), 1);
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
    plug(&fake, 10, "Synth", PortDirection::Output);
    let _ = s.update();
    s.midi().connect_output("synth").expect("connects");
    assert!(s.midi().output().is_some());

    fake.unplug(EndpointId::from_raw(10));
    assert_eq!(
        device_events(&s.update()),
        [
            ("removed", "Synth".to_string()),
            ("output lost", "Synth".to_string())
        ]
    );
    assert!(s.midi().output().is_none());
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
                SessionEvent::Midi(MidiEvent::WatchFailed(why)) => Some(why.to_string()),
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
    plug(&fake, 1, "Keys", PortDirection::Input);
    let _ = s.update();
    let saved = hw(&s).devices().list().remove(0);
    assert_eq!(
        hw(&s).devices().resolve(&saved),
        Resolved::SameName(saved.clone())
    );
    fake.unplug(EndpointId::from_raw(1));
    let _ = s.update();
    assert_eq!(hw(&s).devices().resolve(&saved), Resolved::Missing);
}

/// **Another device's removal leaves a held input and output alone.**
#[cfg(feature = "midi-hardware")]
#[test]
fn another_devices_removal_leaves_a_held_input_open() {
    let (mut s, fake) = hot_plug_session();
    plug(&fake, 1, "Keys", PortDirection::Input);
    plug(&fake, 2, "Pads", PortDirection::Input);
    plug(&fake, 10, "Synth", PortDirection::Output);
    let _ = s.update();
    s.midi().connect_input("keys").expect("connects");
    s.midi().connect_output("synth").expect("connects");

    fake.unplug(EndpointId::from_raw(2));
    assert_eq!(
        device_events(&s.update()),
        [("removed", "Pads".to_string())]
    );
    assert_eq!(hw(&s).connected_input_names(), ["Keys"]);
    assert_eq!(hw(&s).ports().list_input_ports().len(), 1);
    assert!(s.midi().output().is_some());
}

/// **The feeds are partial, as audio's declarations are: an edge a host
/// wired on a fed event input stays.** A host wires its own clip into the
/// tap's event input 0 by hand; the session's sequencer then feeds the tap
/// too, and both notes play. A host's later `set_sources` on that
/// input sets its own edges beside the sequencer's; the sequencer's clip
/// re-aimed elsewhere leaves the host's edges where they are.
#[test]
fn a_hand_wired_event_edge_survives_the_sequencers_feed() {
    use tutti_midi_runtime::MidiClipNode;

    let sorted = |mut v: Vec<EventOutPort>| {
        v.sort();
        v
    };
    let input = |synth| EventInPort::new(synth, 0);
    let mut s = offline();
    let (synth, seen) = tap(&mut s);
    let (other, _) = tap(&mut s);
    let (hand, _) = s.graph().add(MidiClipNode::new(note(
        62,
        MF,
        Beat(1.0 / 3.0),
        BeatDuration(1.0),
    )));
    s.graph()
        .set_sources(input(synth), [hand.events_out()])
        .unwrap();
    s.midi()
        .play_clip(
            synth,
            Beat(0.0),
            note(60, MF, Beat(1.0 / 3.0), BeatDuration(1.0)),
        )
        .unwrap();
    let clip = s.midi().clip(synth).expect("the target's clip");
    let _ = s.update();
    // The clip feeds the target through its MPE transform.
    let transform = s.graph_ref().feed(synth, Feeder::Sequencer)[0];
    assert_eq!(
        sorted(s.graph_ref().sources(input(synth)).unwrap()),
        sorted(vec![hand.events_out(), transform]),
        "the sequencer's clip joins the hand-wired one"
    );
    roll(&s, 9_000);
    let mut ons = note_ons(&seen);
    ons.sort_unstable();
    assert_eq!(ons, vec![(8_000, 60), (8_000, 62)], "both play");

    let (hand2, _) = s.graph().add(MidiClipNode::new([]));
    s.graph()
        .set_sources(input(synth), [hand2.events_out()])
        .unwrap();
    assert_eq!(
        sorted(s.graph_ref().sources(input(synth)).unwrap()),
        sorted(vec![hand2.events_out(), transform]),
        "the host's call sets its own edges, beside the feed's"
    );

    s.midi().retarget(clip, other).unwrap();
    assert_eq!(
        s.graph_ref().sources(input(synth)).unwrap(),
        vec![hand2.events_out()],
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
        .unwrap()
    });
    s.midi()
        .play_clip(synth, Beat(0.0), chord)
        .expect("takes MIDI");
    let _ = s.update();
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

/// **A UMP Endpoint the session holds open is negotiated from `Phase::Poll`,
/// and the outcome is a session event** (M2-104 §7.1.6.1): the
/// update that follows the connect sends the Endpoint Discovery on the
/// device's output, the device (here the fake's, answering as an Endpoint
/// does) replies into the input, and a later update reports
/// `MidiEvent::StreamSettled` once, with the Protocol in use.
#[cfg(feature = "midi-hardware")]
#[test]
fn a_connected_ump_endpoint_is_negotiated_by_the_session_tick() {
    use tutti_midi_hardware::{Basis, Capabilities, Endpoint};
    use tutti_ump::Protocol;

    let (mut s, fake) = hot_plug_session();
    plug(&fake, 7, "Wire", PortDirection::Duplex);
    let _ = s.update();
    s.midi().connect_input("wire").expect("connects");

    let mut device = Endpoint::new(Capabilities {
        midi1: true,
        midi2: true,
        ..Capabilities::default()
    });
    let raw = EndpointId::from_raw(7);
    let mut settled = Vec::new();
    for _ in 0..20 {
        settled.extend(s.update().into_iter().filter_map(|e| match e {
            SessionEvent::Midi(MidiEvent::StreamSettled { device, stream }) => {
                Some((device, *stream))
            }
            _ => None,
        }));
        let mut answers: Vec<Ump> = Vec::new();
        for ump in fake.take_sent(raw) {
            if let Ok(message) = ump.decode() {
                device.reply(message, &mut answers);
            }
        }
        for ump in answers {
            assert!(fake.send_input(raw, ump, Instant::now()));
        }
    }
    let [(id, stream)] = settled.as_slice() else {
        panic!("one StreamSettled event expected, got {settled:?}");
    };
    assert_eq!(
        hw(&s)
            .inputs()
            .iter()
            .find(|d| d.id == *id)
            .map(|d| d.name.as_str()),
        Some("Wire")
    );
    assert_eq!(stream.basis(), Basis::Negotiated);
    assert_eq!(stream.config().protocol, Protocol::Midi2);
    assert_eq!(hw(&s).stream(id), Some(*stream));
}

/// **A MIDI-CI peer of an input the session holds open is a session event**
/// (M2-101 §5.5 and §5.6): once the UMP Endpoint settles, a
/// Function Block that declares MIDI-CI is sent a Discovery by the session's
/// tick; the device's Reply to Discovery is reported as one
/// `MidiEvent::CiPeer` (`CiChange::Found`) naming the device, and the peer is
/// then in `Midi::ci_peers`.
#[cfg(feature = "midi-hardware")]
#[test]
fn a_midi_ci_peer_that_answers_discovery_is_a_session_event() {
    use tutti_midi_device::{
        BlockName, CiCategories, CiHeader, CiMessage, Decoded, DiscoveryData, MaxSysexSize, Muid,
    };
    use tutti_midi_hardware::{Capabilities, CiChange, Endpoint, FunctionBlock};
    use tutti_ump::{Direction, FirstGroup, FunctionBlockId, GroupSpan, SysexAssembler, SysexStep};

    let (mut s, fake) = hot_plug_session();
    plug(&fake, 7, "Wire", PortDirection::Duplex);
    let _ = s.update();
    s.midi().connect_input("wire").expect("connects");
    // Off by default: the app says who it is, with an id it registered.
    s.midi()
        .enable_ci(tutti_ump::DeviceIdentity {
            manufacturer: tutti_ump::ManufacturerId::NON_COMMERCIAL,
            family: [tutti_ump::U7::of(1); 2],
            model: [tutti_ump::U7::of(2); 2],
            revision: [tutti_ump::U7::of(3); 4],
        })
        .expect("hardware");

    let block = FunctionBlock::builder(
        FunctionBlockId::new(0).unwrap(),
        Direction::Bidirectional,
        FirstGroup::new(0).unwrap(),
        GroupSpan::new(1).unwrap(),
    )
    .midi_ci(2, 0)
    .name(BlockName::new("Main").unwrap())
    .build()
    .unwrap();
    let mut device = Endpoint::new(Capabilities {
        midi1: true,
        midi2: true,
        ..Capabilities::default()
    })
    .with_function_block(block);
    let raw = EndpointId::from_raw(7);
    let peer = Muid::of(0x0012_3456);
    let data = DiscoveryData::new(
        tutti_ump::DeviceIdentity {
            manufacturer: tutti_ump::ManufacturerId::of(0x41),
            family: [tutti_ump::U7::of(1); 2],
            model: [tutti_ump::U7::of(2); 2],
            revision: [tutti_ump::U7::of(3); 4],
        },
        CiCategories::empty(),
        MaxSysexSize::of(256),
    );
    let room = SysexAssembler::room_for(256, 1, 16);
    let mut assembler = SysexAssembler::new(vec![0; room].into_boxed_slice(), 256, 16);
    let start = Instant::now();
    let mut found = Vec::new();
    let mut replied = false;
    for ms in 0..40 {
        let now = start + Duration::from_millis(ms);
        for event in update_at(&mut s, now) {
            if let SessionEvent::Midi(MidiEvent::CiPeer { device, change }) = event {
                found.push((device, change));
            }
        }
        let mut answers: Vec<Ump> = Vec::new();
        for ump in fake.take_sent(raw) {
            if let Ok(message @ tutti_ump::Message::Stream(_)) = ump.decode() {
                device.reply(message, &mut answers);
                continue;
            }
            assembler.begin_block();
            let SysexStep::Complete(done) = assembler.push(ump) else {
                continue;
            };
            if let Decoded::Message(CiMessage::Discovery { header, .. }) =
                CiMessage::decode(assembler.message(done).payload())
                && !replied
            {
                replied = true;
                let reply = CiMessage::DiscoveryReply {
                    header: CiHeader::new(peer),
                    to: header.source,
                    data,
                };
                answers.extend(reply.packets(MidiGroup::FIRST));
            }
        }
        for ump in answers {
            assert!(fake.send_input(raw, ump, now));
        }
    }
    assert!(replied, "the session sent a Discovery");
    let [(device_id, CiChange::Found(info))] = found.as_slice() else {
        panic!("one peer found, got {found:?}");
    };
    assert_eq!(
        hw(&s)
            .inputs()
            .iter()
            .find(|d| d.id == *device_id)
            .map(|d| d.name.as_str()),
        Some("Wire")
    );
    assert_eq!(info.muid(), peer);
    assert_eq!(info.peer().discovery(), &data);
    assert_eq!(
        &s.midi().ci_peers(device_id)[..],
        std::slice::from_ref(info)
    );
}

/// **A MIDI-CI peer's Profiles are the session's, and it switches them as the
/// Initiator** (M2-101 section 7): a peer whose Discovery Reply declares Profile
/// Configuration is sent a Profile Inquiry by the session's tick; its Replies
/// make the Profiles `Midi::profiles` lists (`MidiEvent::CiProfiles`, `Known`
/// at the final Reply on the Function Block); a request made from a listed
/// handle with `Midi::set_profile` is sent as a Set Profile On (Sub-ID#2
/// `0x22`) with the Channel count asked, and the peer's Profile Enabled Report
/// resolves it and updates the list.
#[cfg(feature = "midi-hardware")]
#[test]
fn a_midi_ci_peers_profiles_are_listed_and_switched_through_the_session() {
    use tutti_midi_device::{
        BlockName, CI_VERSION, CiCategories, CiDeviceId, CiHeader, CiMessage, Decoded,
        DiscoveryData, MaxSysexSize, Muid, Outcome, ProfileChannels, ProfileId, ProfileLists,
        ProfileState, StandardProfile,
    };
    use tutti_midi_hardware::{
        Capabilities, ChannelCount, Endpoint, FunctionBlock, ListedProfile, ProfilesChange,
    };
    use tutti_ump::{
        DeviceIdentity, Direction, FirstGroup, FunctionBlockId, GroupSpan, ManufacturerId,
        SysexAssembler, SysexStep, U7,
    };

    let (mut s, fake) = hot_plug_session();
    plug(&fake, 7, "Wire", PortDirection::Duplex);
    let _ = s.update();
    s.midi().connect_input("wire").expect("connects");
    let identity = |manufacturer| DeviceIdentity {
        manufacturer,
        family: [U7::of(1); 2],
        model: [U7::of(2); 2],
        revision: [U7::of(3); 4],
    };
    s.midi()
        .enable_ci(identity(ManufacturerId::NON_COMMERCIAL))
        .expect("hardware");
    let block = FunctionBlock::builder(
        FunctionBlockId::new(0).unwrap(),
        Direction::Bidirectional,
        FirstGroup::new(0).unwrap(),
        GroupSpan::new(1).unwrap(),
    )
    .midi_ci(2, 0)
    .name(BlockName::new("Main").unwrap())
    .build()
    .unwrap();
    let mut device = Endpoint::new(Capabilities {
        midi1: true,
        midi2: true,
        ..Capabilities::default()
    })
    .with_function_block(block);
    let raw = EndpointId::from_raw(7);
    let peer = Muid::of(0x0012_3456);
    let profile = ProfileId::Standard(StandardProfile::new(
        U7::of(0x21),
        U7::of(2),
        U7::of(1),
        U7::of(1),
    ));
    let at = |device_id| CiHeader {
        device_id,
        ci_version: CI_VERSION,
        source: peer,
    };
    let room = SysexAssembler::room_for(256, 1, 16);
    let mut assembler = SysexAssembler::new(vec![0; room].into_boxed_slice(), 256, 16);
    let start = Instant::now();
    let mut seen = Vec::new();
    let mut set_on = Vec::new();
    let mut device_id = None;
    let mut switched = false;
    for ms in 0..80 {
        let now = start + Duration::from_millis(ms);
        for event in update_at(&mut s, now) {
            match event {
                SessionEvent::Midi(MidiEvent::CiPeer { device, .. }) => device_id = Some(device),
                SessionEvent::Midi(MidiEvent::CiProfiles { change, .. }) => seen.push(change),
                _ => {}
            }
        }
        if !switched
            && seen
                .iter()
                .any(|c| matches!(c, ProfilesChange::Known { .. }))
            && let Some(id) = &device_id
        {
            let peers = s.midi().profiles(id);
            let listed: Vec<_> = peers[0].listed().collect();
            let [ListedProfile::Channel(c)] = listed.as_slice() else {
                panic!("one Profile on a Channel: {listed:?}");
            };
            assert_eq!(listed[0].state(), ProfileState::Disabled);
            s.midi()
                .set_profile(c.enable(ChannelCount::new(2).unwrap()))
                .expect("the peer lists it");
            switched = true;
        }
        let mut answers: Vec<Ump> = Vec::new();
        for ump in fake.take_sent(raw) {
            if let Ok(message @ tutti_ump::Message::Stream(_)) = ump.decode() {
                device.reply(message, &mut answers);
                continue;
            }
            assembler.begin_block();
            let SysexStep::Complete(done) = assembler.push(ump) else {
                continue;
            };
            match CiMessage::decode(assembler.message(done).payload()) {
                Decoded::Message(CiMessage::Discovery { header, .. }) => {
                    let data = DiscoveryData::new(
                        identity(ManufacturerId::of(0x41)),
                        CiCategories::PROFILE_CONFIGURATION,
                        MaxSysexSize::of(256),
                    );
                    answers.extend(
                        CiMessage::DiscoveryReply {
                            header: CiHeader::new(peer),
                            to: header.source,
                            data,
                        }
                        .packets(MidiGroup::FIRST),
                    );
                }
                Decoded::Message(CiMessage::ProfileInquiry { header, .. }) => {
                    for (address, lists) in [
                        (
                            CiDeviceId::Channel(MidiChannel::of(0)),
                            ProfileLists::new(vec![], vec![profile]).unwrap(),
                        ),
                        (CiDeviceId::FunctionBlock, ProfileLists::none()),
                    ] {
                        answers.extend(
                            CiMessage::ProfileReply {
                                header: at(address),
                                to: header.source,
                                profiles: lists,
                            }
                            .packets(MidiGroup::FIRST),
                        );
                    }
                }
                Decoded::Message(CiMessage::SetProfileOn {
                    header, channels, ..
                }) => {
                    set_on.push((header.device_id, channels));
                    answers.extend(
                        CiMessage::ProfileEnabled {
                            header: at(header.device_id),
                            profile,
                            channels,
                        }
                        .packets(MidiGroup::FIRST),
                    );
                }
                _ => {}
            }
        }
        for ump in answers {
            assert!(fake.send_input(raw, ump, now));
        }
    }
    assert_eq!(
        set_on,
        [(
            CiDeviceId::Channel(MidiChannel::of(0)),
            ProfileChannels::of(2)
        )],
        "one Set Profile On, for Channel 1 and two Channels"
    );
    assert!(
        seen.iter().any(|c| matches!(
            c,
            ProfilesChange::Resolved {
                outcome: Outcome::Reported { state: ProfileState::Enabled, channels },
                ..
            } if *channels == ProfileChannels::of(2)
        )),
        "{seen:?}"
    );
    let id = device_id.expect("the peer was found");
    let peers = s.midi().profiles(&id);
    let listed: Vec<_> = peers[0].listed().collect();
    assert_eq!(listed[0].state(), ProfileState::Enabled);
    assert_eq!(listed[0].channels(), Some(ProfileChannels::of(2)));
}

/// A MIDI-CI peer that lists the MPE Profile (M2-120 section 3.2) disabled on
/// Channel 1, enables it when asked with the Number of Channels (appendix A.3
/// and A.4) and disables it when asked, over the fake OS.
#[cfg(feature = "midi-hardware")]
struct MpePeer {
    device: tutti_midi_hardware::Endpoint,
    assembler: tutti_ump::SysexAssembler<Box<[u8]>>,
    muid: tutti_midi_device::Muid,
}

#[cfg(feature = "midi-hardware")]
impl MpePeer {
    const RAW: u64 = 7;

    fn new() -> Self {
        use tutti_midi_hardware::{Capabilities, Endpoint, FunctionBlock};
        use tutti_ump::{Direction, FirstGroup, FunctionBlockId, GroupSpan, SysexAssembler};
        let block = FunctionBlock::builder(
            FunctionBlockId::new(0).unwrap(),
            Direction::Bidirectional,
            FirstGroup::new(0).unwrap(),
            GroupSpan::new(1).unwrap(),
        )
        .midi_ci(2, 0)
        .name(tutti_midi_device::BlockName::new("Main").unwrap())
        .build()
        .unwrap();
        let room = SysexAssembler::room_for(256, 1, 16);
        Self {
            device: Endpoint::new(Capabilities {
                midi1: true,
                midi2: true,
                ..Capabilities::default()
            })
            .with_function_block(block),
            assembler: SysexAssembler::new(vec![0; room].into_boxed_slice(), 256, 16),
            muid: tutti_midi_device::Muid::of(0x0012_3456),
        }
    }

    /// What the session sent the peer, answered as the peer would.
    fn answer(&mut self, fake: &tutti_midi_hardware::test_support::FakeCounters, now: Instant) {
        use tutti_midi_device::{
            CI_VERSION, CiCategories, CiDeviceId, CiHeader, CiMessage, Decoded, DiscoveryData,
            MaxSysexSize, ProfileId, ProfileLists,
        };
        use tutti_ump::{DeviceIdentity, ManufacturerId, SysexStep, U7};
        let at = |device_id| CiHeader {
            device_id,
            ci_version: CI_VERSION,
            source: self.muid,
        };
        let mut answers: Vec<Ump> = Vec::new();
        for ump in fake.take_sent(EndpointId::from_raw(Self::RAW)) {
            if let Ok(message @ tutti_ump::Message::Stream(_)) = ump.decode() {
                self.device.reply(message, &mut answers);
                continue;
            }
            self.assembler.begin_block();
            let SysexStep::Complete(done) = self.assembler.push(ump) else {
                continue;
            };
            match CiMessage::decode(self.assembler.message(done).payload()) {
                Decoded::Message(CiMessage::Discovery { header, .. }) => {
                    let data = DiscoveryData::new(
                        DeviceIdentity {
                            manufacturer: ManufacturerId::of(0x41),
                            family: [U7::of(1); 2],
                            model: [U7::of(2); 2],
                            revision: [U7::of(3); 4],
                        },
                        CiCategories::PROFILE_CONFIGURATION,
                        MaxSysexSize::of(256),
                    );
                    answers.extend(
                        CiMessage::DiscoveryReply {
                            header: CiHeader::new(self.muid),
                            to: header.source,
                            data,
                        }
                        .packets(MidiGroup::FIRST),
                    );
                }
                Decoded::Message(CiMessage::ProfileInquiry { header, .. }) => {
                    for (address, lists) in [
                        (
                            CiDeviceId::Channel(MidiChannel::of(0)),
                            ProfileLists::new(vec![], vec![ProfileId::MPE]).unwrap(),
                        ),
                        (CiDeviceId::FunctionBlock, ProfileLists::none()),
                    ] {
                        answers.extend(
                            CiMessage::ProfileReply {
                                header: at(address),
                                to: header.source,
                                profiles: lists,
                            }
                            .packets(MidiGroup::FIRST),
                        );
                    }
                }
                Decoded::Message(CiMessage::SetProfileOn {
                    header, channels, ..
                }) => answers.extend(
                    CiMessage::ProfileEnabled {
                        header: at(header.device_id),
                        profile: ProfileId::MPE,
                        channels,
                    }
                    .packets(MidiGroup::FIRST),
                ),
                Decoded::Message(CiMessage::SetProfileOff { header, .. }) => answers.extend(
                    CiMessage::ProfileDisabled {
                        header: at(header.device_id),
                        profile: ProfileId::MPE,
                        channels: tutti_midi_device::ProfileChannels::of(4),
                    }
                    .packets(MidiGroup::FIRST),
                ),
                _ => {}
            }
        }
        for ump in answers {
            assert!(fake.send_input(EndpointId::from_raw(Self::RAW), ump, now));
        }
    }
}

/// **A device that enables the MPE Profile switches its input's ingest to the
/// Profile's rules, and disabling it switches back** (M2-120 sections 3.3,
/// 3.5 and Table 15). The session asks a peer to enable the Profile on
/// Channel 1 with four Channels; the Enabled Report is the zone, so the zone
/// channels' Reset All Controllers and Pitch Bend Sensitivities of 48 reach
/// the synth, and Omni Off on a Member Channel, which the Profile prohibits
/// and which passes with no zone, is dropped. Disabling is the same stop,
/// and Omni Off passes again. Closing the input clears the zones with its slot.
#[cfg(feature = "midi-hardware")]
#[test]
fn a_devices_enabling_the_mpe_profile_switches_its_inputs_rules() {
    use tutti_fixtures::midi::BendRanges;
    use tutti_midi_device::{ProfileChange, ProfileState};
    use tutti_midi_hardware::{ChannelCount, ListedProfile, ProfilesChange};
    use tutti_midi_protocol::{MpeZoneConfig, MpeZones, PitchBendSensitivity};
    use tutti_ump::{CCNumber, Message, Voice2};

    let (backend, fake) =
        tutti_midi_hardware::test_support::FakeBackend::build_with(vec![], vec![], usize::MAX);
    let mut s = Session::builder()
        .midi_hardware_over(backend)
        .build_offline(
            SampleRate::of(RATE),
            ChannelLayout::STEREO,
            MaxBlock::of(Samples(1_024)),
        )
        .expect("builds");
    let (synth, seen) = tap(&mut s);
    s.midi()
        .route(MidiRouteRule::any_channel().to(synth))
        .expect("routes");
    plug(&fake, MpePeer::RAW, "Wire", PortDirection::Duplex);
    let _ = s.update();
    s.midi().connect_input("wire").expect("connects");
    s.midi()
        .enable_ci(tutti_ump::DeviceIdentity {
            manufacturer: tutti_ump::ManufacturerId::NON_COMMERCIAL,
            family: [tutti_ump::U7::of(1); 2],
            model: [tutti_ump::U7::of(2); 2],
            revision: [tutti_ump::U7::of(3); 4],
        })
        .expect("hardware");
    let mut peer = MpePeer::new();
    play(&s);
    let start = Instant::now();
    let clock = std::cell::Cell::new(0u64);
    // Runs the session and the peer for `ms` milliseconds; the profile
    // changes the session reported.
    let mut run = |s: &mut Session, ms: u64| {
        let mut changes = Vec::new();
        for _ in 0..ms {
            clock.set(clock.get() + 1);
            let now = start + Duration::from_millis(clock.get());
            for event in update_at(s, now) {
                if let SessionEvent::Midi(MidiEvent::CiProfiles { device, change }) = event {
                    changes.push((device, change));
                }
            }
            peer.answer(&fake, now);
            blocks(s, 1);
        }
        changes
    };
    let changes = run(&mut s, 60);
    let (device, _) = changes
        .iter()
        .find(|(_, c)| matches!(c, ProfilesChange::Known { .. }))
        .expect("the peer's Profiles are known");
    let device = device.clone();
    let all: Vec<_> = s.midi().profiles(&device)[0].listed().collect();
    let [ListedProfile::Channel(listed)] = all.as_slice() else {
        panic!("one Profile on a Channel: {all:?}");
    };
    assert_eq!(all[0].state(), ProfileState::Disabled);

    // The device speaks MIDI 1.0, which the ingest widens.
    let omni_off = |channel: u8| {
        midi::m1_cc(
            MidiChannel::of(channel),
            CCNumber::of(124),
            tutti_ump::U7::of(0),
        )
    };
    let omni_offs = |seen: &Seen| {
        seen.lock()
            .unwrap()
            .iter()
            .filter(|(_, e)| {
                matches!(
                    e.decode(),
                    Ok(Message::Midi2(a, Voice2::ControlChange { controller, .. }))
                        if controller == CCNumber::of(124) && a.channel.get() == 2
                )
            })
            .count()
    };
    let raw = EndpointId::from_raw(MpePeer::RAW);

    // Not enabled: Omni Off on channel 3 passes.
    assert!(fake.send_input(raw, omni_off(2), Instant::now()));
    let _ = run(&mut s, 5);
    assert_eq!(omni_offs(&seen), 1, "no zone, no rule");

    // Enabled with four Channels from Channel 1.
    s.midi()
        .set_profile(listed.enable(ChannelCount::new(4).unwrap()))
        .expect("the peer lists it");
    let changes = run(&mut s, 60);
    assert!(
        changes.iter().any(|(_, c)| matches!(
            c,
            ProfilesChange::Changed {
                change: ProfileChange::Enabled,
                ..
            }
        )),
        "{changes:?}"
    );
    let peers = s.midi().profiles(&device);
    assert_eq!(
        peers[0].mpe_zones(),
        Some(MpeZones::single(
            MpeZoneConfig::profile(
                MidiChannel::of(0),
                4,
                PitchBendSensitivity::MPE_PROFILE_DEFAULT
            )
            .unwrap()
        ))
    );
    let mut ranges = BendRanges::default();
    for (_, ump) in seen.lock().unwrap().iter() {
        ranges.observe(ump);
    }
    for channel in 1..=3 {
        assert_eq!(
            ranges.semitones(MidiChannel::of(channel)),
            48.0,
            "member {channel}"
        );
    }
    assert!(fake.send_input(raw, omni_off(2), Instant::now()));
    let _ = run(&mut s, 5);
    assert_eq!(omni_offs(&seen), 1, "the Profile prohibits it on a Member");

    // Disabled: the rules are MPE-less again.
    let listed: Vec<_> = s.midi().profiles(&device)[0].listed().collect();
    let [ListedProfile::Channel(listed)] = listed.as_slice() else {
        panic!("one Profile on a Channel: {listed:?}");
    };
    s.midi().set_profile(listed.disable()).expect("lists it");
    let _ = run(&mut s, 60);
    assert_eq!(s.midi().profiles(&device)[0].mpe_zones(), None);
    assert!(fake.send_input(raw, omni_off(2), Instant::now()));
    let _ = run(&mut s, 5);
    assert_eq!(omni_offs(&seen), 2, "no zone, no rule");
}

/// **A MIDI-CI peer's MIDI Message Report is asked for through the session**
/// (M2-101 section 9): a peer whose Discovery Reply declares Process Inquiry is
/// sent an Inquiry: Process Inquiry Capabilities by the session's tick; its reply
/// with MIDI Message Report makes it a `Midi::report_peers` handle
/// (`MidiEvent::CiProcess`); `Midi::report` sends an Inquiry: MIDI Message Report
/// (Sub-ID#2 `0x42`) with the bitmaps asked, and the peer's Reply and End are
/// reported as `ReportBegan` and `ReportEnded`.
#[cfg(feature = "midi-hardware")]
#[test]
fn a_midi_ci_peers_message_report_is_asked_for_through_the_session() {
    use tutti_midi_device::{
        BlockName, CI_VERSION, ChannelControllers, CiCategories, CiDeviceId, CiHeader, CiMessage,
        DataControl, Decoded, DiscoveryData, MaxSysexSize, Muid, NoteData, ProcessFeatures,
        ReportRequest, ReportScope, SystemMessages,
    };
    use tutti_midi_hardware::{Capabilities, Endpoint, FunctionBlock, ProcessChange};
    use tutti_ump::{
        DeviceIdentity, Direction, FirstGroup, FunctionBlockId, GroupSpan, ManufacturerId,
        SysexAssembler, SysexStep, U7,
    };

    let (mut s, fake) = hot_plug_session();
    plug(&fake, 7, "Wire", PortDirection::Duplex);
    let _ = s.update();
    s.midi().connect_input("wire").expect("connects");
    let identity = |manufacturer| DeviceIdentity {
        manufacturer,
        family: [U7::of(1); 2],
        model: [U7::of(2); 2],
        revision: [U7::of(3); 4],
    };
    s.midi()
        .enable_ci(identity(ManufacturerId::NON_COMMERCIAL))
        .expect("hardware");
    let block = FunctionBlock::builder(
        FunctionBlockId::new(0).unwrap(),
        Direction::Bidirectional,
        FirstGroup::new(0).unwrap(),
        GroupSpan::new(1).unwrap(),
    )
    .midi_ci(2, 0)
    .name(BlockName::new("Main").unwrap())
    .build()
    .unwrap();
    let mut device = Endpoint::new(Capabilities {
        midi1: true,
        midi2: true,
        ..Capabilities::default()
    })
    .with_function_block(block);
    let raw = EndpointId::from_raw(7);
    let peer = Muid::of(0x0012_3456);
    let at = |device_id| CiHeader {
        device_id,
        ci_version: CI_VERSION,
        source: peer,
    };
    let asked = ReportScope::FunctionBlock {
        system: SystemMessages::SONG_POSITION,
        controllers: ChannelControllers::CONTROL_CHANGE,
        notes: NoteData::empty(),
    };
    let room = SysexAssembler::room_for(256, 1, 16);
    let mut assembler = SysexAssembler::new(vec![0; room].into_boxed_slice(), 256, 16);
    let start = Instant::now();
    let mut seen = Vec::new();
    let mut inquiries = Vec::new();
    let mut device_id = None;
    let mut asked_for = false;
    for ms in 0..80 {
        let now = start + Duration::from_millis(ms);
        for event in update_at(&mut s, now) {
            match event {
                SessionEvent::Midi(MidiEvent::CiPeer { device, .. }) => device_id = Some(device),
                SessionEvent::Midi(MidiEvent::CiProcess { change, .. }) => seen.push(change),
                _ => {}
            }
        }
        if !asked_for
            && seen
                .iter()
                .any(|c| matches!(c, ProcessChange::Capabilities { .. }))
            && let Some(id) = &device_id
        {
            let peers = s.midi().report_peers(id);
            let [handle] = &peers[..] else {
                panic!("one peer that can be asked: {peers:?}");
            };
            s.midi()
                .report(handle.ask(ReportRequest::new(DataControl::Full, asked)))
                .expect("the peer supports it");
            asked_for = true;
        }
        let mut answers: Vec<Ump> = Vec::new();
        for ump in fake.take_sent(raw) {
            if let Ok(message @ tutti_ump::Message::Stream(_)) = ump.decode() {
                device.reply(message, &mut answers);
                continue;
            }
            assembler.begin_block();
            let SysexStep::Complete(done) = assembler.push(ump) else {
                continue;
            };
            match CiMessage::decode(assembler.message(done).payload()) {
                Decoded::Message(CiMessage::Discovery { header, .. }) => {
                    let data = DiscoveryData::new(
                        identity(ManufacturerId::of(0x41)),
                        CiCategories::PROCESS_INQUIRY,
                        MaxSysexSize::of(256),
                    );
                    answers.extend(
                        CiMessage::DiscoveryReply {
                            header: CiHeader::new(peer),
                            to: header.source,
                            data,
                        }
                        .packets(MidiGroup::FIRST),
                    );
                }
                Decoded::Message(CiMessage::ProcessCapabilitiesInquiry { header, .. }) => {
                    answers.extend(
                        CiMessage::ProcessCapabilitiesReply {
                            header: at(CiDeviceId::FunctionBlock),
                            to: header.source,
                            features: ProcessFeatures::MIDI_MESSAGE_REPORT,
                        }
                        .packets(MidiGroup::FIRST),
                    );
                }
                Decoded::Message(CiMessage::MessageReportInquiry {
                    header,
                    control,
                    scope,
                    ..
                }) => {
                    inquiries.push((header.device_id, control, scope));
                    answers.extend(
                        CiMessage::MessageReportReply {
                            header: at(header.device_id),
                            to: header.source,
                            scope,
                        }
                        .packets(MidiGroup::FIRST),
                    );
                    answers.extend(
                        CiMessage::MessageReportEnd {
                            header: at(header.device_id),
                            to: header.source,
                        }
                        .packets(MidiGroup::FIRST),
                    );
                }
                _ => {}
            }
        }
        for ump in answers {
            assert!(fake.send_input(raw, ump, now));
        }
    }
    assert_eq!(
        inquiries,
        [(CiDeviceId::FunctionBlock, DataControl::Full, asked)],
        "one Inquiry: MIDI Message Report, on the Function Block, as asked"
    );
    let ended = [
        ProcessChange::ReportBegan {
            group: MidiGroup::FIRST,
            muid: peer,
            declared: asked,
        },
        ProcessChange::ReportEnded {
            group: MidiGroup::FIRST,
            muid: peer,
        },
    ];
    let report: Vec<_> = seen
        .iter()
        .filter(|c| !matches!(c, ProcessChange::Capabilities { .. }))
        .collect();
    assert_eq!(report, ended.iter().collect::<Vec<_>>());
}

/// **The hardware out JR-stamps only as its device's configuration says**
/// (M2-104 §7.1.6.2: JR Timestamps go to a device only once a Stream
/// Configuration Notification has said it receives them). The
/// session asks for JR off, so the output is unstamped when the device
/// settles; when the device notifies that it receives JR the next `MidiOut`
/// phase stamps, and the wire carries the JR messages; when it notifies the
/// reverse the stamps stop. There is no switch of the session's own.
#[cfg(feature = "midi-hardware")]
#[test]
fn the_hardware_out_stamps_as_the_devices_configuration_says() {
    use tutti_midi_hardware::{Capabilities, Endpoint};
    use tutti_ump::{Message, Protocol, Stream, StreamConfig};

    let (mut s, fake) = hot_plug_session();
    plug(&fake, 7, "Dev", PortDirection::Duplex);
    let _ = s.update();
    let raw = EndpointId::from_raw(7);
    s.midi().connect_input("dev").expect("connects");
    s.midi().connect_output("dev").expect("connects");

    // The device answers the session's Stream messages as an Endpoint does.
    let mut device = Endpoint::new(Capabilities {
        midi1: true,
        midi2: true,
        rx_jr: true,
        tx_jr: false,
    });
    let start = Instant::now();
    let mut settled = false;
    for ms in 0..8 {
        let now = start + Duration::from_millis(ms);
        let events = update_at(&mut s, now);
        settled |= events
            .iter()
            .any(|e| matches!(e, SessionEvent::Midi(MidiEvent::StreamSettled { .. })));
        let mut answers = Vec::new();
        for ump in fake.take_sent(raw) {
            if let Ok(message @ Message::Stream(_)) = ump.decode() {
                device.reply(message, &mut answers);
            }
        }
        for ump in answers {
            assert!(fake.send_input(raw, ump, now));
        }
    }
    assert!(settled, "the negotiation settled");
    let stamping = |s: &mut Session| s.midi().hardware_out().out.map(|o| o.is_stamping());
    assert_eq!(stamping(&mut s), Some(false), "asked for JR off");

    // One frame of output, routed as a drain does.
    let wire_after = |s: &mut Session, notification: StreamConfig, ms: u64| {
        let ump = Ump::from(Message::Stream(Stream::StreamConfigurationNotification(
            notification,
        )));
        assert!(fake.send_input(raw, ump, start));
        update_at(s, start + Duration::from_millis(ms));
        fake.take_sent(raw);
        s.midi()
            .hardware_out()
            .route(&[(Frame::new(0), key(0, 60)), (Frame::new(480), key(0, 61))]);
        fake.take_sent(raw)
    };
    let config = |rx_jr| StreamConfig {
        protocol: Protocol::Midi2,
        rx_jr,
        tx_jr: false,
    };
    let sent = wire_after(&mut s, config(true), 20);
    assert_eq!(stamping(&mut s), Some(true));
    assert!(
        sent.iter().any(|u| u.is_jr())
            && sent.iter().filter(|u| u.message_type() == 4).count() == 2,
        "{sent:?}"
    );
    let sent = wire_after(&mut s, config(false), 40);
    assert_eq!(stamping(&mut s), Some(false));
    assert!(!sent.iter().any(|u| u.is_jr()), "{sent:?}");
    assert_eq!(sent.len(), 2);
}
