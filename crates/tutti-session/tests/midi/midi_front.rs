//! The session's MIDI front: hardware through `s.midi()`, the session's
//! one MPE mode, a take recorded with `s.midi().record`, its placement, and a
//! saved route rule.

use std::sync::Arc;

use crate::common::{Seen, Tap, blocks, offline};
use tutti_fixtures::midi;
use tutti_graph::{
    Cx, EventCapacity, ForkByClone, Instrument, IntoNode, Io, Node, NodeParts, Prepare, Shape,
    Status,
};
use tutti_midi_runtime::TimedMidiEvent;
use tutti_session::{MidiError, Session, SessionNode, TakePlacement};
use tutti_types::{Beat, ChannelLayout, Frame, MaxBlock, SampleRate, Samples};
use tutti_ump::{MidiChannel, MidiGroup, NoteNumber, NoteVelocity, Voice2};

const RATE: f64 = 48_000.0;

/// Plays as an instrument (five outputs and an event input).
struct Probe(ForkByClone<Five>);

#[derive(Clone)]
struct Five;
impl Node for Five {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::STEREO).with_events(1, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, _: Io<'_>) -> Status {
        Status::Silent
    }
    fn reset(&mut self) {}
}
impl IntoNode for Probe {
    type Controls = ();
    fn into_parts(self) -> NodeParts<()> {
        self.0.into_parts()
    }
}
impl Instrument for Probe {}
impl SessionNode for Probe {}

// --- Recording a take --------------------------------------------------------

/// A silent stereo source that declares 256 frames of latency, so the graph's
/// output time is 256 frames behind its nodes: what an aligned tap would delay by.
#[derive(Clone)]
struct Latent;
impl Node for Latent {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::STEREO)
            .with_latency(tutti_types::Latency::new(Samples(256)))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, _: Io<'_>) -> Status {
        Status::Silent
    }
    fn reset(&mut self) {}
}

/// **A take reads what was played through the live input, frame for frame, and
/// the target still hears it.** A note on at frame 600 and off at 900, sent
/// through `input`, are what `record`'s reader drains: the packets as
/// sent, on those frames, in order (the frames are the graph's own, **not**
/// delayed by the 256 frames of latency another path of this graph has); a
/// second drain is empty; and the tap target heard both as well.
#[test]
fn a_take_records_the_live_input_on_its_frames_and_the_target_still_hears_it() {
    let mut s = offline();
    let seen = Seen::default();
    let (target, ()) = s.graph().add(Tap(Arc::clone(&seen)));
    let (latent, ()) = s.graph().add(ForkByClone(Latent));
    s.graph().connect(latent, tutti_session::Root).unwrap();
    let mut take = s
        .midi()
        .record(target, EventCapacity::of(64))
        .expect("the tap takes MIDI");
    let mut live = s.midi().input(target).expect("the take left the keys");
    let _ = s.update();
    let (c, k) = (MidiChannel::of(2), NoteNumber::of(60));
    live.note_on(Frame::new(600), c, k, NoteVelocity::new(0x8000))
        .unwrap();
    live.note_off(Frame::new(900), c, k).unwrap();
    blocks(&s, 3);

    let got: Vec<_> = take.drain().map(|e| (e.frame, e.ump)).collect();
    assert_eq!(
        got,
        [
            (
                Frame::new(600),
                midi::note_on(MidiGroup::FIRST, c, 60, 0x8000)
            ),
            (Frame::new(900), midi::note_off(MidiGroup::FIRST, c, 60, 0)),
        ]
    );
    assert_eq!(take.drain().count(), 0, "a second drain is empty");
    assert_eq!(take.take_lost(), 0);
    let heard: Vec<u64> = seen.lock().unwrap().iter().map(|(f, _)| *f).collect();
    assert_eq!(heard, [600, 900], "recording took nothing from the target");
}

