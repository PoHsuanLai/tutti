//! A disk voice forked for an offline render plays its file itself.
//!
//! A fork (`isolate`, then `rebind_offline` onto the render's clock, then
//! `reset`, the order a graph fork and `clone_isolated` both use) cannot read
//! the butler's ring: the live audio thread is its one consumer. So it reads
//! the file the butler's record names, on demand, as `offline_read` does.
//! These tests pin what it plays: the file's own samples from the frame its
//! beat falls on, at another render rate, looped as the stream is looped when
//! the fork is taken, and nothing once its stream is gone.
//!
//! Each render moves its clock per 64-frame chunk, after the chunk, as the
//! engine's chunk-major `Legacy` renders do (doc 013's per-chunk timeline).

use std::any::Any;
use std::path::Path;
use std::sync::Arc;

use tutti_core::transport::{OfflineTimeline, OfflineTimelineConfig, OfflineTransport};
use tutti_core::{AudioUnit, Beat, Bpm, BufferVec, SamplePosition, SampleRate, Timeline};
use tutti_sampler::{
    Command, DiskStreamer, DiskVoice, LoopSetting, Playback, Voice, VoiceNode, VoiceSource,
};

const SR: f64 = 48_000.0;
const CHUNK: usize = 64;

/// Frame `i` of every test file: distinct per frame, and exactly what an f32
/// WAV hands back.
fn value(i: usize) -> f32 {
    (i as f32 + 1.0) * 1e-5
}

/// A stereo f32 WAV of `frames` at `rate`: `value(i)` left, `-value(i)` right.
fn write_ramp(path: &Path, rate: u32, frames: usize) {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(path, spec).expect("writes");
    for i in 0..frames {
        w.write_sample(value(i)).expect("writes");
        w.write_sample(-value(i)).expect("writes");
    }
    w.finalize().expect("writes");
}

/// A hand-stepped butler streaming `path` on channel 0, primed.
fn streamer_on(path: &Path) -> DiskStreamer {
    let mut streamer = DiskStreamer::manual(SampleRate(SR), Default::default()).expect("builds");
    streamer
        .commands()
        .send(Command::Stream {
            channel_index: 0,
            file_path: path.to_path_buf(),
            offset: SamplePosition(0.0),
        })
        .expect("the butler is alive");
    assert!(
        streamer.step_until_settled(1_000) < 1_000,
        "the ring primes"
    );
    streamer
}

/// The live voice on channel 0, placed at `beat` on a clock nothing moves.
fn live_voice(streamer: &DiskStreamer, beat: f64) -> DiskVoice {
    let live: Arc<dyn Timeline> = Arc::new(OfflineTimeline::new(&config(0.0)));
    streamer
        .status()
        .take_disk_voice(0, live, Beat(beat), None)
        .expect("the link is installed")
}

fn config(start: f64) -> OfflineTimelineConfig {
    OfflineTimelineConfig {
        start_beat: Beat(start),
        tempo: Bpm(120.0),
        sample_rate: SampleRate(SR),
        loop_range: None,
    }
}

/// Fork `unit` for a render at `rate` on `clock`, as a graph fork does.
fn fork<U: AudioUnit + Clone>(unit: &U, clock: &OfflineTransport, rate: f64) -> U {
    let mut copy = unit.clone();
    copy.isolate();
    copy.rebind_offline(clock as &dyn Any);
    copy.reset();
    copy.set_sample_rate(SampleRate(rate));
    copy
}

/// Render `frames` of `unit`'s two channels, the clock moved after each chunk.
fn render(unit: &mut dyn AudioUnit, clock: &OfflineTimeline, frames: usize) -> [Vec<f32>; 2] {
    let input = BufferVec::new(0);
    let mut output = BufferVec::new(2);
    let mut planes = [Vec::new(), Vec::new()];
    let mut done = 0;
    while done < frames {
        let n = CHUNK.min(frames - done);
        unit.process(n, &input.buffer_ref(), &mut output.buffer_mut());
        let out = output.buffer_ref();
        for (c, plane) in planes.iter_mut().enumerate() {
            plane.extend((0..n).map(|i| out.at_f32(c, i)));
        }
        clock.advance(n);
        done += n;
    }
    planes
}

