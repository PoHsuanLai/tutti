//! **A sampler voice plays in time through the live graph engine.**
//!
//! A placed voice (a `MemorySource` on the live `Transport`, in a
//! `VoicePool`) derives its read position from the playhead, which it polls
//! as an `Arc<dyn Timeline>` on every `AudioUnit::process` call. Through the
//! native graph that call is `Legacy`'s, one per 64-frame chunk, while the
//! engine (`Engine::with_graph`) advances its clock once per graph block. So
//! the engine seats the playhead on each chunk's first frame while the block
//! renders (`engine.rs`, `Seats`; doc 013 §6). Without that every chunk of a
//! 256-frame device block reads one beat, and the voice replays a
//! 64-frame stretch four times a block.
//!
//! The oracle is the `Net` engine (`Engine::new`), whose `TransportClock`
//! node publishes the playhead between its 64-frame chunks. The clock is
//! pushed before the voice, as bevy-tutti's engine build pushes it before
//! any clip, and in that order the net's voice reads, in each chunk, the
//! beat on the chunk's first frame: the seat the graph engine gives it.
//! (Measured: pushed the other way round, the net runs the clock first and
//! its voice reads one chunk ahead, frame 64's sample on frame 0. The net
//! picks that order, not the host.) The two engines render the same device
//! blocks, and must agree to the bit; the dry voice must also be the tone it
//! plays, frame for frame, so an agreement on a wrong answer cannot pass.

use std::sync::Arc;

use tutti_core::dsp::Net;
use tutti_core::graph::{OutPort, Source};
use tutti_core::{
    AudioUnit, Beat, Cents, ChannelLayout, Engine, InterleavedMut, MotionEvent, NodeKey,
    SampleRate, Samples, Timeline, Transport, TransportClock,
};
use tutti_graph::{Editor, Legacy, Prepare};
use tutti_io::Wave;
use tutti_sampler::{MemorySource, Playback, SlotId, Voice, VoicePool, VoiceSource};

const SR: f64 = 48_000.0;

/// The tone's frame `i`: 440 Hz at `SR`.
fn tone_at(i: usize) -> f32 {
    (std::f32::consts::TAU * 440.0 * i as f32 / SR as f32).sin()
}

/// One placed voice at beat 0 on `transport`, pitched by `cents`, playing a
/// one-second table of the tone.
fn voice(transport: &Transport, cents: f32) -> VoicePool {
    let mut wave = Wave::new(1, SR);
    for i in 0..SR as usize {
        wave.push_frame(&[tone_at(i)]);
    }
    let source = MemorySource::with_transport(
        Arc::new(wave),
        Arc::new(transport.clone()) as Arc<dyn Timeline>,
        Beat(0.0),
        None,
    );
    let (mut pool, _handle) = VoicePool::new();
    pool.insert_voice(
        SlotId(1),
        Voice {
            source: VoiceSource::Memory(source),
            play: Playback {
                pitch: Cents::new(cents),
                ..Default::default()
            },
            channel_index: None,
        },
    );
    pool
}

/// A `Net` engine: the clock that moves `transport`, then the voice (see
/// the module docs for why that order).
fn net_engine(transport: &Transport, cents: f32) -> Engine {
    let mut net = Net::new(0, 2);
    net.push(Box::new(TransportClock::new(transport.clock_links(), SR)));
    let id = net.push(Box::new(voice(transport, cents)));
    net.pipe_output(id);
    net.set_sample_rate(SampleRate(SR));
    let backend = net.backend();
    // The backend is fed through the net; keep the frontend alive.
    Box::leak(Box::new(net));
    Engine::new(transport.motion.clone(), backend)
}

/// A graph engine, prepared for blocks of up to 1024 frames (the export's
/// `GRAPH_MAX_BLOCK`), with the voice as a `Legacy` node on both outputs.
fn graph_engine(transport: &Transport, cents: f32) -> (Engine, Editor) {
    let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(SR), Samples(1024)));
    let key = NodeKey(1);
    ed.insert(key, "voice", Legacy::new(voice(transport, cents)));
    ed.spec_mut().topology.outputs = (0..2)
        .map(|port| Source::Node(OutPort { node: key, port }))
        .collect();
    ed.commit().expect("commits");
    let engine = Engine::with_graph(transport, &mut ed, exec).expect("within the limits");
    (engine, ed)
}

/// Start `transport` and render `blocks` device blocks of `n` frames of
/// interleaved stereo through `engine`.
fn play(engine: &Engine, transport: &Transport, n: usize, blocks: usize) -> Vec<f32> {
    transport.motion.try_send(MotionEvent::Play).expect("room");
    let mut all = Vec::with_capacity(n * blocks * 2);
    let mut buf = vec![0.0f32; n * 2];
    for _ in 0..blocks {
        engine.process(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO));
        all.extend_from_slice(&buf);
    }
    all
}

/// At device blocks of `n` frames, dry and a fifth up, the graph engine
/// renders the `Net` engine's output bit for bit, the dry voice is the tone,
/// and both playheads end on the same bit.
fn plays_in_time(n: usize) {
    // Half a second, whatever the block.
    let blocks = 24_000 / n;
    for cents in [0.0f32, 700.0] {
        let net_t = Transport::new(SR);
        let net = net_engine(&net_t, cents);
        let a = play(&net, &net_t, n, blocks);

        let graph_t = Transport::new(SR);
        let (graph, _ed) = graph_engine(&graph_t, cents);
        let b = play(&graph, &graph_t, n, blocks);

        if let Some(i) = a
            .iter()
            .zip(&b)
            .position(|(x, y)| x.to_bits() != y.to_bits())
        {
            panic!(
                "{n}-frame blocks, {cents} cents: the graph parts from the Net at \
                 frame {} (channel {}): net {} graph {}",
                i / 2,
                i % 2,
                a[i],
                b[i]
            );
        }
        assert_eq!(
            net_t.beat().get().to_bits(),
            graph_t.beat().get().to_bits(),
            "{n}-frame blocks, {cents} cents: the playheads end apart"
        );
        let peak = b.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(
            peak > 0.5,
            "{n}-frame blocks, {cents} cents: near-silent ({peak})"
        );
        if cents == 0.0 {
            for (i, s) in b.as_chunks::<2>().0.iter().enumerate() {
                let want = tone_at(i);
                assert!(
                    (s[0] - want).abs() < 1e-3,
                    "{n}-frame blocks: frame {i} read {}, the tone is {want} there",
                    s[0]
                );
            }
        }
    }
}

// Mutations (run), each failing all three sizes:
// - the engine not seating (`Seats::seat` a no-op, so the voice reads the
//   playhead the walk left at the block's end) → parts from the `Net` at
//   frame 0;
// - `run_clock` recording each seat after advancing its chunk (every seat a
//   chunk late) → parts at frame 0;
// - `GraphRender::render` not putting the playhead back after the block
//   (it stands at the last chunk's seat) → the playheads end apart. The
//   audio still agrees: the engine's clock keeps its own position, and the
//   next block seats afresh; what the restore protects is every reader
//   between blocks (a UI's playhead, a control-thread `Timeline` read).

#[test]
fn a_voice_plays_in_time_at_256_frame_blocks() {
    plays_in_time(256);
}

#[test]
fn a_voice_plays_in_time_at_512_frame_blocks() {
    plays_in_time(512);
}

/// The export's block (`GRAPH_MAX_BLOCK`), live.
#[test]
fn a_voice_plays_in_time_at_1024_frame_blocks() {
    plays_in_time(1024);
}
