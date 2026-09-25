//! **A sampler voice plays in time through the live graph engine.**
//!
//! A placed voice (a `MemorySource` on the live `Transport`, in a
//! `VoicePool`) derives its read position from the playhead, which it polls
//! as an `Arc<dyn Timeline>` on every `AudioUnit::process` call. Through the
//! native graph that call is `Legacy`'s, one per 64-frame chunk. So while the
//! plan holds a `Legacy` unit the engine (`Engine::with_graph`) renders
//! chunk-major: graph blocks of at most 64 frames across every node, the
//! playhead published after each (doc 013, "chunk-major `Legacy`
//! compatibility mode"). Rendered in whole device blocks instead, every chunk
//! of a 256-frame block reads one beat, and the voice replays a 64-frame
//! stretch four times a block.
//!
//! The oracle is the `Net` engine (`Engine::new`), whose `TransportClock`
//! node publishes the playhead after each of its 64-frame chunks. The clock
//! is pushed before the voices, as bevy-tutti's engine build pushes it before
//! any clip; in that order the net runs the voices first in each chunk, and
//! they read the beat on the chunk's first frame. (Measured: pushed the other
//! way round, the net runs the clock first and its voice reads one chunk
//! ahead, frame 64's sample on frame 0. The net picks that order, not the
//! host.) The two engines render the same device blocks and must agree to
//! the bit wherever `Net`'s chunk grid and the graph's coincide; a dry voice
//! must also be the tone it plays, frame for frame, so an agreement on a
//! wrong answer cannot pass.
//!
//! Mutations (run), each failing every test here:
//! - `GraphRender::settle` ignoring `has_legacy`, so the engine renders
//!   whole device blocks with `Legacy` units present → the graph parts from
//!   the `Net` (and from the tone) at frame 64;
//! - the engine publishing its playhead in the walk, before the render
//!   (`TransportClock::advance` writing back) → every chunk reads its own
//!   end: the graph runs a chunk ahead of the `Net` from frame 0.

use std::sync::Arc;

use tutti_core::dsp::Net;
use tutti_core::graph::{OutPort, Source};
use tutti_core::{
    At, AudioUnit, Beat, Cents, ChannelLayout, Engine, FadeOut, Frame, InterleavedMut, MotionEvent,
    NodeKey, SampleRate, Samples, Then, Timeline, Transport, TransportClock,
};
use tutti_graph::{CrossfadeCurve, Editor, Fade, Legacy, Prepare};
use tutti_io::Wave;
use tutti_sampler::{MemorySource, Playback, SlotId, Voice, VoicePool, VoiceSource};

const SR: f64 = 48_000.0;
/// Frames per beat at 120 BPM and `SR`.
const FPB: usize = 24_000;

/// The tone's frame `i`: 440 Hz at `SR`.
fn tone_at(i: usize) -> f32 {
    (std::f32::consts::TAU * 440.0 * i as f32 / SR as f32).sin()
}