/// A render's clock at the export rate, and the same clock as a rebind
/// context.
fn clock_at(rate: f64) -> (Arc<OfflineTimeline>, OfflineTransport) {
    let clock = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        sample_rate: SampleRate(rate),
        ..config(0.0)
    }));
    let ctx: OfflineTransport = clock.clone();
    (clock, ctx)
}

/// **A fork plays the file's own samples, from the frame its beat falls
/// on**: silent before beat 2 (frame 48 000 at 120 BPM), then frame `k` of
/// the file on render frame `48 000 + k`, exactly, on both channels. As a bare
/// voice and inside a `VoiceNode` (which reads it a frame at a time, through
/// `tick`, with the clock moving only between chunks).
///
/// Mutation (run): the seat not stepping (`frames + 1` → `frames`) → every
/// chunk repeats one frame → fails. Mutation (run): `rebind_offline` not
/// handing the copy its file → silence → fails. Mutation (run): the
/// placement gate's whole-frame snap removed (`interp::window_position`) →
/// file frame 129 reads an ulp off → fails.
#[test]
fn a_fork_plays_the_file_from_the_frame_its_beat_falls_on() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    write_ramp(&path, SR as u32, 60_000);
    let streamer = streamer_on(&path);
    let voice = live_voice(&streamer, 2.0);
    let node = VoiceNode::with_channels(
        Voice {
            source: VoiceSource::Disk(voice.clone()),
            play: Playback::default(),
            channel_index: Some(0),
        },
        2usize,
    );

    for (what, mut unit) in [
        (
            "a bare voice",
            Box::new(voice.clone()) as Box<dyn AudioUnit>,
        ),
        ("a voice node", Box::new(node.clone())),
    ] {
        let (clock, ctx) = clock_at(SR);
        let mut copy: Box<dyn AudioUnit> = {
            unit.isolate();
            unit.rebind_offline(&ctx as &dyn Any);
            unit.reset();
            unit.set_sample_rate(SampleRate(SR));
            unit
        };
        let [l, r] = render(copy.as_mut(), &clock, 48_000 + 12_000);
        assert!(
            l[..48_000].iter().all(|&s| s == 0.0),
            "{what}: sounded before beat 2"
        );
        for k in 0..12_000 {
            assert_eq!(l[48_000 + k], value(k), "{what}: left, file frame {k}");
            assert_eq!(r[48_000 + k], -value(k), "{what}: right, file frame {k}");
        }
    }
}

/// **A fork at another rate resamples**: a 24 kHz file rendered at 48 kHz
/// reads file frame `n / 2` on render frame `n` of the clip, and ends one
/// second of file later (48 000 render frames), silent after. The file is a
/// ramp, which the cubic kernel reproduces between frames up to rounding.
///
/// Mutation (run): the read rate without the conversion
/// (`SrcRatio::for_rates` → `SrcRatio::UNITY` in `offline_read_rate`) → one
/// file frame per render frame → fails. Mutation (run): the gate measuring
/// by the render's rate instead of the file's → the clip enters late → fails.
#[test]
fn a_fork_at_another_rate_resamples() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp_24k.wav");
    write_ramp(&path, 24_000, 24_000);
    let streamer = streamer_on(&path);
    let voice = live_voice(&streamer, 1.0);
    let (clock, ctx) = clock_at(SR);
    let mut copy = fork(&voice, &ctx, SR);
    let [l, _] = render(&mut copy, &clock, 24_000 + 48_000 + 1_000);

    assert!(
        l[..24_000].iter().all(|&s| s == 0.0),
        "sounded before beat 1"
    );
    assert_eq!(l[24_000], value(0), "the clip's first frame on beat 1");
    // From frame 4: the first frames' taps clamp at the file's start.
    for n in 4..47_990 {
        let want = (n as f32 / 2.0 + 1.0) * 1e-5;
        let got = l[24_000 + n];
        assert!(
            (got - want).abs() < 1e-6,
            "render frame {n} of the clip read {got}, want {want} (file frame {})",
            n as f32 / 2.0
        );
    }
    assert!(
        l[24_000 + 48_000..].iter().all(|&s| s == 0.0),
        "sounded past the file's end"
    );
}

