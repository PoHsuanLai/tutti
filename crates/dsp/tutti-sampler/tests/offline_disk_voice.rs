//! A disk voice forked for an offline render plays its file itself.
//!
//! A fork (the node's fork source, as a graph fork takes it:
//! `ForkMode::Offline`) cannot read the butler's ring: the live audio thread
//! is its one consumer. So it reads the file the butler's record names, on
//! demand, as `offline_read` does, on the render's transport from each
//! block's `Env`. These tests pin what it plays: the file's own samples from
//! the frame its beat falls on, at another render rate, looped as the stream
//! is looped when the fork is taken, and nothing once its stream is gone.
//!
//! Each render hands its node blocks of `BLOCK` frames on a render clock that
//! counts frames (as an export's does), moved after each.

use std::cell::Cell;
use std::path::Path;
use std::sync::Arc;

use tutti_core::transport::{OfflineTimeline, OfflineTimelineConfig, OfflineTransport};
use tutti_core::{Beat, Bpm, Frame, FrameClock, LoopRange, SamplePosition, SampleRate, Samples};
use tutti_graph::{contract, Env, ForkHealth, ForkMode, IntoNode, Node, Prepare, TransportChanges};
use tutti_sampler::{
    Command, DiskStreamer, DiskVoice, LoopSetting, Playback, Voice, VoiceNode, VoiceSource,
};

const SR: f64 = 48_000.0;
const BLOCK: usize = 64;

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
    streamer_with(path, Default::default())
}

/// [`streamer_on`], with `config`.
fn streamer_with(path: &Path, config: tutti_sampler::DiskStreamerConfig) -> DiskStreamer {
    let mut streamer = DiskStreamer::manual(SampleRate(SR), config).expect("builds");
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

/// The live voice on channel 0, placed at `beat`.
fn live_voice(streamer: &DiskStreamer, beat: f64) -> DiskVoice {
    streamer
        .status()
        .take_disk_voice(0, Beat(beat), None)
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

/// A render's transport: a frame clock at 120 BPM (counted, as an export's
/// render clock), from beat 0, looping at `looping`; moved as it renders.
struct Clock {
    clock: Cell<FrameClock>,
    frame: Cell<u64>,
    looping: Option<LoopRange>,
}

impl Clock {
    /// The next block of `len` frames, where the clock stands.
    fn env(&self, len: usize) -> Env {
        let c = self.clock.get();
        Env {
            frame: Frame(self.frame.get()),
            sample_rate: c.sample_rate(),
            block_len: Samples(len),
            transport: tutti_graph::Transport::counted(
                true,
                c.tempo(),
                c.origin(),
                self.looping.map(|l| tutti_graph::LoopRange {
                    start: l.start(),
                    end: l.end(),
                }),
            ),
            changes: TransportChanges::NONE,
        }
    }

    fn advance(&self, n: usize) {
        let mut c = self.clock.get();
        c.advance(Samples(n), self.looping);
        self.clock.set(c);
        self.frame.set(self.frame.get() + n as u64);
    }
}

/// A fork of a node, as the graph's fork source hands it over: the unit and
/// its health probe.
struct Fork {
    node: Box<dyn Node>,
    health: Option<Arc<dyn ForkHealth>>,
}

impl Fork {
    /// The first failure the fork latched, if any.
    fn fault(&self) -> Option<String> {
        self.health
            .as_ref()
            .and_then(|h| h.fault())
            .map(|(_, cause)| cause.to_string())
    }
}

/// Fork `node` for an offline render (`ForkMode::Offline`, the export's), not
/// yet prepared.
fn fork_unprepared(node: impl IntoNode, ctx: &OfflineTransport) -> Fork {
    let source = node.into_parts().fork.expect("forkable");
    let forked = source.fork(ForkMode::Offline(ctx)).expect("forks");
    Fork {
        node: forked.node,
        health: forked.health,
    }
}

/// Fork `voice` for a render at `rate`, as a graph fork does: forked, then
/// prepared.
fn fork(voice: &DiskVoice, ctx: &OfflineTransport, rate: f64) -> Fork {
    let mut f = fork_unprepared(voice.clone(), ctx);
    f.node
        .prepare(&Prepare::new(SampleRate(rate), Samples(BLOCK)));
    f
}

/// Render `frames` of `node`'s two channels, the clock moved after each block.
fn render(node: &mut dyn Node, clock: &Clock, frames: usize) -> [Vec<f32>; 2] {
    let mut planes = [Vec::new(), Vec::new()];
    let mut done = 0;
    while done < frames {
        let n = BLOCK.min(frames - done);
        let out = contract::drive_in(node, &clock.env(n), &[], &[], &[]).audio;
        for (plane, c) in planes.iter_mut().zip(out) {
            plane.extend(c);
        }
        clock.advance(n);
        done += n;
    }
    planes
}

/// A render clock at `rate`, and the offline context a fork is asked for
/// (the export's `ForkMode::Offline`; a native node reads its render's
/// transport from its `Env`, not from this).
fn clock_at(rate: f64) -> (Clock, OfflineTransport) {
    looping_clock_at(rate, None)
}

fn looping_clock_at(rate: f64, looping: Option<LoopRange>) -> (Clock, OfflineTransport) {
    let clock = Clock {
        clock: Cell::new(FrameClock::new(Beat(0.0), Bpm(120.0), SampleRate(rate))),
        frame: Cell::new(0),
        looping,
    };
    let ctx = OfflineTransport::new(Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        sample_rate: SampleRate(rate),
        ..config(0.0)
    })));
    (clock, ctx)
}