/// **A take of an instrument records what its handle's keys play**, frame for
/// frame, though the keys are not `Midi::input`'s: the tap is on the inlet
/// that feeds the instrument, wherever its keys came from.
#[test]
fn a_take_of_an_instrument_records_what_its_handle_plays() {
    let mut s = offline();
    let mut synth = s
        .add_instrument(Probe(ForkByClone(Five)))
        .expect("an instrument")
        .handle;
    let mut take = s
        .midi()
        .record(synth.node(), EventCapacity::of(64))
        .expect("an instrument takes MIDI");
    let _ = s.update();
    let (c, k) = (MidiChannel::of(2), NoteNumber::of(60));
    synth
        .keys_mut()
        .note_on(Frame::new(600), c, k, NoteVelocity::new(0x8000))
        .unwrap();
    synth.keys_mut().note_off(Frame::new(900), c, k).unwrap();
    blocks(&s, 3);
    let got: Vec<_> = take.drain().map(|e| (e.frame, e.ump)).collect();
    assert_eq!(
        got,
        [
            (
                Frame::new(600),
                midi::note_on(MidiGroup::FIRST, c, 60, 0x8000)
            ),
            (Frame::new(900), midi::note_off(MidiGroup::FIRST, c, 60, 0)),
        ]
    );
}

/// **An instrument has one live input, its handle's**: asking `Midi::input`
/// for a second is refused by name, and nothing is opened.
#[test]
fn an_instrument_has_its_handles_keys_and_no_second_live_input() {
    let mut s = offline();
    let synth = s
        .add_instrument(Probe(ForkByClone(Five)))
        .expect("an instrument")
        .handle;
    let key = synth.node().erase();
    assert!(matches!(
        s.midi().input(key),
        Err(MidiError::InstrumentKeys(n)) if n == key
    ));
    assert!(
        s.graph_ref()
            .feed(key, tutti_session::Feeder::LiveInput)
            .is_empty(),
        "no second inlet was opened"
    );
}

/// **The instrument's key is typed, so a source of the caller's own wires into
/// its event input** (`events_in` is a method of `Key`, which an erased
/// `NodeKey` has not).
#[test]
fn an_instruments_typed_key_takes_a_wire_of_the_callers_own() {
    let mut s = offline();
    let synth = s
        .add_instrument(Probe(ForkByClone(Five)))
        .expect("an instrument")
        .handle;
    let (inlet, _writer) = s.graph().open_event_inlet(EventCapacity::of(8));
    s.graph()
        .connect(inlet.events_out(), synth.node().events_in())
        .expect("the instrument's main event input takes a second source");
    assert!(
        s.graph_ref()
            .sources(synth.node().events_in())
            .unwrap()
            .contains(&inlet.events_out()),
        "the caller's wire is one of the input's sources"
    );
}

/// A target that takes no MIDI cannot be recorded.
#[test]
fn recording_a_target_with_no_event_input_is_refused() {
    let mut s = offline();
    let (dc, _) = s
        .graph()
        .add(ForkByClone(tutti_nodes::testing::Const::mono(0.5)));
    assert!(matches!(
        s.midi().record(dc, EventCapacity::of(8)),
        Err(MidiError::NoEventInput(n)) if n == dc.erase()
    ));
}

/// **A take is placed `F - (input + output + lead) + trim`**, at 0 where that
/// reaches before the clock's start: 1,000 - (100 + 200 + 50) + 10 = 660, and
/// a frame inside the compensation lands on the trim alone.
#[test]
fn a_take_is_placed_by_the_documented_figures() {
    let p = TakePlacement {
        input: Samples(100),
        output: Samples(200),
        lead: Samples(50),
        trim: Samples(10),
    };
    assert_eq!(p.place(Frame::new(1_000)), Frame::new(660));
    assert_eq!(
        p.place(Frame::new(300)),
        Frame::new(10),
        "saturates at the start"
    );
    assert_eq!(
        TakePlacement::default().place(Frame::new(77)),
        Frame::new(77),
        "no compensation, no move"
    );
}

// --- MPE ---------------------------------------------------------------------

/// A clip of the first Group's events: each `(beat, packet)`.
fn clip(events: impl IntoIterator<Item = (f64, tutti_ump::Ump)>) -> Vec<TimedMidiEvent> {
    events
        .into_iter()
        .map(|(beat, ump)| TimedMidiEvent::new(Beat(beat), ump).unwrap())
        .collect()
}

/// What a tap heard, as `(channel, message)` of MIDI 2.0 channel voice.
fn voices(seen: &Seen) -> Vec<(u8, Voice2)> {
    seen.lock()
        .unwrap()
        .iter()
        .filter_map(|(_, e)| {
            let (address, voice) = tutti_midi_protocol::channel_voice(*e)?;
            Some((address.channel.get(), voice))
        })
        .collect()
}