/// A pool on `transport` (so it keeps a `BeatCursor`, as a timeline pool
/// does) with one voice placed at beat 0, pitched by `cents`, playing four
/// seconds of the tone.
fn pool(transport: &Transport, cents: f32) -> VoicePool {
    let mut wave = Wave::new(1, SR);
    for i in 0..4 * SR as usize {
        wave.push_frame(&[tone_at(i)]);
    }
    let timeline = Arc::new(transport.clone()) as Arc<dyn Timeline>;
    let source =
        MemorySource::with_transport(Arc::new(wave), Arc::clone(&timeline), Beat(0.0), None);
    let (mut pool, _handle) = VoicePool::with_transport(timeline, None);
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

/// Where each unit's output goes: one unit plays on both device channels
/// (its two ports); two units play one each (their port 0).
fn routes(units: usize) -> Vec<(usize, usize)> {
    match units {
        1 => vec![(0, 0), (0, 1)],
        2 => vec![(0, 0), (1, 0)],
        n => panic!("{n} units"),
    }
}

/// A `Net` engine over `transport`: the clock that moves it, then `units`
/// (see the module docs for why that order), routed by [`routes`].
fn net_engine(transport: &Transport, units: Vec<VoicePool>) -> Engine {
    let mut net = Net::new(0, 2);
    net.push(Box::new(TransportClock::new(transport.clock_links(), SR)));
    let ids: Vec<_> = units.into_iter().map(|u| net.push(Box::new(u))).collect();
    for (out, (unit, port)) in routes(ids.len()).into_iter().enumerate() {
        net.connect_output(ids[unit], port, out);
    }
    net.set_sample_rate(SampleRate(SR));
    let backend = net.backend();
    // The backend is fed through the net; keep the frontend alive.
    Box::leak(Box::new(net));
    Engine::new(transport.motion.clone(), backend)
}

/// A graph engine over `transport`, prepared for blocks of up to 1024 frames
/// (the export's `GRAPH_MAX_BLOCK`), with `units` as `Legacy` nodes at keys
/// 1, 2, … routed by [`routes`].
fn graph_engine(transport: &Transport, units: Vec<VoicePool>) -> (Engine, Editor) {
    let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(SR), Samples(1024)));
    let n = units.len();
    for (i, u) in units.into_iter().enumerate() {
        ed.insert(NodeKey(i as u64 + 1), "voice", Legacy::new(u));
    }
    ed.spec_mut().topology.outputs = routes(n)
        .into_iter()
        .map(|(unit, port)| {
            Source::Node(OutPort {
                node: NodeKey(unit as u64 + 1),
                port: port as u16,
            })
        })
        .collect();
    ed.commit().expect("commits");
    let engine = Engine::with_graph(transport, &mut ed, exec).expect("within the limits");
    (engine, ed)
}

/// Start `transport` and render `blocks` device blocks of `n` frames of
/// interleaved stereo through `engine`, calling `between(i)` before block
/// `i`.
fn play(
    engine: &Engine,
    transport: &Transport,
    n: usize,
    blocks: usize,
    mut between: impl FnMut(usize),
) -> Vec<f32> {
    transport.motion.try_send(MotionEvent::Play).expect("room");
    let mut all = Vec::with_capacity(n * blocks * 2);
    let mut buf = vec![0.0f32; n * 2];
    for i in 0..blocks {
        between(i);
        engine.process(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO));
        all.extend_from_slice(&buf);
    }
    all
}

/// The first interleaved index in `range` (frames) where `a` and `b` are
/// not bit-equal.
fn parts(a: &[f32], b: &[f32], frames: std::ops::Range<usize>) -> Option<usize> {
    (frames.start * 2..frames.end * 2).find(|&i| a[i].to_bits() != b[i].to_bits())
}

/// Panic with where two renders part, if they do in `frames`.
fn assert_same(what: &str, net: &[f32], graph: &[f32], frames: std::ops::Range<usize>) {
    if let Some(i) = parts(net, graph, frames) {
        panic!(
            "{what}: the graph parts from the Net at frame {} (channel {}): net {} graph {}",
            i / 2,
            i % 2,
            net[i],
            graph[i]
        );
    }
}

/// Left channel of `out`, frame `f`, against the tone at source frame `src`.
fn assert_tone(what: &str, out: &[f32], f: usize, src: usize) {
    let (got, want) = (out[2 * f], tone_at(src));
    assert!(
        (got - want).abs() < 1e-3,
        "{what}: frame {f} read {got}, the tone at {src} is {want}"
    );
}

fn assert_sounds(what: &str, out: &[f32]) {
    let peak = out.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    assert!(peak > 0.5, "{what}: near-silent ({peak})");
}

/// At device blocks of `n` frames, dry and a fifth up, the graph engine
/// renders the `Net` engine's output bit for bit, the dry voice is the tone,
/// and both playheads end on the same bit.
fn plays_in_time(n: usize) {
    // Half a second, whatever the block.
    let blocks = 24_000 / n;
    for cents in [0.0f32, 700.0] {
        let what = format!("{n}-frame blocks, {cents} cents");
        let net_t = Transport::new(SR);
        let net = net_engine(&net_t, vec![pool(&net_t, cents)]);
        let a = play(&net, &net_t, n, blocks, |_| {});

        let graph_t = Transport::new(SR);
        let (graph, _ed) = graph_engine(&graph_t, vec![pool(&graph_t, cents)]);
        let b = play(&graph, &graph_t, n, blocks, |_| {});

        assert_same(&what, &a, &b, 0..a.len() / 2);
        assert_eq!(
            net_t.beat().get().to_bits(),
            graph_t.beat().get().to_bits(),
            "{what}: the playheads end apart"
        );
        assert_sounds(&what, &b);
        if cents == 0.0 {
            for f in 0..b.len() / 2 {
                assert_tone(&what, &b, f, f);
            }
        }
    }
}