/// **A fork plays the file's own samples, from the frame its beat falls
/// on**: silent before beat 2 (frame 48 000 at 120 BPM), then frame `k` of
/// the file on render frame `48 000 + k`, exactly, on both channels. As a bare
/// voice and inside a `VoiceNode`.
///
/// Mutation (run): the placement not stepping (`step.advance(Samples(k))` →
/// `Samples(0)` in `place`) → every block repeats one frame → fails.
/// Mutation (run): `fork_copy` not handing the copy its file → silence →
/// fails. Mutation (run): both
/// whole-frame rules removed (`snap_to_whole_frame` and `tap_indices`'s
/// carry of a fraction that rounds to 1.0) → file frame 129 reads an ulp off
/// → fails. Either rule alone lands this case: the gate's origin comes out a
/// hair under the frame, which each of them catches.
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

    let (_, ctx) = clock_at(SR);
    let mut node_fork = fork_unprepared(node, &ctx);
    node_fork
        .node
        .prepare(&Prepare::new(SampleRate(SR), Samples(BLOCK)));
    for (what, mut copy) in [
        ("a bare voice", fork(&voice, &ctx, SR)),
        ("a voice node", node_fork),
    ] {
        let (clock, _) = clock_at(SR);
        let [l, r] = render(&mut *copy.node, &clock, 48_000 + 12_000);
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
/// (`SrcRatio::for_rates` → `SrcRatio::UNITY` in `DiskVoice::step_rate`) →
/// one file frame per render frame → fails. Mutation (run): the gate
/// measuring by the render's rate instead of the file's (`DiskVoice::gate`'s
/// `source_rate`) → the clip enters late → fails.
#[test]
fn a_fork_at_another_rate_resamples() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp_24k.wav");
    write_ramp(&path, 24_000, 24_000);
    let streamer = streamer_on(&path);
    let voice = live_voice(&streamer, 1.0);
    let (clock, ctx) = clock_at(SR);
    let mut copy = fork(&voice, &ctx, SR);
    let [l, _] = render(&mut *copy.node, &clock, 24_000 + 48_000 + 1_000);

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
/// saw: it plays frames `0..3000`, then `1000..3000` over and over.
///
/// Crossfaded, the last 100 frames before the end blend toward the 100 that
/// lead into the loop's start (`[900, 1000)`), frame `k` of the fade weighing
/// the lead-in `(k + 1) / 101`, and the wrap then plays 1000: the join is the
/// file's own step (doc 013's S3; `LoopSpan`). At unit speed on whole frames
/// every read is a frame of that sequence exactly.
///
/// Mutation (run): the fork reading its loop when the voice was built rather
/// than when it is forked (`StreamFile::loop_` forced `Off`) → plays
/// straight on past 3000 → fails. Mutation (run): the crossfade blend dropped
/// → the hard-loop values in the fade → fails. Mutation (run): the lead-in
/// `start + k` (the old head replay) → fails.
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
        let [l, _] = render(&mut *copy.node, &clock, 9_000);
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
                let t = (into + 1) as f32 / (fade + 1) as f32;
                value(p) * (1.0 - t) + value(1_000 - fade + into) * t
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

/// A mono f32 WAV of `frames` at [`SR`]: a sine of period [`PERIOD`] frames.
fn write_sine(path: &Path, frames: usize) {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: SR as u32,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(path, spec).expect("writes");
    for i in 0..frames {
        let v = (std::f64::consts::TAU * i as f64 / PERIOD as f64).sin() as f32;
        w.write_sample(v).expect("writes");
    }
    w.finalize().expect("writes");
}

/// The period of [`write_sine`]'s sine, in frames.
const PERIOD: usize = 100;

/// **A fork's crossfaded loop is continuous at its wrap** (doc 013's S3):
/// on a sine whose loop points click when cut hard — the loop starts on a
/// rising zero crossing and ends a quarter period later in the cycle, so the
/// frame before the wrap is the crest and the one after it zero — no step in
/// the render is larger than the sine's own (`2 sin(π / PERIOD)`, its slope
/// at a zero crossing), round the loop three times. The fade leads into the
/// loop's start, so the join is the sine's own step.
///
/// And a loop from frame 0, `[0, 2281)`, with nothing before its start: the
/// fade goes into the loop's head and the wrap resumes after it (at 256),
/// still continuous (its end sits a quarter period past 256, and cut hard at
/// 0 it clicks).
///
/// The hard loop is asserted to click first, so the loop points have teeth.
///
/// Mutation (run): the lead-in `start + k` in `LoopSpan::fade_at` (the old
/// head replay: fade into the loop's first frames, then play them again) → a
/// step far above the sine's own at the wrap → fails. Mutation (run): the fade
/// dropped (`LoopTap::fade` always `None`) → the hard cut → fails. Mutation
/// (run): the head mode removed (the fade clamped to `start`) → the loop from
/// 0 cuts hard → fails.
#[test]
fn a_forks_crossfaded_loop_is_continuous_at_its_wrap() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("sine.wav");
    write_sine(&path, 4_000);
    let mut streamer = streamer_on(&path);
    let voice = live_voice(&streamer, 0.0);
    let own = (2.0 * (std::f64::consts::PI / PERIOD as f64).sin()) as f32 + 1e-5;

    for (start, end, fade, clicks) in [
        (1_000.0, 3_025.0, 0usize, true),
        (1_000.0, 3_025.0, 256, false),
        (0.0, 2_281.0, 0, true),
        (0.0, 2_281.0, 256, false),
    ] {
        streamer
            .commands()
            .send(Command::Loop {
                channel_index: 0,
                setting: LoopSetting::On {
                    start: SamplePosition(start),
                    end: SamplePosition(end),
                    crossfade_frames: fade,
                },
            })
            .expect("the butler is alive");
        let _ = streamer.step_until_settled(1_000);

        let (clock, ctx) = clock_at(SR);
        let mut copy = fork(&voice, &ctx, SR);
        let [l, _] = render(&mut *copy.node, &clock, 3_025 + 3 * 2_025);
        let at_loop = format!("[{start}, {end})");
        let (step, at) = l
            .windows(2)
            .enumerate()
            .map(|(i, w)| ((w[1] - w[0]).abs(), i))
            .fold((0.0f32, 0), |a, b| if b.0 > a.0 { b } else { a });
        if clicks {
            assert!(
                step > 0.9,
                "{at_loop}: the hard loop does not click ({step} at {at})"
            );
        } else {
            assert!(
                step <= own,
                "{at_loop} fade {fade}: a step of {step} at render frame {at}, larger than the sine's own {own}"
            );
        }
    }
}

