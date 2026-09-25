//! A live disk voice on a looped stream (doc 013, "The live disk loop").
//!
//! The butler writes a looped stream into the ring as the sequence
//! `LoopSpan` defines (`butler::loops`), and the voice consumes the ring with
//! no loop logic of its own. These tests drive a live `DiskVoice` from a
//! hand-stepped butler (`DiskStreamer::manual`, one cycle per block, as the
//! butler thread stands to the audio callback) and compare what it plays with
//! the memory tier's `MemorySource` on the same loop.
//!
//! # Lining the two tiers up
//!
//! The live reader interpolates from a four-frame history it fills as it pops
//! the ring, so at read rate `r` its output frame `j` (counted from a fresh
//! history) sits at file position `r (j + 1) - 3`, where a placed memory voice's
//! frame `m` sits at `r m`. Where `3 / r` is whole (`r` = 1, 1.5, 0.75) that is
//! the memory tier's frame `j + 1 - 3 / r`, and the two are compared frame for
//! frame, bit for bit. At a converted rate (a 44.1 kHz file at 48 kHz) the
//! offset is a fraction of a frame; there the memory voice is placed that
//! fraction later on the clock, and the comparison takes a tolerance: the live
//! reader accumulates its step (`fractional_pos += rate`) where the memory
//! tier multiplies it from a seat, and with a non-dyadic ratio the two part in
//! the last bits.
//!
//! # The entry seek
//!
//! A live voice's gate asks the butler to seek on its first frame in the
//! window. The warm-up holds the clock while that settles (`warm_up`), so the
//! first compared block starts at the clock's beat 0 from a clean history.

use std::path::Path;
use std::sync::Arc;

use tutti_core::transport::{OfflineTimeline, OfflineTimelineConfig};
use tutti_core::{AudioUnit, Beat, Bpm, BufferVec, PlaybackRate, SamplePosition, SampleRate};

use super::DiskVoice;
use crate::{Command, DiskStreamer, DiskStreamerConfig, LoopSetting, MemorySource};

const SR: f64 = 48_000.0;
const BLOCK: usize = 64;
/// Length of the ramp files, in frames.
const LEN: usize = 40_000;

/// Frame `i` of every ramp file: distinct per frame, and exactly what an f32
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

/// The same ramp, resident, for the memory tier.
fn ramp_wave(rate: u32, frames: usize) -> Arc<tutti_io::Wave> {
    let mut wave = tutti_io::Wave::new(2, rate as f64);
    for i in 0..frames {
        wave.push_frame(&[value(i), -value(i)]);
    }
    Arc::new(wave)
}

/// The period of [`write_sine`]'s sine, in frames.
const PERIOD: usize = 100;

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

/// A 120 BPM clock at [`SR`] from `beat` (two beats a second, 24 000 frames a
/// beat).
fn clock(beat: f64) -> Arc<OfflineTimeline> {
    Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        start_beat: Beat(beat),
        tempo: Bpm(120.0),
        sample_rate: SampleRate(SR),
        loop_range: None,
    }))
}

fn loop_on(start: f64, end: f64, fade: usize) -> LoopSetting {
    LoopSetting::On {
        start: SamplePosition(start),
        end: SamplePosition(end),
        crossfade_frames: fade,
    }
}

/// A live voice on a hand-stepped butler, and the clock it is placed on.
struct Live {
    streamer: DiskStreamer,
    voice: DiskVoice,
    clock: Arc<OfflineTimeline>,
    output: BufferVec,
    /// The reader's fraction as the first sounding block began (see
    /// [`warm_up`](Self::warm_up)).
    start_fraction: f64,
}

/// How a [`Live`] voice is set up, besides its file.
#[derive(Clone, Copy)]
struct Setup {
    loop_: LoopSetting,
    speed: f32,
    /// Where the clock starts; the voice is placed at beat 0, so its entry
    /// seek lands this far into the clip.
    beat: f64,
    reverse: bool,
    /// The channel's PDC preroll, in frames.
    preroll: usize,
}

