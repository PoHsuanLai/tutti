//! **A sampler voice plays in time through the live graph engine.**
//!
//! A placed voice (a `MemorySource` in a `VoicePool`) derives its read
//! position from the playhead, which it reads from each block's `Env` — per
//! frame, through the transport's own frame clock, with every transport
//! change inside the block (a locate, a loop wrap) on its frame (doc 013
//! items 8 and 9). So the engine (`Engine::new`) renders whole device blocks:
//! with no `Legacy` unit in the plan there is no chunk-major mode, and a
//! voice neither replays a stretch of a block nor waits for a chunk boundary.
//!
//! Until the sampler's nodes ported, a voice polled an `Arc<dyn Timeline>`
//! on every `AudioUnit::process` call, one per 64-frame `Legacy` chunk, and
//! the engine rendered chunk-major to keep that poll right; a loop wrap and
//! a locate landed on their chunk, not their frame. This file pinned that;
//! it now pins the frame.
//!
//! The oracle is the tone the voice plays: a dry voice must be the tone,
//! frame for frame, at the source frame the playhead puts it on. The pitched
//! voice, whose samples have no closed form (a phase vocoder), is pinned to
//! itself at 64-frame device blocks: a voice reads its block in 64-frame
//! pieces from the block's start, so a device block that is a multiple of 64
//! frames is the same pieces, and must render the same bits.
//!
//! Mutations (run):
//! - `place` gating only a range's first frame (the per-call gate) → the
//!   locate and the loop wrap land on their piece, not their frame → both
//!   fail;
//! - the engine publishing its playhead in the walk, before the render
//!   (`TransportClock::advance` writing back) → every block reads its own
//!   end: the voice runs a block ahead of the tone from frame 0 → every test
//!   here fails.

use std::sync::Arc;

use tutti_core::graph::{OutPort, Source};
use tutti_core::{
    At, Beat, Cents, ChannelLayout, Engine, FadeOut, Frame, InterleavedMut, MotionEvent, NodeKey,
    SampleRate, Samples, Then, Timeline, Transport,
};
use tutti_graph::{CrossfadeCurve, Editor, Fade, Prepare};
use tutti_io::Wave;
use tutti_sampler::{MemorySource, Playback, SlotId, Voice, VoicePool, VoiceSource};

const SR: f64 = 48_000.0;
/// Frames per beat at 120 BPM and `SR`.
const FPB: usize = 24_000;

/// The tone's frame `i`: 440 Hz at `SR`.
fn tone_at(i: usize) -> f32 {
    (std::f32::consts::TAU * 440.0 * i as f32 / SR as f32).sin()
}

