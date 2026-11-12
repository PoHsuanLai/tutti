//! A loop is one value on every tier, and a pool's and a node's handles steer
//! the voice they hold.
//!
//! The material is a ramp whose value *is* its frame index, played at the file's
//! own rate on a transport that moves one file frame per output frame, so
//! output frame `n` is the file frame the voice read: every figure below is an
//! analytic position (a loop repeats `[start, end)`; a speed of 2 reads every
//! second frame), never one render compared with another.

use std::path::Path;
use std::sync::Arc;

use tutti_codec::Wave;
use tutti_sampler::testing::{MockTransport, loop_frames, play, settle};
use tutti_sampler::{
    ByHand, Direction, DiskStreamer, DiskStreamerConfig, DiskVoice, Loop, LoopError, MemorySource,
    Placement, Playback, SlotId, Stream, VoiceNode, VoiceNodeHandle, VoicePool, VoiceSource,
    VoiceWindow,
};
use tutti_types::{Beat, Bpm, PlaybackRate, SampleRate, Samples};

const SR: f64 = 48_000.0;
const RATE: SampleRate = SampleRate(SR);
/// Frames in the file: more than every figure below reads.
const LEN: usize = 4_096;
/// Frames each case renders.
const FRAMES: usize = 2_000;
const BLOCK: usize = 64;

/// The value of file frame `i`: its index, scaled into `[0, 1)`, exact in `f32`.
fn value(i: usize) -> f32 {
    i as f32 / LEN as f32
}

/// The file frames a hard loop over `[start, end)` plays, from frame 0: the
/// file up to `end`, then `[start, end)` over and over.
fn looped_frame(n: usize, start: usize, end: usize) -> usize {
    if n < end {
        n
    } else {
        start + (n - end) % (end - start)
    }
}

fn ramp() -> Arc<Wave> {
    let mut w = Wave::new(1, SampleRate::new(SR));
    for i in 0..LEN {
        w.push_frame(&[value(i)]);
    }
    Arc::new(w)
}

fn write_ramp(path: &Path) {
    let samples: Vec<f32> = (0..LEN).map(value).collect();
    tutti_fixtures::signal::write_wav_f32(path, RATE, 1, &samples);
}

/// 120 bpm at `SR`: one beat is `SR / 2` frames, so the file's own rate reads
/// one frame per output frame.
fn rolling() -> Arc<MockTransport> {
    MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0))
}

/// A hand-driven streamer, with the ramp's stream open and primed.
fn streaming(dir: &Path) -> (DiskStreamer<ByHand>, Stream, tutti_sampler::StreamReader) {
    let path = dir.join("ramp.wav");
    write_ramp(&path);
    let mut streamer = DiskStreamer::manual(SampleRate::new(SR), DiskStreamerConfig::default())
        .expect("streamer builds");
    let opening = streamer.open(&path, Placement::HEAD);
    let (stream, reader) = settle(&mut streamer, opening).expect("the file opens");
    (streamer, stream, reader)
}

fn window() -> VoiceWindow {
    VoiceWindow::from(Beat::new(0.0))
}

/// A memory voice node over the ramp, and its handle.
fn memory_node() -> (VoiceNode, VoiceNodeHandle) {
    let source = MemorySource::placed(ramp(), window()).expect("a placeable wave");
    VoiceNode::with_channels(
        tutti_sampler::Voice {
            source: VoiceSource::Memory(source),
            play: Playback::default(),
        },
        tutti_types::ChannelLayout::MONO,
    )
    .with_handle()
}

fn assert_frames(what: &str, got: &[f32], want: impl Fn(usize) -> usize) {
    for (n, &g) in got.iter().enumerate() {
        assert!(
            (g - value(want(n))).abs() < 1e-6,
            "{what}: output frame {n} is {} (file frame {}), want file frame {}",
            g,
            (g * LEN as f32).round(),
            want(n)
        );
    }
}

/// **`Loop` refuses what is not a loop where it is made.** An empty span, a
/// reversed one, and a crossfade longer than the span each name their reason;
/// a crossfade as long as the loop is the longest that fits.
#[test]
fn a_loop_refuses_an_empty_or_reversed_span_and_a_crossfade_that_does_not_fit() {
    let span = |a: usize, b: usize| Samples::new(a)..Samples::new(b);
    assert_eq!(
        Loop::new(span(10, 10)),
        Err(LoopError::EmptySpan {
            start: Samples::new(10),
            end: Samples::new(10)
        })
    );
    assert!(matches!(
        Loop::new(span(20, 10)),
        Err(LoopError::EmptySpan { .. })
    ));
    let looped = Loop::new(span(100, 164)).expect("a loop of 64 frames");
    assert_eq!(looped.len(), Samples::new(64));
    assert_eq!(
        looped.with_crossfade(Samples::new(65)),
        Err(LoopError::CrossfadeTooLong {
            crossfade: Samples::new(65),
            len: Samples::new(64)
        })
    );
    let longest = looped
        .with_crossfade(Samples::new(64))
        .expect("a crossfade as long as the loop fits");
    assert_eq!(longest.crossfade(), Samples::new(64));
}