/// A member channel's note and bend, then the manager's Damper Pedal, of a
/// lower zone of three members: what the session's mode makes of a clip.
fn mpe_clip() -> Vec<TimedMidiEvent> {
    let (g, member) = (MidiGroup::FIRST, MidiChannel::of(1));
    clip([
        (0.0, midi::note_on(g, member, 60, 0x8000)),
        (0.1, midi::pitch_bend(g, member, 0xC000_0000)),
        (
            0.2,
            midi::cc(
                g,
                MidiChannel::FIRST,
                tutti_ump::CCNumber::SUSTAIN,
                u32::MAX,
            ),
        ),
    ])
}

fn play_for(s: &mut Session, blocks: usize) {
    let _ = s.update();
    crate::common::play(s);
    crate::common::blocks(s, blocks);
}

/// **A clip is transformed in the session's MPE mode**: `Midi::play_clip`
/// wires the clip through an MPE transform that starts in the session's mode
/// (the builder's here), so a member channel's bend reaches the target as a
/// per-note bend on the note that channel holds, and the manager's Damper Pedal
/// as the pedal of every channel of its zone (M1-100 section 2.3.1).
#[test]
fn a_clip_plays_through_the_sessions_mpe_mode() {
    let mut s = Session::builder()
        .mpe_mode(midi::lower_zone(3))
        .build_offline(
            SampleRate::SR_48K,
            ChannelLayout::STEREO,
            MaxBlock::of(Samples(1_024)),
        )
        .expect("builds");
    assert_eq!(s.midi().mpe(), midi::lower_zone(3));
    let (target, seen) = crate::common::tap(&mut s);
    s.midi().play_clip(target, Beat(0.0), mpe_clip()).unwrap();
    play_for(&mut s, 40);
    let got = voices(&seen);
    assert!(
        got.iter().any(|(c, v)| *c == 1
            && matches!(v, Voice2::PerNotePitchBend { note, .. } if note.get() == 60)),
        "{got:?}"
    );
    let pedals: Vec<u8> = got
        .iter()
        .filter(|(_, v)| {
            matches!(v, Voice2::ControlChange { controller, .. }
                if *controller == tutti_ump::CCNumber::SUSTAIN)
        })
        .map(|(c, _)| *c)
        .collect();
    assert_eq!(pedals, [0, 1, 2, 3]);
}

/// **`set_mpe` reaches a clip already in the graph**: a session that began with
/// no MPE plays its clip with the manager's pedal on one channel; after
/// `set_mpe` the same clip's pedal is the zone's, since the transform the clip
/// was wired through took the new mode. (The mode used to reach only a synth
/// added afterwards.)
#[test]
fn set_mpe_reaches_a_clip_already_in_the_graph() {
    let mut s = crate::common::offline();
    let (target, seen) = crate::common::tap(&mut s);
    s.midi().play_clip(target, Beat(0.0), mpe_clip()).unwrap();
    let _ = s.update();
    s.midi().set_mpe(midi::lower_zone(3));
    assert_eq!(s.midi().mpe(), midi::lower_zone(3));
    play_for(&mut s, 40);
    let got = voices(&seen);
    let pedals = got
        .iter()
        .filter(|(_, v)| {
            matches!(v, Voice2::ControlChange { controller, .. }
                if *controller == tutti_ump::CCNumber::SUSTAIN)
        })
        .count();
    assert_eq!(pedals, 4, "{got:?}");
    assert!(
        got.iter()
            .any(|(_, v)| matches!(v, Voice2::PerNotePitchBend { .. })),
        "{got:?}"
    );
}

/// **The transform is the clip's wiring**: removing the clip takes its
/// transform out of the graph, and `retarget` moves both to the new target (the
/// transform still feeds it).
#[test]
fn a_clips_transform_goes_with_the_clip_and_follows_a_retarget() {
    let mut s = crate::common::offline();
    let (first, _) = crate::common::tap(&mut s);
    let (second, seen) = crate::common::tap(&mut s);
    let before = s.graph().node_count();
    s.midi().play_clip(first, Beat(0.0), mpe_clip()).unwrap();
    assert_eq!(
        s.graph().node_count(),
        before + 2,
        "the clip and its transform"
    );
    let clip = s.midi().clip(first).expect("the clip").erase();
    s.midi().retarget(clip, second).unwrap();
    assert!(s.midi().clip(first).is_none());
    assert_eq!(s.midi().clip(second).map(|k| k.erase()), Some(clip));
    s.midi().set_mpe(midi::lower_zone(3));
    play_for(&mut s, 40);
    assert!(
        !voices(&seen).is_empty(),
        "the second target hears the clip"
    );

    s.remove(clip).unwrap();
    assert_eq!(s.graph().node_count(), before, "the transform went too");
}