impl Setup {
    fn looped(loop_: LoopSetting, speed: f32) -> Self {
        Self {
            loop_,
            speed,
            beat: 0.0,
            reverse: false,
            preroll: 0,
        }
    }
}

impl Live {
    /// [`with`](Self::with) a plain [`Setup::looped`].
    fn new(path: &Path, loop_: LoopSetting, speed: f32) -> (Self, [Vec<f32>; 2]) {
        Self::with(path, Setup::looped(loop_, speed))
    }

    /// Stream `path` on channel 0 with the setup's loop set (and its PDC
    /// preroll published before the stream starts), and take a live voice
    /// placed at beat 0, warmed up (see [`warm_up`](Self::warm_up)). The seek
    /// crossfade is off, so the entry seek is a plain reposition.
    fn with(path: &Path, setup: Setup) -> (Self, [Vec<f32>; 2]) {
        let loop_ = setup.loop_;
        let mut config = DiskStreamerConfig::default();
        config.buffer_config.seek_crossfade_frames = 0;
        if setup.preroll > 0 {
            config.pdc = Some(Arc::new(tutti_core::RtPublish::new(vec![
                tutti_core::Samples(setup.preroll),
            ])));
        }
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
        if loop_ != LoopSetting::Off {
            streamer
                .commands()
                .send(Command::Loop {
                    channel_index: 0,
                    setting: loop_,
                })
                .expect("the butler is alive");
            assert!(
                streamer.step_until_settled(1_000) < 1_000,
                "the ring primes"
            );
        }
        let clock = clock(setup.beat);
        let mut voice = streamer
            .status()
            .take_disk_voice(0, clock.clone(), Beat(0.0), None)
            .expect("the link is installed");
        voice.set_sample_rate(SampleRate(SR));
        voice.set_speed(PlaybackRate::new(setup.speed));
        if setup.reverse {
            voice.set_direction(crate::Direction::Reverse);
        }
        let mut live = Self {
            streamer,
            voice,
            clock,
            output: BufferVec::new(2),
            start_fraction: 0.0,
        };
        let first = live.warm_up();
        (live, first)
    }

    /// One block of the voice, both channels.
    fn block(&mut self) -> [Vec<f32>; 2] {
        let input = BufferVec::new(0);
        self.voice
            .process(BLOCK, &input.buffer_ref(), &mut self.output.buffer_mut());
        let out = self.output.buffer_ref();
        [0, 1].map(|c| (0..BLOCK).map(|i| out.at_f32(c, i)).collect())
    }

    /// Settle the entry seek with the clock held at beat 0: the first block
    /// asks for it (and plays the primed ring, discarded), then a butler cycle
    /// and a block at a time until the voice sounds. That block is the first
    /// of the sequence from frame 0, played from a zero history (the flush
    /// zeroed it, and an underrun block keeps it zero) — but not always from a
    /// zero fraction: an underrun block still steps the reader's fraction, by
    /// `64 r` mod 1, which is 0 at the rates whose `3 / r` is whole and not at
    /// a converted one. [`start_fraction`](Self::start_fraction) keeps it.
    fn warm_up(&mut self) -> [Vec<f32>; 2] {
        let _ = self.block();
        for _ in 0..8 {
            let _ = self.streamer.step_once();
            let applied = self.voice.inner.applied_reset_epoch == self.resets();
            let fraction = self.voice.inner.fractional_pos;
            let out = self.block();
            if out[0].iter().any(|&s| s != 0.0) {
                assert!(applied, "the sounding block applied a flush itself");
                self.start_fraction = fraction;
                return out;
            }
        }
        panic!("the voice never sounded after its entry seek");
    }

    /// Render `blocks` more blocks after `first`, moving the clock after each
    /// block and running one butler cycle before the next.
    fn render(&mut self, first: [Vec<f32>; 2], blocks: usize) -> [Vec<f32>; 2] {
        let mut planes = first;
        for _ in 0..blocks {
            self.clock.advance(BLOCK);
            let _ = self.streamer.step_once();
            let [l, r] = self.block();
            planes[0].extend(l);
            planes[1].extend(r);
        }
        planes
    }

