//! The sample-accuracy contract (doc 013 §6, "Proof") at the engine: a
//! graph rendered through `Engine::with_graph`, a transport started by a
//! timestamped command (`MotionFsm::schedule(At::Frame)`), and notes
//! scheduled at `At::Beat` into two impulse nodes — one direct, one behind
//! PDC. Each note must land on its beat's frame at its node, and the two
//! paths must leave the engine on the **same** frame: the direct one is
//! delayed at the output to meet the latent one.
//!
//! The node rows, and the graph-level paths, are in `tutti-graph`'s
//! `tests/contract.rs` (and the node crates'); this is the end-to-end case
//! where the transport, the command queue and PDC all meet.

use tutti_core::{
    At, Beat, ChannelLayout, Engine, Frame, InterleavedMut, MotionEvent, SampleRate, Samples,
    Transport,
};
use tutti_graph::contract::{Latent, Pulse};
use tutti_graph::{EventIn, EventKind, GraphBuilder, Prepare, Ump};
use tutti_types::{Latency, NodeKey};

const SR: f64 = 48_000.0;
/// Frames per beat at the transport's default 120 BPM.
const FPB: u64 = 24_000;
/// The latent sibling's declared latency: longer than a device block.
const D: usize = 300;
const NOTE_ON: EventKind = EventKind::Midi(Ump([0x2090_3c64, 0, 0, 0]));

/// An engine over a graph of two `Pulse`s — `(direct, pdc)` — whose global
/// outputs are `[pdc, direct]`, the second pulse's event input merged with a
/// silent sibling declaring `D` frames of latency.
fn engine(
    transport: &Transport,
    max_block: usize,
) -> (Engine, tutti_graph::Editor, NodeKey, NodeKey) {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let direct = g.add(Pulse::new(Latency::ZERO));
    let pdc = g.add(Pulse::new(Latency::ZERO));
    let sibling = g.add(Latent::new(Latency::new(Samples(D))));
    g.event_connect(sibling, 0, pdc, 0);
    g.connect_output(pdc, 0, 0).connect_output(direct, 0, 1);
    let (mut ed, exec) = g
        .build(Prepare::new(SampleRate(SR), Samples(max_block)))
        .expect("builds");
    let plan = exec.plan().expect("installed");
    assert_eq!(plan.unit(pdc).unwrap().arrival, Latency::new(Samples(D)));
    assert_eq!(plan.unit(direct).unwrap().arrival, Latency::ZERO);
    let engine = Engine::with_graph(transport, &mut ed, exec).expect("within the limits");
    (engine, ed, direct, pdc)
}

/// Render `blocks` (device block lengths) and return the frames at which
/// each of the two output channels is non-zero.
fn onsets(engine: &Engine, blocks: &[usize]) -> [Vec<u64>; 2] {
    let mut hits = [Vec::new(), Vec::new()];
    let mut done = 0u64;
    for &n in blocks {
        let mut buf = vec![0.0f32; n * 2];
        engine.process(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO));
        for (i, frame) in buf.chunks(2).enumerate() {
            for c in 0..2 {
                if frame[c] != 0.0 {
                    assert_eq!(frame[c], 1.0, "a pulse is one sample of 1.0");
                    hits[c].push(done + i as u64);
                }
            }
        }
        done += n as u64;
    }
    hits
}

/// A timed start mid-block, and notes at beat 0 (the start's own frame, in
/// the start's block), beat 1/1000 (24 frames on, still in it) and beat 1/2:
/// both paths sound every note at `start + beat + D`, on one frame, under a
/// ragged device schedule.
///
/// Mutation (run): in the engine, hand the executor
/// `TransportChanges::NONE` → the start is heard at the next block's first
/// frame, the two in-block notes land late (at that frame) → fails. In
/// `CommandRx::gather`, resolve every command at arrival zero → the PDC
/// path's notes leave `D` early, ahead of the direct path's → fails. Land
/// every due command at offset 0 of its block → fails. In `compile`, give
/// every global output channel zero alignment → the direct path leaves `D`
/// early at the output → fails.
#[test]
fn beat_notes_after_a_timed_start_leave_both_paths_on_one_frame() {
    // A start at several offsets into a 256-frame block, including its first
    // and last frames; ragged blocks after the first few.
    for start in [1_024u64, 1_025, 1_100, 1_279] {
        let transport = Transport::new(SR);
        let (engine, mut ed, direct, pdc) = engine(&transport, 256);
        let beats = [0.0, 0.001, 0.5];
        for node in [direct, pdc] {
            for b in beats {
                ed.schedule(At::Beat(Beat(b)), EventIn { node, port: 0 }, NOTE_ON)
                    .expect("room");
            }
        }
        transport
            .motion
            .schedule(At::Frame(Frame(start)), MotionEvent::Play)
            .expect("room");
        let mut blocks = vec![256usize; 4];
        blocks.extend([1, 63, 64, 65, 200, 7].iter().cycle().take(300));
        let expected: Vec<u64> = beats
            .iter()
            .map(|b| start + (b * FPB as f64).round() as u64 + D as u64)
            .collect();
        let [pdc_out, direct_out] = onsets(&engine, &blocks);
        assert_eq!(pdc_out, expected, "start {start}: the PDC'd path");
        assert_eq!(direct_out, expected, "start {start}: the direct path");
        assert_eq!(transport.motion.scheduled_outstanding(), 0);
        ed.collect();
        assert_eq!(ed.commands_outstanding(), 0, "every note landed");
    }
}
