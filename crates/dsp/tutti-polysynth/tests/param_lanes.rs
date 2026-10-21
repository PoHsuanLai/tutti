//! The synth's params under a control lane, as an engine wires one (doc 014
//! §4.2): a `ControlLaneNode` whose knob is the synth's own cell, sending
//! into the synth's param port. The master volume, unison detune and
//! stereo spread are ports ([`SYNTH_PARAMS`]).
//!
//! A note at frame 0 into a saw synth, 120 BPM at 48 kHz.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use tutti_core::graph::{OutPort, Source};
use tutti_core::{
    AtomicF32, Beat, BeatDuration, Bpm, Cents, Depth, NodeKey, PhaseIncrement, SampleRate, Samples,
    Seconds, Spread, UnitParam,
};
use tutti_graph::{
    Editor, EventEdge, EventIn, EventOut, ParamIn, ParamRange, Prepare, Renderer, Transport,
};
use tutti_midi_runtime::{MidiClipNode, TimedMidiEvent};
use tutti_midi_types::{MidiChannel, MidiEvent, MidiGroup};
use tutti_mod::{Amount, Clock, ControlProgram, CurveType, Layer, LayerId, Polarity, Wave};
use tutti_nodes::automation::ControlLaneNode;
use tutti_polysynth::{EnvelopeConfig, OscillatorType, PolySynth, SynthConfig, UnisonConfig};

const RATE: f64 = 48_000.0;
const BLOCK: usize = 256;
const FRAMES: usize = 4_096;
const SYNTH: NodeKey = NodeKey(2);
const LANE: NodeKey = NodeKey(3);

/// A saw synth with no attack; with `unison`, three sub-voices at
/// `detune` cents and no spread.
fn synth(unison: bool, detune: f32) -> PolySynth {
    PolySynth::new(SynthConfig {
        sample_rate: SampleRate(RATE),
        oscillator: OscillatorType::Saw,
        envelope: EnvelopeConfig {
            attack: Seconds(0.0),
            ..Default::default()
        },
        unison: unison.then_some(UnisonConfig {
            voice_count: 3,
            detune_cents: Cents(detune),
            stereo_spread: Spread::POINT,
            phase_randomize: false,
        }),
        ..Default::default()
    })
    .expect("synth builds")
}

/// A lane over `span` adding a constant `depth` of it.
fn constant(depth: f32) -> ControlProgram {
    let layer = Layer {
        id: LayerId(1),
        clock: Clock::Beats {
            per_cycle: BeatDuration(1.0),
            phase: PhaseIncrement(0.0),
        },
        wave: Wave::Const(1.0),
        amount: Amount {
            depth: Depth(depth),
            polarity: Polarity::Bipolar,
            curve: CurveType::Linear,
        },
        depth_by: None,
    };
    ControlProgram::new(None, vec![layer], vec![]).expect("valid")
}

/// A lane on one of the synth's params: which, its range, the knob cell,
/// and the constant depth of the range the lane adds.
struct Drive {
    param: UnitParam,
    range: ParamRange,
    knob: Arc<AtomicF32>,
    depth: f32,
}

/// Renders `synth` playing a note at frame 0, both channels, with `drive`'s
/// lane on its port when given (wired before the first block, so it drives
/// the port from frame 0: a unit's first block is not declicked).
fn render(synth: PolySynth, drive: Option<Drive>) -> [Vec<f32>; 2] {
    let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(RATE), Samples(BLOCK)));
    let note = TimedMidiEvent::new(
        Beat(0.0),
        MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0xFFFF),
    );
    ed.insert(NodeKey(1), "clip", MidiClipNode::new([note]));
    ed.insert(SYNTH, "synth", synth);
    ed.spec_mut().connect_events(
        EventIn {
            node: SYNTH,
            port: 0,
        },
        EventEdge::Direct(EventOut {
            node: NodeKey(1),
            port: 0,
        }),
    );
    ed.spec_mut().topology.outputs = (0..2)
        .map(|port| Source::Node(OutPort { node: SYNTH, port }))
        .collect();
    ed.commit().expect("commits");
    if let Some(d) = drive {
        let at = ParamIn {
            node: SYNTH,
            param: d.param,
        };
        ed.set_param_range(at, d.range)
            .expect("the synth is placed");
        let range = ed
            .param_range_cell(at)
            .expect("the synth declares the param as a port");
        let lane = ControlLaneNode::for_unit(d.param, constant(d.depth), Some(d.knob), Some(range));
        ed.insert(LANE, "lane", lane);
        ed.spec_mut().connect_param(
            at,
            EventOut {
                node: LANE,
                port: 0,
            },
        );
        ed.commit().expect("the lane commits");
    }
    let mut r = Renderer::new(ed, exec);
    r.set_transport_fn(|f| Transport::new(true, Bpm(120.0), Beat(f.get() as f64 / 24_000.0), None));
    let mut out = r.render(FRAMES);
    let right = out.swap_remove(1);
    let left = out.swap_remove(0);
    [left, right]
}