    /// The ring resets the butler has asked this voice's reader for.
    fn resets(&self) -> u64 {
        self.voice.shared_state.reset_epoch()
    }
}

/// A placed memory voice on the same loop, at `speed`, its window starting
/// `start` beats in, rendered for `frames` from beat `from`.
fn memory(
    wave: Arc<tutti_io::Wave>,
    loop_: LoopSetting,
    speed: f32,
    start: f64,
    from: f64,
    frames: usize,
) -> [Vec<f32>; 2] {
    let clock = clock(from);
    let mut source = MemorySource::with_transport(wave, clock.clone(), Beat(start), None);
    source.set_speed(PlaybackRate::new(speed));
    source.set_loop_setting(loop_);
    source.set_sample_rate(SampleRate(SR));
    let input = BufferVec::new(0);
    let mut output = BufferVec::new(2);
    let mut planes = [Vec::new(), Vec::new()];
    for _ in 0..frames.div_ceil(BLOCK) {
        source.process(BLOCK, &input.buffer_ref(), &mut output.buffer_mut());
        let out = output.buffer_ref();
        for (c, plane) in planes.iter_mut().enumerate() {
            plane.extend((0..BLOCK).map(|i| out.at_f32(c, i)));
        }
        clock.advance(BLOCK);
    }
    planes
}

/// **A live disk voice on a looped stream plays the memory tier's loop, bit
/// for bit**, across many wraps: crossfaded, hard, and from frame 0 (the fade
/// into the loop's head, `LoopSpan`'s fallback), at unity, 1.5× and 0.75×.
/// The butler never flushes the ring while it loops: the reset count after
/// the warm-up holds through every wrap.
///
/// This is the live half of doc 013's "The live disk loop": before the fix
/// the butler flushed the ring on every cycle once the reader's consumed-frame
/// count passed the loop's end, and the voice fell silent after 576 frames.
///
/// Mutation (run): this module against `origin/main`'s butler (the `AtEnd`
/// flush, the RT loop crossfade) → the entry seek's flush adds the whole ring
/// to the reader's consumed count, which is then past every loop's end, so
/// every cycle flushes (the reset count climbs by one a cycle) and the voice
/// never sounds → every test here fails. Mutation (run): the blend dropped
/// from `fill_sequence` → the crossfaded rows part in the first fade → fails.
/// Mutation (run): `place_frame` wrapping to `start` rather than `resume` →
/// the row from frame 0 parts at its first wrap → fails. Mutation (run): the
/// fade's lead-in captured from `resume` rather than before it → fails.
/// Mutation (run): the refill's run not cut at the loop's end → fails.
#[test]
fn a_live_looped_voice_plays_the_memory_tiers_loop_bit_for_bit() {
    struct Case {
        what: &'static str,
        speed: f32,
        /// `3 / speed - 1`: how many output frames the live reader's history
        /// runs behind the memory tier (module docs).
        lag: usize,
        loop_: LoopSetting,
    }
    let cases = [
        Case {
            what: "a crossfaded loop",
            speed: 1.0,
            lag: 2,
            loop_: loop_on(3_000.0, 7_001.0, 700),
        },
        Case {
            what: "a crossfaded loop at 1.5x",
            speed: 1.5,
            lag: 1,
            loop_: loop_on(3_000.0, 7_001.0, 700),
        },
        Case {
            what: "a hard loop at 1.5x",
            speed: 1.5,
            lag: 1,
            loop_: loop_on(3_000.0, 7_001.0, 0),
        },
        Case {
            what: "a crossfaded loop at 0.75x",
            speed: 0.75,
            lag: 3,
            loop_: loop_on(3_000.0, 5_001.0, 500),
        },
        Case {
            what: "a crossfaded loop from frame 0",
            speed: 1.0,
            lag: 2,
            loop_: loop_on(0.0, 5_001.0, 700),
        },
    ];
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    write_ramp(&path, SR as u32, LEN);
    for case in cases {
        let (mut live, first) = Live::new(&path, case.loop_, case.speed);
        assert_eq!(
            live.start_fraction, 0.0,
            "{}: a whole-frame start",
            case.what
        );
        let resets = live.resets();
        const BLOCKS: usize = 900;
        let [l, r] = live.render(first, BLOCKS);
        assert_eq!(
            live.resets(),
            resets,
            "{}: the butler flushed the ring while it looped",
            case.what
        );
        let memory = memory(
            ramp_wave(SR as u32, LEN),
            case.loop_,
            case.speed,
            0.0,
            0.0,
            l.len(),
        );
        let LoopSetting::On { end, .. } = case.loop_ else {
            unreachable!()
        };
        let wraps = (l.len() as f64 * case.speed as f64 - end.get()) / 2_000.0;
        assert!(wraps > 5.0, "{}: too few wraps to say anything", case.what);
        for (c, live) in [l, r].iter().enumerate() {
            for m in 4..live.len() - case.lag {
                let (x, y) = (live[m + case.lag], memory[c][m]);
                assert_eq!(
                    x.to_bits(),
                    y.to_bits(),
                    "{}: channel {c}, memory frame {m}: live {x} memory {y}",
                    case.what
                );
            }
        }
    }
}

