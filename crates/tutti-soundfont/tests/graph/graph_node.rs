//! The unit as a graph node: a clip node's note
//! sounds on its frame (to the voices' 8-frame chunk), the node follows its
//! graph's rate, and a forked graph plays the clip.
//!
//! Against the committed `TimGM6mb.sf2`; a missing fixture fails, as in the
//! other suites here. 120 BPM: a beat is half a second.

use std::sync::Arc;
use tutti_types::MaxBlock;

use tutti_fixtures::midi;
use tutti_graph::NodeKey;
use tutti_graph::{
    Editor, EventEdge, EventIn, EventOut, Executor, ForkMode, ForkTarget, Prepare, Renderer,
    TransportPosition,
};
use tutti_midi_protocol::widen_velocity;
use tutti_midi_runtime::{MidiClipNode, TimedMidiEvent};
use tutti_sf2::SoundFont;
use tutti_soundfont::{Settings, SoundFontUnit};
use tutti_types::{Beat, Bpm, SampleRate, Samples};
use tutti_ump::{MidiChannel, U7};

const BLOCK: usize = 256;

fn font() -> Arc<SoundFont> {
    let path = tutti_fixtures::asset("soundfonts/TimGM6mb.sf2");
    let mut file = std::fs::File::open(&path)
        .unwrap_or_else(|e| panic!("test soundfont missing at {}: {e}", path.display()));
    Arc::new(SoundFont::from_reader(&mut file).expect("parses"))
}

fn unit() -> SoundFontUnit {
    SoundFontUnit::new(font(), Settings::new()).expect("builds")
}

/// A clip playing middle C at `frame` into the unit, both at key 1 and 2,
/// the unit on the outputs; prepared at `rate`.
fn graph(unit: SoundFontUnit, rate: f64, frame: u64) -> (Editor, Executor) {
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate::of(rate),
        MaxBlock::of(Samples(BLOCK)),
    ));
    let beat = Beat(frame as f64 / (rate / 2.0));
    let velocity = widen_velocity(U7::of(100)).get(); // 7-bit 100, widened (M2-115)
    let on = midi::note_on(MidiChannel::FIRST, 60, velocity);
    ed.insert(
        NodeKey(1),
        "clip",
        MidiClipNode::new([TimedMidiEvent::new(beat, on).unwrap()]),
    );
    ed.insert(NodeKey(2), "font", unit);
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
    ed.spec_mut().topology.set_outputs((0..2).map(|port| {
        tutti_graph::Source::Node(tutti_graph::OutPort {
            node: NodeKey(2),
            port,
        })
    }));
    ed.commit().expect("commits");
    (ed, exec)
}

fn render(ed: Editor, exec: Executor, rate: f64, frames: usize) -> Vec<f32> {
    let mut r = Renderer::new(ed, exec);
    r.set_transport_fn(move |f| {
        TransportPosition::new(
            true,
            Bpm::of(120.0),
            Beat(f.get() as f64 / (rate / 2.0)),
            None,
        )
    });
    r.render(frames).swap_remove(0)
}

fn onset(out: &[f32]) -> Option<usize> {
    out.iter().position(|&s| s != 0.0)
}

/// **A clip's note sounds on its frame.** At frame 1 000 (a multiple of the
/// 8-frame chunk) it sounds exactly 1 000 frames after the same note at
/// frame 0 does.
#[test]
fn a_clip_note_sounds_on_its_frame() {
    let lead = {
        let (ed, exec) = graph(unit(), 48_000.0, 0);
        onset(&render(ed, exec, 48_000.0, BLOCK)).expect("a note at frame 0 sounds")
    };
    let (ed, exec) = graph(unit(), 48_000.0, 1_000);
    assert_eq!(
        onset(&render(ed, exec, 48_000.0, 4_096)),
        Some(1_000 + lead)
    );
}

/// **A forked graph plays the clip**, on its render's `Env`, from frame
/// 1 000 as live.
#[test]
fn a_forked_graph_plays_the_clip() {
    let lead = {
        let (ed, exec) = graph(unit(), 48_000.0, 0);
        onset(&render(ed, exec, 48_000.0, BLOCK)).expect("a note at frame 0 sounds")
    };
    let (live, _exec) = graph(unit(), 48_000.0, 1_000);
    let prepare = *live.prepare();
    let (fork, fork_exec) = live
        .fork(ForkTarget::Master, ForkMode::Offline, prepare)
        .expect("a clip and a soundfont node fork");
    assert_eq!(
        onset(&render(fork, fork_exec, 48_000.0, 4_096)),
        Some(1_000 + lead)
    );
}