/// **A looped memory voice repeats the span's frames.** Hard loop `[300, 700)`
/// of the ramp, set through the node's handle: the output is the file to frame
/// 700, then frames 300 to 699 over and over.
#[test]
fn a_looped_memory_voice_repeats_the_span() {
    let (mut node, handle) = memory_node();
    handle
        .set_loop(Some(loop_frames(300, 700, 0)))
        .expect("a memory voice never fails");
    let got = render_with(&mut node, FRAMES);
    assert_frames("memory", &got, |n| looped_frame(n, 300, 700));
}

/// **A looped disk voice repeats the same span's frames**, the loop set on the
/// stream its handle owns: the same `Loop` as the memory voice, and the same
/// analytic positions.
#[test]
fn a_looped_disk_voice_repeats_the_span() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let (mut streamer, stream, reader) = streaming(dir.path());
    let voice = DiskVoice::from_reader(reader, window());
    let (mut node, handle) = VoiceNode::streaming(voice, stream, Playback::default()).with_handle();
    handle
        .set_loop(Some(loop_frames(300, 700, 0)))
        .expect("the streamer lives");
    let _ = streamer.step_until_settled(1_000);
    let got = render_with(&mut node, FRAMES);
    assert_frames("disk", &got, |n| looped_frame(n, 300, 700));
}

/// `frames` of `node`, prepared, from beat 0.
fn render_with(node: &mut VoiceNode, frames: usize) -> Vec<f32> {
    use tutti_graph::Node;
    node.prepare(&tutti_graph::Prepare::new(
        RATE,
        tutti_types::MaxBlock::of(Samples::new(BLOCK)),
    ));
    play(node, &rolling(), RATE, frames, BLOCK).swap_remove(0)
}

/// **A disk voice added through the pool handle keeps its loop and direction.**
/// `add_streaming` takes the voice's `Playback` with the stream: the loop it
/// carries plays the span, and a reversed one plays the file from its end.
/// (A disk voice cannot be added without its stream, so the add that once
/// dropped both silently is not writable: see `VoicePoolHandle::add`.)
#[test]
fn a_disk_voice_added_to_a_pool_keeps_its_loop_and_direction() {
    for direction in [Direction::Forward, Direction::Reverse] {
        let dir = tempfile::tempdir().expect("a temp dir");
        let (mut streamer, stream, reader) = streaming(dir.path());
        let voice = DiskVoice::from_reader(reader, window());
        let (pool, handle) = VoicePool::with_channels(tutti_types::ChannelLayout::MONO)
            .expect("mono is a width")
            .with_handle();
        let play = Playback {
            loop_: Some(loop_frames(300, 700, 0)),
            direction,
            ..Playback::default()
        };
        handle
            .add_streaming(SlotId(1), voice, stream, play)
            .expect("the queue has room");
        let _ = streamer.step_until_settled(1_000);
        let mut pool = tutti_sampler::testing::prepared(pool, RATE, BLOCK);
        let got = play_pool(&mut pool, &mut streamer);
        match direction {
            // Reverse mirrors the file and ignores the loop. A live stream turns
            // by a crossfade from what it was playing (about 256 frames past
            // the block it is in, over 512 more), so the mirror is judged past
            // it.
            Direction::Reverse => {
                const TURNED: usize = 1_024;
                assert_frames("reversed disk voice in a pool", &got[TURNED..], |n| {
                    LEN - 1 - (TURNED + n)
                });
            }
            Direction::Forward => {
                assert_frames("looped disk voice in a pool", &got, |n| {
                    looped_frame(n, 300, 700)
                });
            }
        }
        drop(handle);
    }
}

/// `FRAMES` of `pool` from beat 0, the streamer stepped between blocks as a
/// running one is, so a stream that turns or loops has its ring refilled.
fn play_pool(pool: &mut VoicePool, streamer: &mut DiskStreamer<ByHand>) -> Vec<f32> {
    let t = rolling();
    let mut out = Vec::new();
    while out.len() < FRAMES {
        let block = tutti_sampler::testing::block(pool, &t, RATE, BLOCK).swap_remove(0);
        out.extend(block);
        t.advance(BLOCK as i64, RATE);
        let _ = streamer.step_until_settled(1_000);
    }
    out
}

/// **The handle sets a memory voice's speed and stops and resumes it.** At
/// speed 2 the voice reads every second file frame; stopped it is silent and
/// reads nothing, and it resumes where the transport is.
#[test]
fn a_node_handle_sets_speed_and_stops_and_resumes_the_voice() {
    let (mut node, handle) = memory_node();
    handle.set_speed(PlaybackRate::new(2.0));
    let got = render_with(&mut node, 400);
    assert_frames("speed 2", &got, |n| 2 * n);

    let (mut node, handle) = memory_node();
    node = tutti_sampler::testing::prepared(node, RATE, BLOCK);
    let t = rolling();
    let mut heard = Vec::new();
    for block in 0..6 {
        match block {
            2 => handle.stop(),
            4 => handle.play(),
            _ => {}
        }
        heard.extend(tutti_sampler::testing::block(&mut node, &t, RATE, BLOCK).swap_remove(0));
        t.advance(BLOCK as i64, RATE);
    }
    assert!(handle.is_playing());
    for (n, &g) in heard.iter().enumerate() {
        let stopped = (2 * BLOCK..4 * BLOCK).contains(&n);
        let want = if stopped { 0.0 } else { value(n) };
        assert!(
            (g - want).abs() < 1e-6,
            "frame {n}: {g}, want {want} (stopped: {stopped})"
        );
    }
}