/// **A reversed fork falls silent past the file's first frame** (doc 013's
/// S1), as a forward one does past its last: the file backwards, then
/// silence — not frame 0 held as DC.
///
/// Mutation (run): `OfflineRead::read_into` without either silence — the
/// early close of a reversed read at or past `len`, and the reverse arm's own
/// check (the old `(len - 1 - pos).max(0.0)` alone) → `value(0)` from render
/// frame `LEN` on → fails. (The early close alone keeps it silent once the
/// file's length is known; the arm's check covers a first read already past
/// it.)
#[test]
fn a_reversed_fork_is_silent_past_the_first_frame() {
    const LEN: usize = 5_000;
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    write_ramp(&path, SR as u32, LEN);
    let streamer = streamer_on(&path);
    let mut voice = live_voice(&streamer, 0.0);
    voice.set_direction(tutti_sampler::Direction::Reverse);
    let (clock, ctx) = clock_at(SR);
    let mut copy = fork(&voice, &ctx, SR);
    let [l, _] = render(&mut *copy.node, &clock, LEN + 2_000);
    for (k, &got) in l[..LEN].iter().enumerate() {
        assert_eq!(got, value(LEN - 1 - k), "clip frame {k}");
    }
    for (k, &got) in l.iter().enumerate().skip(LEN) {
        assert_eq!(got, 0.0, "clip frame {k}, past the file's first frame");
    }
}

