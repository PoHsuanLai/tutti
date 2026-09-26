//! The unit as a graph node (doc 013, rewrite item 5): a clip node's note
//! sounds on its frame (to rustysynth's 8-frame chunk), the node follows its
//! graph's rate, and a forked graph plays the clip.
//!
//! Against the committed `TimGM6mb.sf2`; a missing fixture fails, as in the
//! other suites here. 120 BPM: a beat is half a second.

use std::path::PathBuf;
use std::sync::Arc;

use tutti_core::transport::{OfflineTimeline, OfflineTimelineConfig, OfflineTransport};
use tutti_core::{Beat, Bpm, NodeKey, SampleRate, Samples};
use tutti_graph::{
    Editor, EventEdge, EventIn, EventOut, Executor, ForkMode, ForkTarget, Prepare, Renderer,
    Transport,
};
use tutti_midi_runtime::{MidiClipNode, TimedMidiEvent};
use tutti_midi_types::{MidiChannel, MidiEvent, MidiGroup};
use tutti_soundfont::{SoundFont, SoundFontUnit, SynthesizerSettings};

const BLOCK: usize = 256;

fn font() -> Arc<SoundFont> {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../assets/soundfonts/TimGM6mb.sf2");
    let mut file = std::fs::File::open(&path)
        .unwrap_or_else(|e| panic!("test soundfont missing at {}: {e}", path.display()));
    Arc::new(SoundFont::new(&mut file).expect("parses"))
}

fn unit(rate: i32) -> SoundFontUnit {
    SoundFontUnit::new(font(), &SynthesizerSettings::new(rate)).expect("builds")
}

/// A clip playing middle C at `frame` into the unit, both at key 1 and 2,
/// the unit on the outputs; prepared at `rate`.
fn graph(unit: SoundFontUnit, rate: f64, frame: u64) -> (Editor, Executor) {
    let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(rate), Samples(BLOCK)));
    let beat = Beat(frame as f64 / (rate / 2.0));
    let on = MidiEvent::note_on_7bit(MidiGroup::FIRST, MidiChannel::FIRST, 60, 100);
    ed.insert(
        NodeKey(1),
        "clip",
        MidiClipNode::new([TimedMidiEvent::new(beat, on)]),
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
    ed.spec_mut().topology.outputs = (0..2)
        .map(|port| {
            tutti_core::graph::Source::Node(tutti_core::graph::OutPort {
                node: NodeKey(2),
                port,
            })
        })
        .collect();
    ed.commit().expect("commits");
    (ed, exec)
}

fn render(ed: Editor, exec: Executor, rate: f64, frames: usize) -> Vec<f32> {
    let mut r = Renderer::new(ed, exec);
    r.set_transport_fn(move |f| {
        Transport::new(true, Bpm(120.0), Beat(f.get() as f64 / (rate / 2.0)), None)
    });
    r.render(frames).swap_remove(0)
}

fn onset(out: &[f32]) -> Option<usize> {
    out.iter().position(|&s| s != 0.0)
}

/// **A clip's note sounds on its frame.** At frame 1 000 (a multiple of the
/// 8-frame chunk) it sounds exactly 1 000 frames after the same note at
/// frame 0 does.
///
/// Mutation: `gather` dropping the event input → silence → fails.
/// Mutation: events applied at the block start (offsets ignored) → 1 000
/// becomes 768 → fails.
#[test]
fn a_clip_note_sounds_on_its_frame() {
    let lead = {
        let (ed, exec) = graph(unit(48_000), 48_000.0, 0);
        onset(&render(ed, exec, 48_000.0, BLOCK)).expect("a note at frame 0 sounds")
    };
    let (ed, exec) = graph(unit(48_000), 48_000.0, 1_000);
    assert_eq!(
        onset(&render(ed, exec, 48_000.0, 4_096)),
        Some(1_000 + lead)
    );
}

/// **The node follows its graph's rate.** A unit built at 44.1 kHz in a
/// graph prepared at 48 kHz renders what a unit built at 48 kHz does.
///
/// Mutation: skip the rebuild in `prepare` → it renders at 44.1 kHz, every
/// sample differs → fails.
#[test]
fn the_node_follows_its_graphs_rate() {
    let (ed, exec) = graph(unit(44_100), 48_000.0, 0);
    let rerated = render(ed, exec, 48_000.0, 4_096);
    let (ed, exec) = graph(unit(48_000), 48_000.0, 0);
    let native = render(ed, exec, 48_000.0, 4_096);
    assert!(onset(&native).is_some(), "the note sounds");
    assert_eq!(rerated, native);
}

/// **A forked graph plays the clip**, on its render's `Env`, from frame
/// 1 000 as live.
///
/// Mutation: the fork source's `native` false → a `Legacy` fork with no event
/// input; the fork's compile refuses the edge → fails.
#[test]
fn a_forked_graph_plays_the_clip() {
    let lead = {
        let (ed, exec) = graph(unit(48_000), 48_000.0, 0);
        onset(&render(ed, exec, 48_000.0, BLOCK)).expect("a note at frame 0 sounds")
    };
    let (live, _exec) = graph(unit(48_000), 48_000.0, 1_000);
    let offline = OfflineTransport::new(Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        sample_rate: SampleRate(48_000.0),
        ..Default::default()
    })));
    let prepare = *live.prepare();
    let (fork, fork_exec) = live
        .fork(ForkTarget::Master, ForkMode::Offline(&offline), prepare)
        .expect("a clip and a soundfont node fork");
    assert_eq!(
        onset(&render(fork, fork_exec, 48_000.0, 4_096)),
        Some(1_000 + lead)
    );
}
