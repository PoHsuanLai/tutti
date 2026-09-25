//! **One MIDI clip source feeding two synths plays its notes once**, through
//! the native graph engine.
//!
//! A `MidiClipSource` polls the live transport through a `BeatCursor` on
//! every block it is asked for, and keeps its own event cursor; both are
//! shared state. Installed on a synth whose clone also runs (the two share
//! their `MidiInPort`, and so the source), both synths poll it in every
//! 64-frame `Legacy` call. The first poll of a chunk takes the chunk's
//! events; the second finds the event cursor already past them and the beat
//! cursor where it was, so the clip plays once, on whichever synth polls
//! first, as it did through `Net`. (Until doc 013 Phase 3 PR 15 the graph was
//! also compared, bit for bit, with a `Net` engine rendering the same two
//! synths. With that backend gone the oracle is the property itself: the
//! pair renders what one synth playing the clip alone renders.)
//!
//! What this protects against is a render that walks one node's whole block
//! and then the next's (node-major) over a timeline moved per chunk: the
//! second synth would see the beat go back to the block's start, read it as
//! a rewind (`BeatWindowSync::Rewound`), rewind the shared event cursor and
//! refire the notes, and the first would do the same at the next block. The
//! graph engine renders chunk-major while a `Legacy` unit is present (doc
//! 013, "chunk-major `Legacy` compatibility mode"), so nothing rewinds.
//!
//! Mutation (run): `GraphRender::settle` ignoring `has_legacy` (whole
//! device blocks) → the shared clip renders silence at both block sizes →
//! fails (the "clip is silent" check; the `Net` comparison this replaced
//! caught it first).

use std::sync::Arc;

use tutti_core::graph::{OutPort, Source};
use tutti_core::{
    Beat, ChannelLayout, Engine, InterleavedMut, MotionEvent, NodeKey, SampleRate, Samples,
    Timeline, Transport,
};
use tutti_graph::{Editor, Legacy, Prepare};
use tutti_midi_runtime::{MidiClipSource, TimedMidiEvent};
use tutti_midi_types::convert::midi1_velocity_to_midi2;
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::{MidiChannel, MidiGroup};
use tutti_polysynth::{PolySynth, SynthConfig};

const SR: f64 = 48_000.0;

/// A synth playing a clip on `transport`: four notes in the first second
/// (beats ⅕ to 1⅗ at 120 BPM), each half a beat long.
fn synth(transport: &Transport) -> PolySynth {
    let mut s = PolySynth::new(SynthConfig {
        sample_rate: SampleRate(SR),
        max_voices: 8,
        ..Default::default()
    })
    .expect("a synth");
    let on = |n: u8| {
        MidiEvent::note_on(
            MidiGroup::FIRST,
            MidiChannel::new(0),
            n,
            midi1_velocity_to_midi2(100),
        )
    };
    let off = |n: u8| MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::new(0), n, 0);
    let mut events = Vec::new();
    for (i, note) in [60u8, 64, 67, 72].into_iter().enumerate() {
        let at = 0.2 + 0.4 * i as f64;
        events.push(TimedMidiEvent::new(Beat(at), on(note)));
        events.push(TimedMidiEvent::new(Beat(at + 0.5), off(note)));
    }
    s.set_midi_source(Arc::new(MidiClipSource::new(
        s.midi_port().unit_id(),
        events,
        Arc::new(transport.clone()) as Arc<dyn Timeline>,
    )));
    s
}

fn graph_engine(transport: &Transport, units: Vec<PolySynth>) -> (Engine, Editor) {
    let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(SR), Samples(1024)));
    let n = units.len();
    for (i, u) in units.into_iter().enumerate() {
        ed.insert(NodeKey(i as u64 + 1), "synth", Legacy::new(u));
    }
    ed.spec_mut().topology.outputs = (0..n)
        .map(|i| {
            Source::Node(OutPort {
                node: NodeKey(i as u64 + 1),
                port: 0,
            })
        })
        .collect();
    ed.commit().expect("commits");
    let engine = Engine::new(transport, &mut ed, exec).expect("within the limits");
    (engine, ed)
}

/// One second of `engine` in `block`-frame device blocks, each frame's two
/// channels summed: the clip, whichever synth played it.
fn play(engine: &Engine, transport: &Transport, block: usize) -> Vec<f32> {
    transport.motion.try_send(MotionEvent::Play).expect("room");
    let mut out = Vec::new();
    let mut buf = vec![0.0f32; block * 2];
    while out.len() < SR as usize {
        engine.process(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO));
        out.extend(buf.as_chunks::<2>().0.iter().map(|f| f[0] + f[1]));
    }
    out
}

fn first_difference(a: &[f32], b: &[f32]) -> Option<usize> {
    a.iter()
        .zip(b)
        .position(|(x, y)| x.to_bits() != y.to_bits())
}

/// At `block`-frame device blocks: two synths sharing the clip render, in
/// sum, one synth playing the clip alone, bit for bit: its notes are played
/// once, never refired.
fn shared_clip_plays_once(block: usize) {
    let graph_t = Transport::new(SR);
    let s = synth(&graph_t);
    let (graph, _ed) = graph_engine(&graph_t, vec![s.clone(), s]);
    let b = play(&graph, &graph_t, block);

    let alone_t = Transport::new(SR);
    // The same stereo root, the second synth with no clip at all: a mono
    // root would be folded onto both device channels and double the sum.
    let quiet = PolySynth::new(SynthConfig {
        sample_rate: SampleRate(SR),
        max_voices: 8,
        ..Default::default()
    })
    .expect("a synth");
    let (alone, _ed2) = graph_engine(&alone_t, vec![synth(&alone_t), quiet]);
    let c = play(&alone, &alone_t, block);

    let peak = b.iter().fold(0.0f32, |m, x| m.max(x.abs()));
    assert!(peak > 0.05, "{block}: the clip is silent ({peak})");
    if let Some(i) = first_difference(&c, &b) {
        panic!(
            "{block}-frame blocks: two synths sharing the clip part from one at frame {i}: \
             one {} two {}",
            c[i], b[i]
        );
    }
}

#[test]
fn a_shared_clip_plays_once_at_256_frame_blocks() {
    shared_clip_plays_once(256);
}

#[test]
fn a_shared_clip_plays_once_at_512_frame_blocks() {
    shared_clip_plays_once(512);
}