/// **A varispeed change lands on a fork's next block**, at the position the
/// new speed gives: 32 frames at 1×, then — the live voice set to 2× and
/// forked again — 32 more, seated where the gate puts the playhead at 2× (32
/// frames in at 2× is file frame 64) and stepping two file frames a frame.
/// (The `AudioUnit` era pinned a change *between two frames of one clock
/// reading*, reachable only frame by frame; a block reads its rates once.)
///
/// Mutation (run): the fork's gate ignoring varispeed (`DiskVoice::window_rate`
/// without `effective_speed`) → the second render starts at file frame 32 →
/// fails.
#[test]
fn a_varispeed_change_lands_on_a_forks_next_block() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    write_ramp(&path, SR as u32, 10_000);
    let streamer = streamer_on(&path);
    let mut voice = live_voice(&streamer, 0.0);
    let (clock, ctx) = clock_at(SR);
    let mut first = fork(&voice, &ctx, SR);
    let left = render(&mut *first.node, &clock, 32);
    voice.set_speed(tutti_core::PlaybackRate::new(2.0));
    // A fork's controls are a snapshot: the 2x reaches a fork taken after it.
    let mut second = fork(&voice, &ctx, SR);
    let right = render(&mut *second.node, &clock, 32);
    for (k, &got) in left[0].iter().enumerate() {
        assert_eq!(got, value(k), "render frame {k}, at 1x");
    }
    for (k, &got) in right[0].iter().enumerate() {
        assert_eq!(got, value(64 + 2 * k), "render frame {}, at 2x", 32 + k);
    }
}

/// **A fork of a stream that is gone plays nothing, and says so**: the live
/// voice's channel restarted on another file, so the live voice is on a ring
/// the butler no longer feeds, and so is its fork — it must not pick up the
/// other file from the channel, and its render fails rather than pass the
/// silence off as the clip.
///
/// Mutation (run): `StreamOrigin::describe` not checking that the stream
/// ended → the fork plays the other file → fails. Mutation (run): the
/// `StreamGone` latch removed from `fork_copy` → no fault → fails.
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
    let [l, _] = render(&mut *copy.node, &clock, 4_096);
    assert!(
        l.iter().all(|&s| s == 0.0),
        "the fork played the channel's new file"
    );
    let fault = copy.fault().expect("the silence is a latched failure");
    assert!(fault.contains("ended"), "{fault}");
}