/// **At a converted rate the live loop still is the memory tier's**, to a
/// tolerance: a 44.1 kHz file at 48 kHz, crossfaded, across many wraps. The
/// memory voice is placed the live reader's lag later (a fraction of a frame
/// here, `3 / r - 1` output frames), and the two agree within 1e-6 (the ramp
/// peaks near 0.07; the live reader accumulates its step where the memory
/// tier multiplies it, module docs).
///
/// Mutation (run): the loop taken in session frames rather than the file's
/// (`RingLoop::capture` scaling its points by 48 000 / 44 100) → parts at the
/// first fade → fails. Mutation (run): the blend dropped → fails.
#[test]
fn a_live_looped_voice_at_a_converted_rate_matches_the_memory_tier() {
    const FILE_RATE: u32 = 44_100;
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    write_ramp(&path, FILE_RATE, LEN);
    let loop_ = loop_on(3_000.0, 7_001.0, 700);
    let (mut live, first) = Live::new(&path, loop_, 1.0);
    let resets = live.resets();
    let [l, _] = live.render(first, 900);
    assert_eq!(
        live.resets(),
        resets,
        "the butler flushed the ring while it looped"
    );
    let rate = FILE_RATE as f64 / SR;
    // The live frame `j` sits at `r (j + 1) - 3 + f0` (`f0` the reader's
    // starting fraction); the memory voice placed `lag` frames late at `r (m -
    // lag)`: equal at `m = j` for `lag = (3 - f0) / r - 1`.
    let lag_frames = (3.0 - live.start_fraction) / rate - 1.0;
    // Beats at 120 BPM: two a second.
    let lag_beats = lag_frames / SR * 2.0;
    let [memory, _] = memory(
        ramp_wave(FILE_RATE, LEN),
        loop_,
        1.0,
        lag_beats,
        0.0,
        l.len(),
    );
    // From the memory voice's second block: its first is gated whole at beat
    // 0, before its window.
    let mut worst = 0.0f32;
    for m in BLOCK..l.len() {
        worst = worst.max((l[m] - memory[m]).abs());
    }
    assert!(
        worst < 1e-6,
        "the live loop parts from the memory tier by {worst}"
    );
    assert!(
        l.len() as f64 * rate > 7_001.0 + 5.0 * 4_001.0,
        "too few wraps to say anything"
    );
}

