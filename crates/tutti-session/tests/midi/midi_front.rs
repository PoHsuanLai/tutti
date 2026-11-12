//! The session's MIDI front (L1f-a): hardware through `s.midi()`, the session's
//! one MPE mode, a take recorded with `s.midi().record`, its placement, and a
//! saved route rule.

use std::sync::{Arc, Mutex};

use tutti_fixtures::midi;
use tutti_graph::{
    Cx, EventCapacity, EventKind, ForkByClone, Instrument, IntoNode, Io, Node, NodeParts, Prepare,
    Shape, Status,
};
use tutti_midi_protocol::MpeMode;
use tutti_session::{MidiError, Session, SessionInstrument, SessionNode, TakePlacement};
use tutti_types::{ChannelLayout, Frame, InterleavedMut, MaxBlock, SampleRate, Samples};
use tutti_ump::{MidiChannel, MidiGroup, NoteNumber, Ump};

const RATE: f64 = 48_000.0;

fn offline() -> Session {
    Session::builder()
        .build_offline(
            SampleRate(RATE),
            ChannelLayout::STEREO,
            MaxBlock::of(Samples(1_024)),
        )
        .expect("builds")
}

fn blocks(s: &Session, n: usize) {
    let mut buf = vec![0.0f32; 512 * 2];
    for _ in 0..n {
        s.render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO));
    }
}

type Seen = Arc<Mutex<Vec<(u64, [u32; 4])>>>;

/// An event sink: what its event input hears, by absolute frame.
struct Tap(Seen);

impl Node for Tap {
    fn shape(&self) -> Shape {
        Shape::EMPTY.with_events(1, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, io: Io<'_>) -> Status {
        for e in io.events(0) {
            if let EventKind::Midi(Ump(w)) = e.kind {
                self.0
                    .lock()
                    .unwrap()
                    .push((cx.env.frame.get() + u64::from(e.offset.get()), w));
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
/// through `live_input`, are what `record`'s reader drains: the packets as
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
    let live = s.midi().live_input(target).expect("the same live input");
    let _ = s.update();
    let (c, k) = (MidiChannel::of(2), NoteNumber::of(60));
    live.note_on(Frame(600), c, k, 0x8000).unwrap();
    live.note_off(Frame(900), c, k).unwrap();
    blocks(&s, 3);

    let got: Vec<_> = take.drain().map(|e| (e.frame, e.ump)).collect();
    assert_eq!(
        got,
        [
            (Frame(600), midi::note_on(MidiGroup::FIRST, c, 60, 0x8000)),
            (Frame(900), midi::note_off(MidiGroup::FIRST, c, 60, 0)),
        ]
    );
    assert_eq!(take.drain().count(), 0, "a second drain is empty");
    assert_eq!(take.take_lost(), 0);
    let heard: Vec<u64> = seen.lock().unwrap().iter().map(|(f, _)| *f).collect();
    assert_eq!(heard, [600, 900], "recording took nothing from the target");
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
    assert_eq!(p.place(Frame(1_000)), Frame(660));
    assert_eq!(p.place(Frame(300)), Frame(10), "saturates at the start");
    assert_eq!(
        TakePlacement::default().place(Frame(77)),
        Frame(77),
        "no compensation, no move"
    );
}

// --- MPE ---------------------------------------------------------------------

/// Records the MPE mode it is built in; plays as an instrument (five outputs
/// and an event input).
struct Probe(ForkByClone<Five>, Arc<Mutex<Vec<MpeMode>>>);

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
impl SessionInstrument for Probe {
    fn bind_mpe(self, mode: MpeMode) -> Self {
        self.1.lock().unwrap().push(mode);
        self
    }
}

/// **The session's MPE mode is the ingest node's and the instrument's.** The
/// builder's is the starting mode; `set_mpe` publishes it to the ingest node
/// and an instrument added after is built in it, one that was added before is
/// not touched.
#[test]
fn the_sessions_mpe_mode_reaches_the_ingest_and_the_next_instrument() {
    let rotation = MpeMode::SingleChannelRotation {
        channel: MidiChannel::of(3),
    };
    let mut s = Session::builder()
        .mpe_mode(rotation)
        .build_offline(
            SampleRate(RATE),
            ChannelLayout::STEREO,
            MaxBlock::of(Samples(1_024)),
        )
        .expect("builds");
    assert_eq!(s.midi().mpe(), rotation, "the builder's mode to begin with");

    let built = Arc::new(Mutex::new(Vec::new()));
    s.add_instrument(Probe(ForkByClone(Five), Arc::clone(&built)))
        .expect("an instrument");
    assert_eq!(*built.lock().unwrap(), [rotation]);

    s.midi().set_mpe(MpeMode::Disabled);
    assert_eq!(s.midi().mpe(), MpeMode::Disabled);
    assert_eq!(
        s.midi_nodes()
            .expect("the builder's MIDI nodes")
            .input_controls
            .pending_mpe_mode(),
        Some(MpeMode::Disabled),
        "the ingest node was asked"
    );
    s.add_instrument(Probe(ForkByClone(Five), Arc::clone(&built)))
        .expect("an instrument");
    assert_eq!(*built.lock().unwrap(), [rotation, MpeMode::Disabled]);
}

/// **A synth added through the session follows the session's MPE mode over its
/// own configuration.**
#[cfg(feature = "synth")]
#[test]
fn a_synth_added_through_the_session_takes_the_sessions_mpe_mode() {
    use tutti_polysynth::{PolySynth, SynthConfig};
    let rotation = MpeMode::SingleChannelRotation {
        channel: MidiChannel::of(3),
    };
    // The synth's own config says rotation on channel 5; the session says 3.
    let own = MpeMode::SingleChannelRotation {
        channel: MidiChannel::of(5),
    };
    let synth = PolySynth::new(SynthConfig {
        mpe: own,
        ..SynthConfig::default()
    });
    assert_eq!(synth.mpe(), &own);
    assert_eq!(
        SessionInstrument::bind_mpe(synth, rotation).mpe(),
        &rotation,
        "the hook overrides the config"
    );
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
                SampleRate(RATE),
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

        assert!(s.midi().disconnect_input("keys"));
        assert!(!s.midi().disconnect_input("keys"), "not open any more");
        assert!(s.midi().disconnect_output());
        assert!(!s.midi().disconnect_output());
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
        assert!(!bare.midi().disconnect_input("keys"));
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
}
