//! The synth's params under a control lane, as an engine wires one: a `ControlLaneNode` whose
//! knob is the synth's own cell, sending into the synth's param port. The master volume, unison
//! detune and stereo spread are ports.
//!
//! A note at frame 0 into a saw synth, 120 BPM at 48 kHz.

use tutti_types::MaxBlock;

use tutti_graph::NodeKey;
use tutti_graph::{
    Editor, EventEdge, EventIn, EventOut, ParamIn, ParamRange, Prepare, Renderer, TransportPosition,
};
use tutti_graph::{OutPort, Source};
use tutti_midi_runtime::{MidiClipNode, TimedMidiEvent};
use tutti_modulation::{Amount, Clock, ControlProgram, Law, Layer, LayerId, Polarity, Waveform};
use tutti_nodes::automation::ControlLaneNode;
use tutti_polysynth::{Adsr, OscillatorType, PolySynth, SynthConfig, UnisonConfig, UnisonVoices};
use tutti_types::{
    Beat, BeatsPerCycle, Bpm, CentsSpan, Depth, PhaseIncrement, SampleRate, Samples, Seconds,
    Spread, UnitParam,
};
use tutti_ump::{Message, MidiChannel, NoteNumber, NoteVelocity, Voice2};

const RATE: f64 = 48_000.0;
const BLOCK: usize = 256;
const FRAMES: usize = 4_096;
const SYNTH: NodeKey = NodeKey(2);
const LANE: NodeKey = NodeKey(3);

/// A saw synth with no attack; with `unison`, three sub-voices at
/// `detune` cents and no spread.
fn synth(unison: bool, detune: f32) -> PolySynth {
    PolySynth::new(SynthConfig {
        oscillator: OscillatorType::Saw,
        envelope: Adsr {
            attack: Seconds(0.0),
            ..Default::default()
        },
        unison: unison.then_some(UnisonConfig {
            voice_count: UnisonVoices::of(3),
            detune_cents: CentsSpan::of(detune),
            stereo_spread: Spread::POINT,
            phase_randomize: false,
        }),
        ..Default::default()
    })
}

/// A lane over `span` adding a constant `depth` of it.
fn constant(depth: f32) -> ControlProgram {
    let layer = Layer {
        id: LayerId(1),
        clock: Clock::Beats {
            per_cycle: BeatsPerCycle::of(1.0),
            phase: PhaseIncrement(0.0),
        },
        wave: Waveform::Const(1.0),
        amount: Amount {
            depth: Depth::of(depth),
            polarity: Polarity::Bipolar,
            curve: Law::Linear,
        },
        depth_by: None,
    };
    ControlProgram::new(None, vec![layer], vec![]).expect("valid")
}

/// A lane on one of the synth's params: which, its range, and the constant
/// depth of the range the lane adds. Its knob is the synth's own cell, which
/// the synth's `ParamSet` hands out.
struct Drive {
    param: UnitParam,
    range: ParamRange,
    depth: f32,
}

/// Renders `synth` playing a note at frame 0, both channels, with `drive`'s
/// lane on its port when given (wired before the first block, so it drives
/// the port from frame 0: a unit's first block is not declicked).
fn render(synth: PolySynth, drive: Option<Drive>) -> [Vec<f32>; 2] {
    render_with_knob(synth, drive).0
}

/// [`render`], and the driven param's knob as the synth's `ParamSet` holds it
/// after the render (`None` without a lane).
fn render_with_knob(synth: PolySynth, drive: Option<Drive>) -> ([Vec<f32>; 2], Option<f32>) {
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate::of(RATE),
        MaxBlock::of(Samples(BLOCK)),
    ));
    let note = TimedMidiEvent::new(
        Beat(0.0),
        Message::midi2(
            MidiChannel::FIRST,
            Voice2::note_on(NoteNumber::of(60), NoteVelocity::MAX),
        ),
    )
    .unwrap();
    ed.insert(NodeKey(1), "clip", MidiClipNode::new([note]));
    let params = ed.insert(SYNTH, "synth", synth);
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
    ed.spec_mut()
        .topology
        .set_outputs((0..2).map(|port| Source::Node(OutPort { node: SYNTH, port })));
    ed.commit().expect("commits");
    let driven = drive.as_ref().map(|d| d.param);
    if let Some(d) = drive {
        let at = ParamIn {
            node: SYNTH,
            param: d.param.into(),
        };
        ed.set_param_range(at, d.range)
            .expect("the synth is placed");
        let range = ed
            .param_range_cell(at)
            .expect("the synth declares the param as a port");
        let lane = ControlLaneNode::for_unit(
            d.param,
            constant(d.depth),
            params.cell(d.param),
            Some(range),
        );
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
    r.set_transport_fn(|f| {
        TransportPosition::new(true, Bpm::of(120.0), Beat(f.get() as f64 / 24_000.0), None)
    });
    let mut out = r.render(FRAMES);
    let right = out.swap_remove(1);
    let left = out.swap_remove(0);
    ([left, right], driven.and_then(|p| params.get(p)))
}

/// **A lane drives the synth's master volume around its own cell.** The
/// knob at 0.5 plus a constant quarter of a 0..1 range plays at 0.75: every
/// sample is 1.5 times the undriven synth's at 0.5 (the volume is a plain
/// gain on the voices' sum), and the cell still holds the knob (the lane
/// sends to the port and never writes it).
#[test]
fn a_lane_drives_the_master_volume_around_its_cell() {
    let mut plain = synth(false, 0.0);
    plain.set_volume(tutti_types::Amplitude(0.5));
    let [undriven, _] = render(plain, None);

    let mut driven_synth = synth(false, 0.0);
    driven_synth.set_volume(tutti_types::Amplitude(0.5));
    let ([driven, _], knob) = render_with_knob(
        driven_synth,
        Some(Drive {
            param: UnitParam::Volume,
            range: ParamRange::new(0.0, 1.0),
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
    assert_eq!(knob, Some(0.5), "the lane never writes the knob");
}

/// **A lane drives the unison spread.** Three sub-voices at a 10-cent
/// detune with no spread are all centred, so the two channels are equal;
/// the knob at 0 plus the whole of a 0..1 range spreads them hard left and
/// right, so the channels differ.
#[test]
fn a_lane_drives_the_unison_spread() {
    let [l, r] = render(synth(true, 10.0), None);
    assert!(l.iter().any(|s| s.abs() > 0.05), "the note sounds");
    assert_eq!(l, r, "no spread: the channels are equal");

    let ([l, r], knob) = render_with_knob(
        synth(true, 10.0),
        Some(Drive {
            param: UnitParam::StereoSpread,
            range: ParamRange::new(0.0, 1.0),
            depth: 1.0,
        }),
    );
    let apart = l
        .iter()
        .zip(&r)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0, f32::max);
    assert!(apart > 0.05, "spread: the channels differ by {apart}");
    assert_eq!(knob, Some(0.0), "the lane never writes the knob");
}

/// **A lane drives the unison detune.** Three in-phase sub-voices at 0
/// cents are one saw three times over; the knob at 0 plus 20 cents (a fifth
/// of a 0..100 range) beats, and sounds as the synth does with its own
/// detune at 20 cents.
#[test]
fn a_lane_drives_the_unison_detune() {
    let ([driven, _], knob) = render_with_knob(
        synth(true, 0.0),
        Some(Drive {
            param: UnitParam::Detune,
            range: ParamRange::new(0.0, 100.0),
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
    assert_eq!(knob, Some(0.0), "the lane never writes the knob");
}