// --- A saved route rule ------------------------------------------------------

/// **A saved route rule keyed by an app's own id reads back, and a channel the
/// wire cannot hold is refused**, not clamped: the rule's Group and channel are
/// range-checked values.
#[cfg(feature = "serde")]
#[test]
fn a_route_rule_keyed_by_the_apps_id_round_trips_and_refuses_a_bad_channel() {
    use tutti_session::MidiRouteRule;
    let rule = MidiRouteRule::<String>::for_channel(MidiChannel::of(4))
        .in_group(MidiGroup::of(2))
        .to("lead".to_string())
        .to("pad".to_string())
        .disabled();
    let json = serde_json::to_string(&rule).unwrap();
    let back: MidiRouteRule<String> = serde_json::from_str(&json).unwrap();
    assert_eq!(back, rule);
    assert!(!back.enabled);
    assert_eq!(back.targets, ["lead", "pad"]);
    // Both states of `enabled` survive, so neither is a default filled in.
    let on = MidiRouteRule::<String>::for_channel(MidiChannel::of(4)).to("lead".to_string());
    let on_back: MidiRouteRule<String> =
        serde_json::from_str(&serde_json::to_string(&on).unwrap()).unwrap();
    assert_eq!(on_back, on);
    assert!(on_back.enabled);

    let bad = json.replace("\"channel\":4", "\"channel\":16");
    assert_ne!(bad, json, "the channel was in the text");
    assert!(serde_json::from_str::<MidiRouteRule<String>>(&bad).is_err());
}

// --- Hardware through the view -----------------------------------------------

#[cfg(feature = "midi-hardware")]
mod hardware {
    use super::*;
    use tutti_midi_hardware::test_support::{FakeBackend, FakeCounters, endpoint};
    use tutti_midi_hardware::{EndpointId, Identity, PortDirection};
    use tutti_session::Resolution;

