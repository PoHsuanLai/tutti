//! The synth as a native node, fed by a clip node's event port (doc 013,
//! rewrite item 5): a clip's note sounds on its frame, in the block it is
//! written, and a forked graph plays it offline with nothing rebound.
//!
//! 120 BPM at 48 kHz: a beat is 24 000 frames.

use std::sync::Arc;

use tutti_core::graph::{OutPort, Source};
use tutti_core::transport::{OfflineTimeline, OfflineTimelineConfig, OfflineTransport};
use tutti_core::{Beat, Bpm, NodeKey, SampleRate, Samples, Seconds};
use tutti_graph::{
    Editor, EventEdge, EventIn, EventOut, Executor, ForkMode, ForkTarget, Prepare, Renderer,
    Transport,
};
use tutti_midi_runtime::{MidiClipNode, TimedMidiEvent};
use tutti_midi_types::{MidiChannel, MidiEvent, MidiGroup};
use tutti_polysynth::{EnvelopeConfig, OscillatorType, PolySynth, SynthConfig};

const RATE: f64 = 48_000.0;
const BLOCK: usize = 256;

fn saw() -> PolySynth {
    PolySynth::new(SynthConfig {
        sample_rate: SampleRate(RATE),
        oscillator: OscillatorType::Saw,
        envelope: EnvelopeConfig {
            attack: Seconds(0.0),
            ..Default::default()
        },
        ..Default::default()
    })
    .expect("synth builds")
}

fn note_on_at(frame: u64) -> TimedMidiEvent {
    TimedMidiEvent::new(
        Beat(frame as f64 / 24_000.0),
        MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0xFFFF),
    )
}

/// A clip (key 1) playing a note at `frame` into a native synth (key 2) on
/// the global outputs, committed.
fn graph(frame: u64) -> (Editor, Executor) {
    let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(RATE), Samples(BLOCK)));
    ed.insert(NodeKey(1), "clip", MidiClipNode::new([note_on_at(frame)]));
    ed.insert(NodeKey(2), "synth", saw());
    ed.spec_mut().connect_events(
        EventIn {
            node: NodeKey(2),
            port: 0,
        },
        EventEdge::Direct(EventOut {
            node: NodeKey(1),
            port: 0,
        }),
    );
    ed.spec_mut().topology.outputs = (0..2)
        .map(|port| {
            Source::Node(OutPort {
                node: NodeKey(2),
                port,
            })
        })
        .collect();
    ed.commit().expect("commits");
    (ed, exec)
}

/// Render `frames` of the left channel, rolling at 120 BPM from beat 0.
fn render(ed: Editor, exec: Executor, frames: usize) -> Vec<f32> {
    let mut r = Renderer::new(ed, exec);
    r.set_transport_fn(|f| Transport::new(true, Bpm(120.0), Beat(f.get() as f64 / 24_000.0), None));
    r.render(frames).swap_remove(0)
}

fn onset(out: &[f32]) -> Option<usize> {
    out.iter().position(|&s| s != 0.0)
}

/// Where the synth's first non-zero sample falls after a note-on at frame 0
/// (a saw starts from 0, so a frame in).
fn lead() -> usize {
    let (ed, exec) = graph(0);
    onset(&render(ed, exec, BLOCK)).expect("a note at frame 0 sounds in its block")
}

/// **A clip's note sounds on its frame, in the block the clip writes it.**
/// The note is at frame 1 000, inside the fourth 256-frame block; the
/// synth's first sample is exactly that far after its first sample for a
/// note at frame 0. A block late (the mailbox path's delivery) or at the
/// block's start both miss.
///
/// Mutation: apply the event input's events at offset 0 (`gather_events`
/// ignoring the offset) → the onset moves to 768 → fails. Mutation: read no
/// event input (only the port) → silence → fails.
#[test]
fn a_clip_note_sounds_on_its_frame_in_the_same_block() {
    let (ed, exec) = graph(1_000);
    let out = render(ed, exec, 2_048);
    assert_eq!(onset(&out), Some(1_000 + lead()));
}

/// **An exported graph plays the clip, on the render's timeline, with
/// nothing rebound.** The live graph is forked offline and rendered; the
/// fork's clip node reads its render's `Env`, so the note is on the same
/// frame as live.
///
/// Mutation: a clip fork with no events → the fork is silent → fails.
/// Mutation: fork the synth through `Legacy` (`SynthFork::native` false) →
/// it has no event input, the edge is refused at the fork's compile → fails.
#[test]
fn a_forked_graph_plays_the_clip() {
    let (live, _exec) = graph(1_000);
    let offline = OfflineTransport::new(Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        sample_rate: SampleRate(RATE),
        ..Default::default()
    })));
    let prepare = *live.prepare();
    let (fork, fork_exec) = live
        .fork(ForkTarget::Master, ForkMode::Offline(&offline), prepare)
        .expect("a clip and a native synth fork");
    let out = render(fork, fork_exec, 2_048);
    assert_eq!(onset(&out), Some(1_000 + lead()));
}