#[test]
fn a_voice_plays_in_time_at_256_frame_blocks() {
    plays_in_time(256);
}

/// Not a multiple of 64: both engines chunk each device block from its own
/// start, 7 × 64 + 32, so their grids still coincide.
#[test]
fn a_voice_plays_in_time_at_480_frame_blocks() {
    plays_in_time(480);
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

/// Past the graph's prepared `MaxBlock` (1024): the engine splits the
/// device block into graph blocks, and chunks those.
#[test]
fn a_voice_plays_in_time_at_2048_frame_blocks() {
    plays_in_time(2048);
}

/// **Two `Legacy` nodes sharing one `BeatCursor`** (two clones of one pool)
/// render the `Net`'s output bit for bit, and the tone. Each clone polls the
/// cursor in every chunk; chunk-major, both read the chunk's beat, and
/// neither sees the other's poll as a jump. Had each node walked its own
/// block while the cursor stood where the other left it (a node-major
/// render), the second would see a rewind every block and flush its voices
/// (`VoicePool::flush_on_seek`).
///
/// Dry, not pitched: a pitched voice's vocoder bank is shared by a plain
/// clone too, and two clones ticking it trip the bank's guard (`isolate`
/// would fix that, and would also drop the shared cursor under test). The
/// discontinuity itself is pinned directly, with a cursor probe, in
/// tutti-core's `legacy_chunk_major.rs`.
#[test]
fn two_voices_sharing_a_cursor_play_as_through_the_net() {
    let make = |t: &Transport| {
        let p = pool(t, 0.0);
        vec![p.clone(), p]
    };
    let net_t = Transport::new(SR);
    let net = net_engine(&net_t, make(&net_t));
    let a = play(&net, &net_t, 512, 60, |_| {});
    let graph_t = Transport::new(SR);
    let (graph, _ed) = graph_engine(&graph_t, make(&graph_t));
    let b = play(&graph, &graph_t, 512, 60, |_| {});
    assert_sounds("shared cursor", &b);
    assert_same("shared cursor", &a, &b, 0..a.len() / 2);
    for f in 0..b.len() / 2 {
        assert_tone("shared cursor", &b, f, f);
        let right = b[2 * f + 1];
        assert!(
            (right - tone_at(f)).abs() < 1e-3,
            "shared cursor, the second clone: frame {f} read {right}"
        );
    }
}

/// **A crossfade of a voice** (the pool replaced by a clone of itself,
/// sharing its cursor, over 4096 frames) renders what the voice renders
/// uncrossfaded, to rounding (the two gains sum to one), and the tone
/// throughout: both halves read the timeline chunk by chunk during the fade.
#[test]
fn a_crossfaded_voice_plays_on_through_the_fade() {
    let blocks = 60;
    let render = |fade: bool| {
        let t = Transport::new(SR);
        let p = pool(&t, 0.0);
        let spare = p.clone();
        let (engine, mut ed) = graph_engine(&t, vec![p]);
        let mut spare = Some(spare);
        play(&engine, &t, 512, blocks, |i| {
            if fade && i == 20 {
                ed.replace(
                    NodeKey(1),
                    Legacy::new(spare.take().expect("once")),
                    Fade::new(Samples(4096), CrossfadeCurve::EqualAmplitude),
                )
                .expect("same shape");
                ed.commit().expect("commits");
            }
            ed.collect();
        })
    };
    let plain = render(false);
    let faded = render(true);
    assert_sounds("crossfade", &faded);
    for f in 0..faded.len() / 2 {
        assert!(
            (faded[2 * f] - plain[2 * f]).abs() < 1e-6,
            "frame {f}: crossfaded {} uncrossfaded {}",
            faded[2 * f],
            plain[2 * f]
        );
        assert_tone("crossfade", &faded, f, f);
    }
}

/// **A loop wrap** (beats 1–1.5, reached mid-chunk at frame 36 000) renders
/// the `Net`'s output bit for bit: the wrap is the clock's, not a cut, and
/// both engines read the wrapped beat from the next chunk on. The chunk the
/// wrap falls in plays on past the loop end on both, from its first frame's
/// beat, as a `Legacy` clip reader always has; after it, the dry voice is
/// the tone a loop length back.
#[test]
fn a_loop_wrap_plays_as_through_the_net() {
    let start = |t: &Transport| {
        t.settings.loop_span.set_range(1.0, 1.5);
        t.settings.loop_span.set_enabled(true);
    };
    let blocks = 60_000 / 512;
    let net_t = Transport::new(SR);
    start(&net_t);
    let net = net_engine(&net_t, vec![pool(&net_t, 0.0)]);
    let a = play(&net, &net_t, 512, blocks, |_| {});
    let graph_t = Transport::new(SR);
    start(&graph_t);
    let (graph, _ed) = graph_engine(&graph_t, vec![pool(&graph_t, 0.0)]);
    let b = play(&graph, &graph_t, 512, blocks, |_| {});

    assert_same("loop", &a, &b, 0..a.len() / 2);
    let end = 3 * FPB / 2;
    let wrap_chunk = end / 64 * 64;
    for f in 0..wrap_chunk + 64 {
        assert_tone("loop, before the wrap", &b, f, f);
    }
    for f in wrap_chunk + 64..b.len() / 2 {
        assert_tone("loop, after the wrap", &b, f, f - FPB / 2);
    }
}

/// **A locate scheduled inside a block** (`At::Frame(10 037)`, to beat ¼,
/// immediate) against the `Net`: bit for bit up to the chunk the locate
/// falls in, and within a hair after it; the difference is that chunk.
///
/// A `Legacy` clip reader reads the playhead once per 64-frame call. `Net`
/// renders pieces split at the locate's frame, so its voice starts the
/// target on that frame. The graph engine cuts the transport there (in
/// `Env`, for native nodes), but a `Legacy` unit's call is the chunk: it
/// reads the target at the chunk's first frame, 53 frames early. From the
/// next chunk both play the target at its frame. After the locate the two
/// grids differ (`Net` chunks from the locate's frame), so they read the
/// beat at different frames and differ in the low bits, not in what they
/// play.
#[test]
fn a_scheduled_locate_lands_on_its_chunk() {
    let at = 10_037;
    let target = FPB / 4;
    let locate = |t: &Transport| {
        t.motion
            .schedule(
                At::Frame(Frame(at as u64)),
                MotionEvent::Locate {
                    beat: Beat(0.25),
                    fade: FadeOut::Immediate,
                    then: Then::Keep,
                },
            )
            .expect("room");
    };
    let blocks = 24_000 / 512;
    let net_t = Transport::new(SR);
    let net = net_engine(&net_t, vec![pool(&net_t, 0.0)]);
    locate(&net_t);
    let a = play(&net, &net_t, 512, blocks, |_| {});
    let graph_t = Transport::new(SR);
    let (graph, _ed) = graph_engine(&graph_t, vec![pool(&graph_t, 0.0)]);
    locate(&graph_t);
    let b = play(&graph, &graph_t, 512, blocks, |_| {});

    let chunk = at / 64 * 64;
    assert_same("locate, before its chunk", &a, &b, 0..chunk);
    for f in chunk..chunk + 64 {
        assert_tone("locate, the graph's chunk", &b, f, target + (f - chunk));
    }
    for f in chunk..at {
        assert_tone("locate, the Net before it", &a, f, f);
    }
    for f in at..b.len() / 2 {
        assert_tone("locate, the Net after it", &a, f, target + (f - at));
    }
    for f in chunk + 64..b.len() / 2 {
        assert_tone("locate, the graph after it", &b, f, target + (f - at));
        assert!(
            (a[2 * f] - b[2 * f]).abs() < 1e-4,
            "frame {f}: net {} graph {}",
            a[2 * f],
            b[2 * f]
        );
    }
}