/// A pool with one voice placed at beat 0, pitched by `cents`, playing four
/// seconds of the tone.
fn pool(cents: f32) -> VoicePool {
    let mut wave = Wave::new(1, SR);
    for i in 0..4 * SR as usize {
        wave.push_frame(&[tone_at(i)]);
    }
    let source = MemorySource::placed(Arc::new(wave), Beat(0.0), None);
    let mut pool = VoicePool::new();
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
/// (the export's `GRAPH_MAX_BLOCK`), with `units` at keys 1, 2, … routed by
/// [`routes`].
fn graph_engine(transport: &Transport, units: Vec<VoicePool>) -> (Engine, Editor) {
    let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(SR), Samples(1024)));
    let n = units.len();
    for (i, u) in units.into_iter().enumerate() {
        ed.insert(NodeKey(i as u64 + 1), "voice", u);
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
        let (graph, _ed) = graph_engine(&t, vec![pool(cents)]);
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
            let (by64, _ed) = graph_engine(&t64, vec![pool(cents)]);
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

/// Not a multiple of 64: the voice reads each device block in pieces from
/// its own start, 7 × 64 + 32. The dry voice is still the tone (the pitched
/// one's pieces differ from the 64-frame grid's, so it is only heard).
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
/// device block into graph blocks.
#[test]
fn a_voice_plays_in_time_at_2048_frame_blocks() {
    plays_in_time(2048);
}

/// **Two nodes over one transport** (two clones of one pool) both render the
/// tone. Each reads the transport from its own block's `Env` and keeps its
/// own record of where the playhead was (its `Clock`), so neither sees the
/// other's render as a jump. Under the `AudioUnit` era two such clones
/// shared one `BeatCursor` on a shared timeline, and only a chunk-major
/// render kept them from seeing a rewind every block; this pins that the
/// native nodes share nothing to get wrong.
///
/// Dry, not pitched: a stretch filter's fill-up would only delay the tone.
#[test]
fn two_voices_over_one_transport_both_play_the_tone() {
    let t = Transport::new(SR);
    let p = pool(0.0);
    let (graph, _ed) = graph_engine(&t, vec![p.clone(), p]);
    let b = play(&graph, &t, 512, 60, |_| {});
    assert_sounds("two voices", &b);
    for f in 0..b.len() / 2 {
        assert_tone_exact("two voices", &b, f, f);
        let right = b[2 * f + 1];
        assert_eq!(
            right.to_bits(),
            tone_at(f).to_bits(),
            "two voices, the second: frame {f} read {right}"
        );
    }
}

/// **A crossfade of a voice** (the pool replaced by a clone of itself over
/// 4096 frames) renders what the voice renders uncrossfaded, to rounding (the
/// two gains sum to one), and the tone throughout: both halves read the
/// transport from their blocks' `Env` during the fade.
#[test]
fn a_crossfaded_voice_plays_on_through_the_fade() {
    let blocks = 60;
    let render = |fade: bool| {
        let t = Transport::new(SR);
        let p = pool(0.0);
        let spare = p.clone();
        let (engine, mut ed) = graph_engine(&t, vec![p]);
        let mut spare = Some(spare);
        play(&engine, &t, 512, blocks, |i| {
            if fade && i == 20 {
                ed.replace(
                    NodeKey(1),
                    spare.take().expect("once"),
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

/// **A loop wrap** (beats 1–1.51, reached at frame 36 240, inside a
/// 512-frame block; 12 240 frames long, not a whole number of the tone's
/// periods, so a wrap on the wrong frame reads the wrong phase): the wrap is the clock's, not a cut, and the voice reads the
/// wrapped beat **from the wrap's frame on** — the tone to the bit up to it,
/// and the tone a loop length back from it (within 1e-3: a wrap re-seats the
/// read through the beat, which the wrapped segments carry to rounding, so
/// the later passes read a hair off the whole frame). (Until the voice
/// read its `Env`, the 64-frame chunk the wrap fell in played on past the
/// loop's end, from its first frame's beat.)
///
/// Mutation (run): `Runs` not cutting a run at a wrap (`wrap_offset`
/// answering `None`) → the block plays on past the loop's end → fails.
#[test]
fn a_loop_wrap_plays_the_tone_a_loop_back_from_its_frame() {
    let blocks = 60_000 / 512;
    let t = Transport::new(SR);
    t.settings.loop_span.set_range(1.0, 1.51);
    t.settings.loop_span.set_enabled(true);
    let (graph, _ed) = graph_engine(&t, vec![pool(0.0)]);
    let b = play(&graph, &t, 512, blocks, |_| {});

    let (end, len) = (FPB * 151 / 100, FPB * 51 / 100);
    assert_ne!(end % 512, 0, "the wrap falls inside a block");
    assert_ne!(len * 440 % 48_000, 0, "a wrap's frame shows in the tone");
    for f in 0..end {
        assert_tone_exact("loop, before the wrap", &b, f, f);
    }
    for f in end..b.len() / 2 {
        let src = FPB + (f - end) % len;
        assert_tone("loop, from the wrap", &b, f, src);
    }
}

/// **A locate scheduled inside a block** (`At::Frame(10 037)`, to beat ¼,
/// immediate) lands **on its frame**: the tone up to it, and the target at
/// its frame from it. The engine cuts the transport on the locate's frame
/// (a change in the block's `Env`), and the voice reads it there. (Until the
/// voice read its `Env`, a `Legacy` clip reader read the playhead once per
/// 64-frame call and played the target from the chunk's first frame, 53
/// frames early.)
///
/// Mutation (run): `Runs` ignoring `Env::changes` → the voice plays the tone
/// on past the locate → fails.
#[test]
fn a_scheduled_locate_lands_on_its_frame() {
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
    let (graph, _ed) = graph_engine(&t, vec![pool(0.0)]);
    locate(&t);
    let b = play(&graph, &t, 512, blocks, |_| {});

    for f in 0..at {
        assert_tone_exact("locate, before it", &b, f, f);
    }
    for f in at..b.len() / 2 {
        assert_tone_exact("locate, from it", &b, f, target + (f - at));
    }
}