/// **A fork whose file cannot be read fails, naming the file.** The file is
/// removed after the stream started (the butler keeps its own handle), so the
/// fork, which re-opens it by its path, cannot: it renders silence and latches
/// the failure its render reports (the fork's health probe, which the
/// export reads).
///
/// Mutation (run): `OfflineRead::open` not latching an open failure → no
/// fault → fails.
#[test]
fn a_fork_whose_file_cannot_be_read_fails_naming_it() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("gone.wav");
    write_ramp(&path, SR as u32, 10_000);
    let streamer = streamer_on(&path);
    let voice = live_voice(&streamer, 0.0);
    std::fs::remove_file(&path).expect("removes");

    let (clock, ctx) = clock_at(SR);
    let mut copy = fork(&voice, &ctx, SR);
    assert!(copy.health.is_some(), "a fork has a probe");
    assert!(copy.fault().is_none(), "healthy before it renders");
    let [l, _] = render(&mut *copy.node, &clock, 1_024);
    assert!(l.iter().all(|&s| s == 0.0));
    let fault = copy.fault().expect("the unreadable file is a failure");
    assert!(fault.contains("gone.wav"), "names the file: {fault}");
}

/// **A fork rendered before it is prepared with a sample rate renders
/// nothing, and says so**, rather than read at a default rate and play off
/// pitch. (The graph prepares every fork before it renders; a fork source
/// used by hand may not.)
///
/// Mutation (run): the offline read falling back to 44.1 kHz when no rate
/// was given → it plays, with no fault → fails.
#[test]
fn a_fork_told_no_rate_fails_rather_than_guess() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    write_ramp(&path, SR as u32, 10_000);
    let streamer = streamer_on(&path);
    let voice = live_voice(&streamer, 0.0);
    let (clock, ctx) = clock_at(SR);
    let mut copy = fork_unprepared(voice.clone(), &ctx);
    let [l, _] = render(&mut *copy.node, &clock, 256);
    assert!(l.iter().all(|&s| s == 0.0));
    let fault = copy.fault().expect("no rate is a failure");
    assert!(fault.contains("sample rate"), "{fault}");
}

/// **A reversed fork plays the file backwards**, across its pages: frame `k`
/// of the clip is the file's frame `len - 1 - k`, exactly, over a file three
/// pages long. (Past the file's first frame is doc 013's follow-up S1, not
/// asserted.)
///
/// Mutation (run): the reverse mirror removed from `OfflineRead::read_into`
/// → plays forwards → fails.
#[test]
fn a_reversed_fork_plays_the_file_backwards() {
    const LEN: usize = 100_000;
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    write_ramp(&path, SR as u32, LEN);
    let streamer = streamer_on(&path);
    let mut voice = live_voice(&streamer, 0.0);
    voice.set_direction(tutti_sampler::Direction::Reverse);
    let (clock, ctx) = clock_at(SR);
    let mut copy = fork(&voice, &ctx, SR);
    let [l, _] = render(&mut *copy.node, &clock, LEN);
    for (k, &got) in l.iter().enumerate() {
        assert_eq!(got, value(LEN - 1 - k), "clip frame {k}");
    }
}

