//! **A sampler voice plays in time through the live graph engine.**
//!
//! A placed voice (a `MemorySource` on the live `Transport`, in a
//! `VoicePool`) derives its read position from the playhead, which it polls
//! as an `Arc<dyn Timeline>` on every `AudioUnit::process` call. Through the
//! native graph that call is `Legacy`'s, one per 64-frame chunk. So while the
//! plan holds a `Legacy` unit the engine (`Engine::new`) renders
//! chunk-major: graph blocks of at most 64 frames across every node, the
//! playhead published after each (doc 013, "chunk-major `Legacy`
//! compatibility mode"). Rendered in whole device blocks instead, every chunk
//! of a 256-frame block reads one beat, and the voice replays a 64-frame
//! stretch four times a block.
//!
//! The oracle is the tone the voice plays: a dry voice must be the tone,
//! frame for frame, at the source frame the playhead puts it on. Until doc
//! 013 Phase 3 PR 15 the graph was also compared, bit for bit, with the
//! `Net` engine (a `TransportClock` node publishing after each 64-frame
//! chunk); that backend is gone, and every comparison with it had an
//! analytic half, which is what is left. The pitched voice, whose samples
//! have no closed form (a phase vocoder), is pinned instead to itself at
//! 64-frame device blocks: chunk-major, a device block that is a multiple of
//! 64 frames is the same chunks, so it must render the same bits.
//!
//! Mutations (run):
//! - `GraphRender::settle` ignoring `has_legacy`, so the engine renders
//!   whole device blocks with `Legacy` units present → every
//!   `a_voice_plays_in_time_*` and the locate test fail (the voice goes
//!   silent, or parts from the 64-frame render). The shared-cursor, loop and
//!   crossfade tests do not see it: with these fixtures a whole-block render
//!   still plays the tone, to the bit (the first two compared with `Net`
//!   until PR 15; the crossfade test never did). tutti-core's
//!   `legacy_chunk_major.rs` pins that mode directly, with a cursor probe;
//! - the engine publishing its playhead in the walk, before the render
//!   (`TransportClock::advance` writing back) → every chunk reads its own
//!   end: the voice runs a chunk ahead of the tone from frame 0 → every test
//!   here fails.

use std::sync::Arc;