/// **A live crossfaded loop is continuous at its wrap** (doc 013's S3, the
/// live tier): on a sine whose loop points click when cut hard — the loop
/// starts on a rising zero crossing and ends a quarter period later in the
/// cycle — no step in the output is larger than the sine's own (`2 sin(π /
/// PERIOD)`), round the loop three times; and a loop from frame 0 `[0,
/// 2281)`, whose fade goes into its head. The hard loops are asserted to click,
/// so the loop points have teeth.
///
/// Mutation (run): the blend dropped from `fill_sequence` → the crossfaded
/// loops click → fails. Mutation (run): the fade's lead-in captured from
/// `resume` rather than before it (the old head replay) → a step far above
/// the sine's own at the wrap → fails.
#[test]
fn a_live_crossfaded_loop_is_continuous_at_its_wrap() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("sine.wav");
    write_sine(&path, 4_000);
    let own = (2.0 * (std::f64::consts::PI / PERIOD as f64).sin()) as f32 + 1e-5;
    for (start, end, fade, clicks) in [
        (1_000.0, 3_025.0, 0usize, true),
        (1_000.0, 3_025.0, 256, false),
        (0.0, 2_281.0, 0, true),
        (0.0, 2_281.0, 256, false),
    ] {
        let (mut live, first) = Live::new(&path, loop_on(start, end, fade), 1.0);
        let [l, _] = live.render(first, (3_025 + 3 * 2_025) / BLOCK);
        let at_loop = format!("[{start}, {end}) fade {fade}");
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
                "{at_loop}: a step of {step} at frame {at}, larger than the sine's own {own}"
            );
        }
    }
}

/// **A seek past the loop's end, and a PDC preroll, land on the loop where
/// the memory tier plays the same position.** The stream's position counts the
/// file straight on and the refill places it on the loop, so a voice entering
/// its clip at beat 1 (24 000 frames in, well past a loop `[3000, 7001)`) plays
/// what a memory voice plays there — and with a 12 000-frame preroll on its
/// channel, what the memory voice plays half a beat earlier. Bit for bit.
///
/// Mutation (run): the refill writing from the straight position unplaced
/// (`place_frame` → identity) → it reads the file at 24 000, not the loop →
/// fails. Mutation (run): the seek's `pdc_preroll` not applied → the preroll
/// row plays the memory voice's beat-1 frames → fails.
#[test]
fn a_seek_and_a_preroll_land_on_the_loop_where_the_memory_tier_plays() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    write_ramp(&path, SR as u32, LEN);
    let loop_ = loop_on(3_000.0, 7_001.0, 700);
    for (preroll, memory_from) in [(0usize, 1.0), (12_000, 0.5)] {
        let (mut live, first) = Live::with(
            &path,
            Setup {
                beat: 1.0,
                preroll,
                ..Setup::looped(loop_, 1.0)
            },
        );
        let resets = live.resets();
        let [l, _] = live.render(first, 300);
        assert_eq!(
            live.resets(),
            resets,
            "preroll {preroll}: a flush while looping"
        );
        let [memory, _] = memory(
            ramp_wave(SR as u32, LEN),
            loop_,
            1.0,
            0.0,
            memory_from,
            l.len(),
        );
        for m in 4..l.len() - 2 {
            assert_eq!(
                l[m + 2].to_bits(),
                memory[m].to_bits(),
                "preroll {preroll}: memory frame {m}: live {} memory {}",
                l[m + 2],
                memory[m]
            );
        }
    }
}

/// **Reverse ignores the loop, live as on every tier**: a reversed voice
/// entering at beat 1 plays the file backwards from frame 24 000 straight
/// through a loop `[3000, 7001)` and on to frame 0, exactly as the same voice
/// with no loop; and a loop changed while it plays reversed is only stored —
/// no flush, nothing heard.
///
/// Mutation (run): the reverse refill placing its cursor on the loop (a
/// first cut of this fix, for a stream turned round after it had looped) →
/// the entry at 24 000 starts from the loop's frame 3 995 → fails. Mutation
/// (run): `handle_set_stream_loop` repositioning a reversed stream → the
/// mid-play change flushes → fails.
#[test]
fn a_reversed_voice_ignores_its_loop() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    write_ramp(&path, SR as u32, LEN);
    let reversed = |loop_| Setup {
        beat: 1.0,
        reverse: true,
        ..Setup::looped(loop_, 1.0)
    };
    let (mut plain, first) = Live::with(&path, reversed(LoopSetting::Off));
    let [want, _] = plain.render(first, 420);
    let (mut looped, first) = Live::with(&path, reversed(loop_on(3_000.0, 7_001.0, 700)));
    let resets = looped.resets();
    let [mut got, _] = looped.render(first, 20);
    looped
        .streamer
        .commands()
        .send(Command::Loop {
            channel_index: 0,
            setting: loop_on(10_000.0, 12_000.0, 0),
        })
        .expect("the butler is alive");
    let [rest, _] = looped.render([Vec::new(), Vec::new()], 400);
    got.extend(rest);
    assert_eq!(
        looped.resets(),
        resets,
        "a loop change flushed a reversed stream"
    );
    assert_eq!(got.len(), want.len());
    // The file backwards from its frame 23 999, through the loop, to frame 0.
    for (j, &v) in want.iter().enumerate().take(24_002).skip(2) {
        assert_eq!(v, value(23_999 - (j - 2)), "reversed, frame {j}");
    }
    for (k, (&g, &w)) in got.iter().zip(&want).enumerate() {
        assert_eq!(g.to_bits(), w.to_bits(), "frame {k}: looped {g} plain {w}");
    }
}