/// **A stretched fork reads its file at the stretcher's rate**, from the
/// seat as well as along it: at a read rate of 0.5 (a 2× stretch), frame `k`
/// of the clip reads file frame `k / 2`, across every chunk. The file is a
/// ramp, which the kernel reproduces between frames up to rounding.
///
/// Mutation (run): `.then(stretch)` dropped from the step
/// (`DiskVoice::step_rate`) → a file frame per render frame within each
/// piece → fails. Mutation (run): dropped from `DiskVoice::window_rate` (the
/// seat) → every piece seats at `k`, not `k / 2` → fails.
#[test]
fn a_stretched_fork_reads_at_the_stretchers_rate() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    write_ramp(&path, SR as u32, 20_000);
    let streamer = streamer_on(&path);
    let voice = live_voice(&streamer, 0.0);
    voice.set_stretch_rate(tutti_core::ReadRate(0.5));
    let (clock, ctx) = clock_at(SR);
    let mut copy = fork(&voice, &ctx, SR);
    let [l, _] = render(&mut *copy.node, &clock, 8_192);
    for (k, &got) in l.iter().enumerate().skip(4) {
        let want = (k as f32 / 2.0 + 1.0) * 1e-5;
        assert!(
            (got - want).abs() < 1e-6,
            "clip frame {k} read {got}, want {want}"
        );
    }
}

/// **A fork reads across its pages at a converted rate**: a 44.1 kHz file
/// three pages long, rendered at 48 kHz, reads file frame `n × 44.1 / 48` on
/// every render frame `n`, through each page boundary, to its last frame.
///
/// Mutation (run): `Pages::load` skipping the decoder's seek (the decoder
/// reads on from where the last page ended, `PAGE_LEAD` frames off) → every
/// page after the first reads the wrong frames → fails.
#[test]
fn a_fork_reads_across_pages_at_a_converted_rate() {
    const LEN: usize = 100_000;
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp_44k1.wav");
    write_ramp(&path, 44_100, LEN);
    let streamer = streamer_on(&path);
    let voice = live_voice(&streamer, 0.0);
    let (clock, ctx) = clock_at(SR);
    let mut copy = fork(&voice, &ctx, SR);
    let frames = (LEN as f64 * SR / 44_100.0) as usize - 8;
    let [l, _] = render(&mut *copy.node, &clock, frames);
    for (n, &got) in l.iter().enumerate().skip(4) {
        let at = n as f64 * 44_100.0 / SR;
        let want = ((at + 1.0) * 1e-5) as f32;
        assert!(
            (got - want).abs() < 1e-6,
            "render frame {n} read {got}, want {want} (file frame {at:.3})"
        );
    }
}

/// **A fork follows its render's timeline when it loops**: on a render clock
/// looping beats `[0, 1.01)` (24 240 frames at 120 BPM), a clip at beat 0
/// starts again from its first frame at every wrap, **on the wrap's frame**
/// — the read re-seats where the render's `Env` wraps, rather than count on
/// from where it was. The loop is not a whole number of blocks, so the
/// wraps fall inside blocks (the first 48 frames into one).
///
/// Mutation (run): `Runs` not cutting a run at a wrap (`wrap_offset`
/// answering `None`) → it plays on past the wrap → fails. (With a loop of
/// 24 000 frames, a whole number of blocks, every wrap fell on a block's
/// first frame and this mutation passed.)
#[test]
fn a_fork_follows_a_looping_render_timeline() {
    const LOOP: usize = 24_240;
    assert_ne!(LOOP % BLOCK, 0, "a wrap inside a block");
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    write_ramp(&path, SR as u32, 60_000);
    let streamer = streamer_on(&path);
    let voice = live_voice(&streamer, 0.0);
    let (clock, ctx) = looping_clock_at(SR, LoopRange::new(Beat(0.0), Beat(1.01)));
    let mut copy = fork(&voice, &ctx, SR);
    let [l, _] = render(&mut *copy.node, &clock, 60_000);
    for (k, &got) in l.iter().enumerate() {
        assert_eq!(got, value(k % LOOP), "render frame {k}");
    }
}