use tutti_core::graph::{OutPort, Source};
use tutti_core::{
    At, Beat, Cents, ChannelLayout, Engine, FadeOut, Frame, InterleavedMut, MotionEvent, NodeKey,
    SampleRate, Samples, Then, Timeline, Transport,
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
    let engine = Engine::new(transport, &mut ed, exec).expect("within the limits");
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

/// Left channel of `out`, frame `f`, is the tone's source frame `src`, to
/// the bit: a dry voice read at a whole source frame copies the sample the
/// wave holds, which is `tone_at(src)` computed the same way.
fn assert_tone_exact(what: &str, out: &[f32], f: usize, src: usize) {
    let (got, want) = (out[2 * f], tone_at(src));
    assert_eq!(
        got.to_bits(),
        want.to_bits(),
        "{what}: frame {f} read {got}, the tone at {src} is {want}"
    );
}

/// Left channel of `out`, frame `f`, against the tone at source frame `src`,
/// within 1e-3 (a crossfade's two gains sum to one only to rounding; a loop
/// wrap reseats the read position through the beat).
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

/// At device blocks of `n` frames, dry and a fifth up: the dry voice is the
/// tone, the pitched one renders what it renders at 64-frame device blocks
/// (when `n` is a multiple of 64, so the chunks are the same), and the
/// playhead ends on the beat the frames rendered put it on, to the bit.
fn plays_in_time(n: usize) {
    // Half a second, whatever the block.
    let blocks = 24_000 / n;
    let frames = blocks * n;
    for cents in [0.0f32, 700.0] {
        let what = format!("{n}-frame blocks, {cents} cents");
        let t = Transport::new(SR);
        let (graph, _ed) = graph_engine(&t, vec![pool(&t, cents)]);
        let b = play(&graph, &t, n, blocks, |_| {});

        // The engine's clock is closed form (`TimelineSegment::beat_at`),
        // no libm: frames × tempo / (60 × rate), from beat 0 at 120 BPM.
        let want = (frames as f64 * 120.0) / (60.0 * SR);
        assert_eq!(
            t.beat().get().to_bits(),
            want.to_bits(),
            "{what}: the playhead ends at {}, not {want}",
            t.beat().get()
        );
        assert_sounds(&what, &b);
        if cents == 0.0 {
            for f in 0..frames {
                assert_tone_exact(&what, &b, f, f);
            }
        } else if n.is_multiple_of(64) {
            let t64 = Transport::new(SR);
            let (by64, _ed) = graph_engine(&t64, vec![pool(&t64, cents)]);
            let r = play(&by64, &t64, 64, frames / 64, |_| {});
            if let Some(i) = (0..2 * frames).find(|&i| r[i].to_bits() != b[i].to_bits()) {
                panic!(
                    "{what}: parts from 64-frame blocks at frame {} (channel {}): {} vs {}",
                    i / 2,
                    i % 2,
                    b[i],
                    r[i]
                );
            }
        }
    }
}

#[test]
fn a_voice_plays_in_time_at_256_frame_blocks() {
    plays_in_time(256);
}

/// Not a multiple of 64: the engine chunks each device block from its own
/// start, 7 × 64 + 32. The dry voice is still the tone (the pitched one's
/// chunks differ from the 64-frame grid's, so it is only heard).
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
/// both render the tone. Each clone polls the
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
fn two_voices_sharing_a_cursor_both_play_the_tone() {
    let t = Transport::new(SR);
    let p = pool(&t, 0.0);
    let (graph, _ed) = graph_engine(&t, vec![p.clone(), p]);
    let b = play(&graph, &t, 512, 60, |_| {});
    assert_sounds("shared cursor", &b);
    for f in 0..b.len() / 2 {
        assert_tone_exact("shared cursor", &b, f, f);
        let right = b[2 * f + 1];
        assert_eq!(
            right.to_bits(),
            tone_at(f).to_bits(),
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

/// **A loop wrap** (beats 1–1.5, reached mid-chunk at frame 36 000): the
/// wrap is the clock's, not a cut, and the voice reads the wrapped beat from
/// the next chunk on. The chunk the wrap falls in plays on past the loop end,
/// from its first frame's beat, as a `Legacy` clip reader always has (and
/// as it did through `Net`, which this was compared with bit for bit until
/// doc 013 PR 15); after it, the dry voice is the tone a loop length back.
#[test]
fn a_loop_wrap_plays_the_tone_a_loop_back() {
    let blocks = 60_000 / 512;
    let t = Transport::new(SR);
    t.settings.loop_span.set_range(1.0, 1.5);
    t.settings.loop_span.set_enabled(true);
    let (graph, _ed) = graph_engine(&t, vec![pool(&t, 0.0)]);
    let b = play(&graph, &t, 512, blocks, |_| {});

    let end = 3 * FPB / 2;
    let wrap_chunk = end / 64 * 64;
    for f in 0..wrap_chunk + 64 {
        assert_tone_exact("loop, before the wrap", &b, f, f);
    }
    for f in wrap_chunk + 64..b.len() / 2 {
        assert_tone("loop, after the wrap", &b, f, f - FPB / 2);
    }
}

/// **A locate scheduled inside a block** (`At::Frame(10 037)`, to beat ¼,
/// immediate): the tone up to the chunk the locate falls in, the target
/// from that chunk's first frame, and the target at its frame after it.
///
/// A `Legacy` clip reader reads the playhead once per 64-frame call. The
/// engine cuts the transport on the locate's frame (in `Env`, for native
/// nodes), but a `Legacy` unit's call is the chunk: it reads the target at
/// the chunk's first frame, 53 frames early. From the next chunk it plays
/// the target at its frame. (A `Net` engine rendered pieces split at the
/// locate's frame, so its voice started the target on that frame; this was
/// compared with it until doc 013 PR 15, bit for bit before the chunk and
/// within a hair after it.)
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
    let t = Transport::new(SR);
    let (graph, _ed) = graph_engine(&t, vec![pool(&t, 0.0)]);
    locate(&t);
    let b = play(&graph, &t, 512, blocks, |_| {});

    let chunk = at / 64 * 64;
    for f in 0..chunk {
        assert_tone_exact("locate, before its chunk", &b, f, f);
    }
    for f in chunk..chunk + 64 {
        assert_tone_exact("locate, its chunk", &b, f, target + (f - chunk));
    }
    for f in chunk + 64..b.len() / 2 {
        assert_tone_exact("locate, after it", &b, f, target + (f - at));
    }
}