/// **A fork loops as the stream is looped when it is taken** — a loop set on
/// the live stream after the voice was built, which the voice itself never
/// saw: it plays frames `0..3000`, then `1000..3000` over and over. With a
/// 100-frame crossfade the last 100 frames before the end blend linearly into
/// the loop's first 100.
///
/// Mutation (run): the fork reading its loop when the voice was built rather
/// than when it is rebound (`StreamFile::loop_` forced `Off`) → plays
/// straight on past 3000 → fails. Mutation (run): the crossfade blend dropped
/// → the hard-loop values in the fade → fails.
#[test]
fn a_fork_loops_as_the_stream_is_looped_when_it_is_taken() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    write_ramp(&path, SR as u32, 20_000);
    let mut streamer = streamer_on(&path);
    let voice = live_voice(&streamer, 0.0);

    for fade in [0usize, 100] {
        streamer
            .commands()
            .send(Command::Loop {
                channel_index: 0,
                setting: LoopSetting::On {
                    start: SamplePosition(1_000.0),
                    end: SamplePosition(3_000.0),
                    crossfade_frames: fade,
                },
            })
            .expect("the butler is alive");
        let _ = streamer.step_until_settled(1_000);

        let (clock, ctx) = clock_at(SR);
        let mut copy = fork(&voice, &ctx, SR);
        let [l, _] = render(&mut copy, &clock, 9_000);
        for (k, &got) in l.iter().enumerate() {
            let at = |p: usize| {
                if p < 3_000 {
                    p
                } else {
                    1_000 + (p - 1_000) % 2_000
                }
            };
            let p = at(k);
            let want = if p >= 3_000 - fade {
                let into = p - (3_000 - fade);
                let t = into as f32 / fade as f32;
                value(p) * (1.0 - t) + value(1_000 + into) * t
            } else {
                value(p)
            };
            assert!(
                (got - want).abs() < 1e-7,
                "fade {fade}: render frame {k} read {got}, want {want} (loop position {p})"
            );
        }
    }
}

/// **A fork of a stream that is gone plays nothing**: the live voice's
/// channel restarted on another file, so the live voice is on a ring the
/// butler no longer feeds, and so is its fork — it must not pick up the other
/// file from the channel.
///
/// Mutation (run): `StreamOrigin::describe` not checking the region → the
/// fork plays the other file → fails.
#[test]
fn a_fork_of_a_stream_that_is_gone_plays_nothing() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let (first, second) = (dir.path().join("a.wav"), dir.path().join("b.wav"));
    write_ramp(&first, SR as u32, 10_000);
    write_ramp(&second, SR as u32, 10_000);
    let mut streamer = streamer_on(&first);
    let voice = live_voice(&streamer, 0.0);
    streamer
        .commands()
        .send(Command::Stream {
            channel_index: 0,
            file_path: second,
            offset: SamplePosition(0.0),
        })
        .expect("the butler is alive");
    let _ = streamer.step_until_settled(1_000);

    let (clock, ctx) = clock_at(SR);
    let mut copy = fork(&voice, &ctx, SR);
    let [l, _] = render(&mut copy, &clock, 4_096);
    assert!(
        l.iter().all(|&s| s == 0.0),
        "the fork played the channel's new file"
    );
}