/// **A fork is bit-identical to the memory tier**: the same file in memory
/// and on disk, placed at beat 1, render the same samples, bit for bit, at
/// every frame of every block — the two tiers read through one placement,
/// one step, one loop and one kernel.
///
/// - **At 1.5× varispeed**, on fractional positions.
/// - **A 24 kHz file at 48 kHz** (a non-unity conversion): the memory tier
///   used to step by varispeed alone within a chunk (doc 013's placed
///   `MemorySource` rate follow-up).
/// - **A crossfaded loop at 1.5×**, through the fade, the seam and many
///   wraps, on fractional positions: the fade toward the lead-in (S3) and the
///   taps through the seam (N2). A placed memory voice used to ignore its
///   loop.
/// - **The same loop, paged**: the rows above fork onto the butler's cached
///   wave (`Open::Resident`, the butler decodes the file whole to capture a
///   loop's fade), which reads through the very `read_looped_frame` the
///   memory tier does — so there the two sides share their loop code, and a
///   bug in it passes. This row evicts the wave from the butler's cache
///   (a one-entry cache, and a second stream whose loop loads its own file)
///   so the fork decodes pages (`Pages::read_looped_into`), an independent
///   fetch-and-blend. A hard loop at 1.5× runs the paged seam taps too.
/// - **Reversed** (the memory voice in a `VoiceNode`, where direction lives),
///   past the file's first frame into silence (S1).
///
/// **What this does not pin**: anything the two tiers share — `LoopSpan`'s
/// fade weight, lead-in and tap layout, the seat, the kernel. Agreement
/// there is by construction. Those are pinned against hand-computed
/// oracles: `loop_span`'s own tests, `a_fork_loops_as_the_stream_is_looped_when_it_is_taken`
/// (the fork) and `memory_source`'s `a_crossfaded_loop_plays_the_hand_computed_frames`
/// (the memory tier).
///
/// Mutation (run): the fork's step not composing varispeed (`speed` →
/// `PlaybackRate::UNITY` in `DiskVoice::step_rate`) → it parts from memory
/// inside the first block → fails. Mutation (run): the memory tier's step
/// `read_rate` → `window_rate` (`placed_positions`) → the 24 kHz case parts
/// inside the clip's first block → fails. Mutation (run): `MemorySource::read_placed_into`
/// ignoring the loop → the loop case parts at the first wrap → fails.
/// Mutation (run): the memory tier's reverse holding frame 0 → the reverse
/// case parts past the start → fails. Mutation (run): the paged blend weight
/// alone changed (`blend(.., 1.0 - t)` in `Pages::read_looped_into`) → the
/// paged row parts in the first fade → fails (the resident row does not see
/// it, which is why the paged row exists).
#[test]
fn a_fork_matches_the_memory_tier_bit_for_bit() {
    const LEN: usize = 40_000;
    struct Case {
        what: &'static str,
        file_rate: u32,
        speed: f32,
        loop_: LoopSetting,
        reverse: bool,
        paged: bool,
    }
    let cases = [
        Case {
            what: "1.5x varispeed",
            file_rate: SR as u32,
            speed: 1.5,
            loop_: LoopSetting::Off,
            reverse: false,
            paged: false,
        },
        Case {
            what: "a 24 kHz file at 48 kHz",
            file_rate: 24_000,
            speed: 1.0,
            loop_: LoopSetting::Off,
            reverse: false,
            paged: false,
        },
        Case {
            what: "a crossfaded loop at 1.5x",
            file_rate: SR as u32,
            speed: 1.5,
            loop_: LoopSetting::On {
                start: SamplePosition(3_000.0),
                end: SamplePosition(7_001.0),
                crossfade_frames: 700,
            },
            reverse: false,
            paged: false,
        },
        Case {
            what: "a crossfaded loop at 1.5x, paged",
            file_rate: SR as u32,
            speed: 1.5,
            loop_: LoopSetting::On {
                start: SamplePosition(3_000.0),
                end: SamplePosition(7_001.0),
                crossfade_frames: 700,
            },
            reverse: false,
            paged: true,
        },
        Case {
            what: "a hard loop at 1.5x, paged",
            file_rate: SR as u32,
            speed: 1.5,
            loop_: LoopSetting::On {
                start: SamplePosition(3_000.0),
                end: SamplePosition(7_001.0),
                crossfade_frames: 0,
            },
            reverse: false,
            paged: true,
        },
        Case {
            what: "reversed",
            file_rate: SR as u32,
            speed: 1.0,
            loop_: LoopSetting::Off,
            reverse: true,
            paged: false,
        },
    ];
    for case in cases {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("ramp.wav");
        write_ramp(&path, case.file_rate, LEN);
        let mut config = tutti_sampler::DiskStreamerConfig::default();
        if case.paged {
            // One wave resident at a time, so the next file the butler loads
            // evicts this one.
            config.buffer_config.cache_max_entries = 1;
        }
        let mut streamer = streamer_with(&path, config);
        if case.loop_ != LoopSetting::Off {
            streamer
                .commands()
                .send(Command::Loop {
                    channel_index: 0,
                    setting: case.loop_,
                })
                .expect("the butler is alive");
            let _ = streamer.step_until_settled(1_000);
        }
        if case.paged {
            // A second stream whose crossfaded loop makes the butler load its
            // own file whole — evicting the first, which the fork then pages.
            let other = dir.path().join("other.wav");
            write_ramp(&other, SR as u32, 1_000);
            let commands = streamer.commands();
            commands
                .send(Command::Stream {
                    channel_index: 1,
                    file_path: other,
                    offset: SamplePosition(0.0),
                })
                .expect("the butler is alive");
            let _ = streamer.step_until_settled(1_000);
            commands
                .send(Command::Loop {
                    channel_index: 1,
                    setting: LoopSetting::On {
                        start: SamplePosition(500.0),
                        end: SamplePosition(900.0),
                        crossfade_frames: 100,
                    },
                })
                .expect("the butler is alive");
            let _ = streamer.step_until_settled(1_000);
        }
        let mut voice = live_voice(&streamer, 1.0);
        voice.set_speed(tutti_core::PlaybackRate::new(case.speed));
        if case.reverse {
            voice.set_direction(tutti_sampler::Direction::Reverse);
        }
        let mut wave = tutti_io::Wave::new(2, case.file_rate as f64);
        for i in 0..LEN {
            wave.push_frame(&[value(i), -value(i)]);
        }
        let (clock, ctx) = clock_at(SR);
        let mut source = tutti_sampler::MemorySource::placed(Arc::new(wave), Beat(1.0), None);
        source.set_speed(tutti_core::PlaybackRate::new(case.speed));
        source.set_loop_setting(case.loop_);
        let mut memory: Box<dyn Node> = if case.reverse {
            Box::new(VoiceNode::with_channels(
                Voice {
                    source: VoiceSource::Memory(source),
                    play: Playback {
                        direction: tutti_sampler::Direction::Reverse,
                        ..Playback::default()
                    },
                    channel_index: None,
                },
                2usize,
            ))
        } else {
            Box::new(source)
        };
        memory.prepare(&Prepare::new(SampleRate(SR), Samples(BLOCK)));
        let mut copy = fork(&voice, &ctx, SR);

        let mut sounded = 0usize;
        // Beat 1, then two file lengths: past the end of every case's file.
        for block in 0..(24_000 + 2 * LEN) / BLOCK {
            let env = clock.env(BLOCK);
            let a = contract::drive_in(&mut *copy.node, &env, &[], &[], &[]).audio;
            let b = contract::drive_in(&mut *memory, &env, &[], &[], &[]).audio;
            for c in 0..2 {
                for i in 0..BLOCK {
                    let (x, y) = (a[c][i], b[c][i]);
                    assert_eq!(
                        x.to_bits(),
                        y.to_bits(),
                        "{}: channel {c}, frame {}: disk {x} memory {y}",
                        case.what,
                        block * BLOCK + i
                    );
                    sounded += usize::from(x != 0.0);
                }
            }
            clock.advance(BLOCK);
        }
        assert!(sounded >= LEN, "{}: the two agreed on silence", case.what);
    }
}