/// **A lane drives the synth's master volume around its own cell.** The
/// knob at 0.5 plus a constant quarter of a 0..1 range plays at 0.75: every
/// sample is 1.5 times the undriven synth's at 0.5 (the volume is a plain
/// gain on the voices' sum), and the cell still holds the knob (the lane
/// sends to the port and never writes it).
///
/// Mutation (run): the synth reading its cell, not the driven port
/// (`block_driven(&io, 0)` replaced by `None`) → every sample is 1.0 times
/// → fails. Mutation (run): `SYNTH_PARAMS` not declared in the shape
/// → `param_range_cell` is `None` → fails.
#[test]
fn a_lane_drives_the_master_volume_around_its_cell() {
    let plain = synth(false, 0.0);
    plain.set_volume(0.5);
    let [undriven, _] = render(plain, None);

    let driven_synth = synth(false, 0.0);
    driven_synth.set_volume(0.5);
    let knob = driven_synth.volume_atomic();
    let [driven, _] = render(
        driven_synth,
        Some(Drive {
            param: UnitParam::Volume,
            range: ParamRange::new(0.0, 1.0),
            knob: knob.clone(),
            depth: 0.25,
        }),
    );
    assert!(undriven.iter().any(|s| s.abs() > 0.05), "the note sounds");
    for (i, (&d, &u)) in driven.iter().zip(&undriven).enumerate() {
        assert!(
            (d - 1.5 * u).abs() <= 1e-6,
            "frame {i}: driven {d} vs 1.5 x {u}"
        );
    }
    assert_eq!(
        knob.load(Ordering::Acquire),
        0.5,
        "the lane never writes the knob"
    );
}

/// **A lane drives the unison spread.** Three sub-voices at a 10-cent
/// detune with no spread are all centred, so the two channels are equal;
/// the knob at 0 plus the whole of a 0..1 range spreads them hard left and
/// right, so the channels differ.
///
/// Mutation (run): the unison engine reading its spread cell, not
/// the driven port (`block_driven(&io, 2)` replaced by `None`) → the
/// channels stay equal → fails.
#[test]
fn a_lane_drives_the_unison_spread() {
    let [l, r] = render(synth(true, 10.0), None);
    assert!(l.iter().any(|s| s.abs() > 0.05), "the note sounds");
    assert_eq!(l, r, "no spread: the channels are equal");

    let s = synth(true, 10.0);
    let knob = s.spread_atomic().expect("unison");
    let [l, r] = render(
        s,
        Some(Drive {
            param: UnitParam::StereoSpread,
            range: ParamRange::new(0.0, 1.0),
            knob: knob.clone(),
            depth: 1.0,
        }),
    );
    let apart = l
        .iter()
        .zip(&r)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0, f32::max);
    assert!(apart > 0.05, "spread: the channels differ by {apart}");
    assert_eq!(
        knob.load(Ordering::Acquire),
        0.0,
        "the lane never writes the knob"
    );
}

/// **A lane drives the unison detune.** Three in-phase sub-voices at 0
/// cents are one saw three times over; the knob at 0 plus 20 cents (a fifth
/// of a 0..100 range) beats, and sounds as the synth does with its own
/// detune at 20 cents.
///
/// Mutation (run): the unison engine reading its detune cell, not
/// the driven port (`block_driven(&io, 1)` replaced by `None`) → the
/// driven synth stays at 0 cents → fails.
#[test]
fn a_lane_drives_the_unison_detune() {
    let s = synth(true, 0.0);
    let knob = s.detune_atomic().expect("unison");
    let [driven, _] = render(
        s,
        Some(Drive {
            param: UnitParam::Detune,
            range: ParamRange::new(0.0, 100.0),
            knob: knob.clone(),
            depth: 0.2,
        }),
    );
    let [at_20, _] = render(synth(true, 20.0), None);
    let [at_0, _] = render(synth(true, 0.0), None);
    for (i, (&d, &e)) in driven.iter().zip(&at_20).enumerate() {
        assert!(
            (d - e).abs() <= 1e-4,
            "frame {i}: driven {d} vs 20 cents {e}"
        );
    }
    assert!(
        driven.iter().zip(&at_0).any(|(d, e)| (d - e).abs() > 1e-2),
        "20 cents sounds unlike 0 cents"
    );
    assert_eq!(
        knob.load(Ordering::Acquire),
        0.0,
        "the lane never writes the knob"
    );
}