    fn session() -> (Session, FakeCounters) {
        let (backend, fake) = FakeBackend::build_with(vec![], vec![], usize::MAX);
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

    /// A device the OS names by a unit, so its saved identity is exact.
    fn unit(raw: u64, name: &str, unit: &str) -> tutti_midi_hardware::EndpointInfo {
        let mut e = endpoint(raw, name);
        e.identity = Identity::Product(unit.into());
        e
    }

    /// **The view lists the devices and opens and closes them**: an input by
    /// name, an output by id; the output gives the hardware out its sink; a
    /// disconnect says whether one was open.
    #[test]
    fn the_view_lists_connects_and_disconnects_devices() {
        let (mut s, fake) = session();
        fake.plug(endpoint(1, "Keys"), PortDirection::Input);
        fake.plug(endpoint(10, "Synth"), PortDirection::Output);
        let _ = s.update();

        let names: Vec<String> = s.midi().devices().into_iter().map(|d| d.name).collect();
        assert_eq!(names, ["Keys", "Synth"], "inputs and outputs, by id");

        s.midi().connect_input("keys").expect("connects");
        assert_eq!(
            s.testing_midi_io().unwrap().connected_input_names(),
            ["Keys"]
        );
        assert!(s.midi().hardware_out().out.is_none(), "no output yet");
        let synth = s
            .midi()
            .devices()
            .into_iter()
            .find(|d| d.name == "Synth")
            .unwrap();
        s.midi().connect_output(&synth.id).expect("connects");
        assert!(
            s.midi().hardware_out().out.is_some(),
            "the output has a sink to drain to"
        );

        s.midi().disconnect_input("keys").expect("it was open");
        assert!(
            matches!(
                s.midi().disconnect_input("keys"),
                Err(MidiError::InputNotOpen(_))
            ),
            "not open any more"
        );
        s.midi().disconnect_output().expect("it was open");
        assert!(matches!(
            s.midi().disconnect_output(),
            Err(MidiError::OutputNotOpen)
        ));
    }

    /// **A name that matches nothing is the hardware's error, carried as the
    /// source**, and a session with no OS layer refuses with its own.
    #[test]
    fn refusals_are_one_error_with_the_cause_as_its_source() {
        use std::error::Error as _;
        let (mut s, _fake) = session();
        let err = s.midi().connect_input("nothing").unwrap_err();
        assert!(matches!(
            err,
            MidiError::Hardware(tutti_midi_hardware::Error::NoDeviceNamed { .. })
        ));
        assert!(err.source().is_some(), "the hardware error is the source");
        let session_error: tutti_session::Error = err.into();
        assert!(matches!(session_error, tutti_session::Error::Midi(_)));

        let mut bare = offline();
        assert!(matches!(
            bare.midi().connect_input("keys"),
            Err(MidiError::NoHardware)
        ));
        assert!(bare.midi().devices().is_empty());
        assert!(matches!(
            bare.midi().disconnect_input("keys"),
            Err(MidiError::NoHardware)
        ));
        assert!(matches!(
            bare.midi().disconnect_output(),
            Err(MidiError::NoHardware)
        ));
    }

    /// **Only a device that is surely the saved one is connected unasked.** A
    /// device named by its unit comes back `Connected` (in the directions it was
    /// saved with); one that only has the saved name (an ALSA identity) is
    /// returned to ask about and nothing is opened; two of one name are
    /// candidates; none is `Missing`.
    #[test]
    fn a_saved_device_is_bound_only_when_it_is_exact() {
        let (mut s, fake) = session();
        fake.plug(unit(1, "Pad In", "unit-1"), PortDirection::Input);
        fake.plug(unit(2, "Pad Out", "unit-1"), PortDirection::Output);
        fake.plug(endpoint(3, "Keys"), PortDirection::Input);
        let _ = s.update();
        let all = s.midi().devices();
        let saved = |name: &str| all.iter().find(|d| d.name == name).cloned().unwrap();
        let (pad_out, keys) = (saved("Pad Out"), saved("Keys"));

        assert_eq!(
            s.midi().reconnect_saved(&pad_out).unwrap(),
            Resolution::Connected(pad_out.id.clone())
        );
        assert!(s.midi().hardware_out().out.is_some(), "saved as an output");

        assert_eq!(
            s.midi().reconnect_saved(&keys).unwrap(),
            Resolution::SameName(keys.clone())
        );
        assert!(
            s.testing_midi_io()
                .unwrap()
                .connected_input_names()
                .is_empty(),
            "a same-name device is not opened unasked"
        );

        // The unit's input is gone: the saved output now names a port of a unit
        // with one other present device, so it can no longer be told for sure.
        fake.unplug(EndpointId::from_raw(2));
        let _ = s.update();
        assert!(matches!(
            s.midi().reconnect_saved(&pad_out).unwrap(),
            Resolution::Ambiguous(c) if c.len() == 1 && c[0].name == "Pad In"
        ));
        fake.unplug(EndpointId::from_raw(1));
        fake.unplug(EndpointId::from_raw(3));
        let _ = s.update();
        assert_eq!(
            s.midi().reconnect_saved(&pad_out).unwrap(),
            Resolution::Missing
        );
    }

    /// **What a device says of itself is read through the session's front**:
    /// nothing for a device that is not open, and a saved device of an open one
    /// that has said nothing knows nothing, so it confirms as `Unknown` (never
    /// as the same unit).
    #[test]
    fn a_device_that_has_said_nothing_is_unknown() {
        use tutti_midi_hardware::{Learned, Match};
        let (mut s, fake) = session();
        fake.plug(endpoint(3, "Keys"), PortDirection::Input);
        let _ = s.update();
        let keys = s.midi().devices().into_iter().next().unwrap();
        assert!(s.midi().learned(&keys.id).is_empty());
        assert!(s.midi().saved(&keys.id).is_none(), "not open");

        s.midi().connect_input(&keys.id).unwrap();
        let saved = s.midi().saved(&keys.id).expect("open");
        assert_eq!(saved.info(), &keys);
        assert_eq!(saved.learned(), &Learned::new());
        assert_eq!(s.midi().confirm(&keys.id, &saved), Match::Unknown);
    }
}