/// **A loop change takes effect at the butler's next cycle, at the frame the
/// reader reads next**, not once the ring drains (it may hold 30 s of the old
/// loop). The butler flushes the ring at its head — the reader's straight
/// position, frames played plus the four it had fetched ahead — and refills
/// from that position under the new loop, where the memory tier (whose loop
/// change is a store) places the same position; exactly one flush. It moves as
/// a seek does: with a hand-stepped butler the reader finds the ring empty for
/// the block after the flush (the refill lands a cycle later) and restarts
/// from a zero history (two more zero frames), so it resumes one block
/// behind the clock it left, which the gate lets stand as it lets any
/// reposition's drift under `SEEK_EPSILON_SAMPLES` stand. (With the seek
/// crossfade on, that block is covered by the crossfade; doc 013's follow-ups
/// record what that crossfade still gets wrong.)
///
/// Here: a hard loop `[1000, 3000)`, changed to `[500, 1500)` 81 blocks in
/// (straight 5 188, the old loop's frame 1 188, the new one's 1 188 too — the
/// point of the change is where it goes next: to 1 499, then round to 500).
///
/// Mutation (run): the change only stored (no reposition) → the ring plays
/// on through the old loop's frames → fails. Mutation (run): the reposition
/// at the writer's cursor rather than the head → resumes thousands of frames
/// on → fails.
#[test]
fn a_loop_change_takes_effect_at_the_readers_next_frame() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    write_ramp(&path, SR as u32, LEN);
    let (mut live, first) = Live::new(&path, loop_on(1_000.0, 3_000.0, 0), 1.0);
    let resets = live.resets();
    let [before, _] = live.render(first, 80);
    let old = |s: usize| {
        if s < 3_000 {
            s
        } else {
            1_000 + (s - 3_000) % 2_000
        }
    };
    for (j, &v) in before.iter().enumerate().skip(2) {
        assert_eq!(v, value(old(j - 2)), "before the change, frame {j}");
    }
    live.streamer
        .commands()
        .send(Command::Loop {
            channel_index: 0,
            setting: loop_on(500.0, 1_500.0, 0),
        })
        .expect("the butler is alive");
    let [after, _] = live.render([Vec::new(), Vec::new()], 60);
    assert_eq!(live.resets(), resets + 1, "one flush, for the change");
    assert!(
        after[..BLOCK + 2].iter().all(|&s| s == 0.0),
        "the block after the flush finds the ring empty"
    );
    let head = before.len() + 4;
    let new = |s: usize| {
        if s < 1_500 {
            s
        } else {
            500 + (s - 1_500) % 1_000
        }
    };
    let mut wrapped = 0;
    for (i, &v) in after[BLOCK + 2..].iter().enumerate() {
        assert_eq!(
            v,
            value(new(head + i)),
            "after the change, frame {i} of the refill (straight {})",
            head + i
        );
        wrapped += usize::from(new(head + i) == 500);
    }
    assert!(wrapped >= 2, "the new loop wrapped {wrapped} times");
}
