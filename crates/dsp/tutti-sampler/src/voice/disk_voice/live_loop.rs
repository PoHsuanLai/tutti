//! A live disk voice against the memory tier, through loops and repositions
//! (doc 013, "The live disk loop" and "The live disk reposition").
//!
//! The butler writes a stream into a ring indexed by straight position — the
//! loop's sequence, the file mirrored in reverse — and the voice seats on the
//! clock as the memory tier does and reads its taps there by position
//! (`live_read`). These tests drive a live `DiskVoice` from a hand-stepped
//! butler (`DiskStreamer::manual`, one cycle per block, as the butler thread
//! stands to the audio callback) and compare it with a placed `MemorySource`
//! on its own copy of the clock, given the same edits at the same blocks.
//!
//! **Bit for bit, from the first frame, at any rate.** Both read the same
//! positions (the same gate and seat) through the same tap layout and kernel,
//! from the same frames, so there is no lag to line up and no tolerance to
//! take — a 44.1 kHz file at 48 kHz included. Where the two differ is only
//! where a jump or an edit lands: the memory tier cuts there, the live voice
//! crossfades there (from its copy of the old continuation, or the butler's
//! record of the old loop) over the ring's fade length. The tests allow that
//! window and nothing else, and count the frames the live voice could not
//! read (underruns): a reposition costs none.

use std::path::Path;
use std::sync::Arc;

use tutti_core::{AudioUnit, Beat, Bpm, BufferVec, PlaybackRate, SamplePosition, SampleRate};

use super::DiskVoice;
use crate::test_transport::MockTransport;
use crate::{Command, DiskStreamer, DiskStreamerConfig, LoopSetting, MemorySource};

const SR: f64 = 48_000.0;
const BLOCK: usize = 64;
/// Length of the ramp files, in frames.
const LEN: usize = 40_000;
/// The butler's default seek crossfade, in output frames.
const FADE: usize = 512;

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

fn loop_on(start: f64, end: f64, fade: usize) -> LoopSetting {
    LoopSetting::On {
        start: SamplePosition(start),
        end: SamplePosition(end),
        crossfade_frames: fade,
    }
}

/// Beats at 120 BPM of `frames` at [`SR`].
fn beats(frames: f64) -> f64 {
    frames / SR * 2.0
}

/// How a [`Live`] voice is set up, besides its file.
#[derive(Clone, Copy)]
struct Setup {
    loop_: LoopSetting,
    speed: f32,
    /// Where the clock starts (the voice is placed at beat 0); the stream is
    /// started there, so the ring holds it before the first block.
    beat: f64,
    /// The channel's PDC preroll, in frames.
    preroll: usize,
    /// Other streams on channels 1.., so the butler refills in parallel.
    neighbours: usize,
}

impl Setup {
    fn looped(loop_: LoopSetting, speed: f32) -> Self {
        Self {
            loop_,
            speed,
            beat: 0.0,
            preroll: 0,
            neighbours: 0,
        }
    }
}

/// A live voice on a hand-stepped butler, and the clock it is placed on.
struct Live {
    streamer: DiskStreamer,
    voice: DiskVoice,
    clock: Arc<MockTransport>,
    output: BufferVec,
}

impl Live {
    fn new(path: &Path, loop_: LoopSetting, speed: f32) -> Self {
        Self::with(path, Setup::looped(loop_, speed))
    }

    /// Stream `path` on channel 0 from where the clock starts, with the
    /// setup's loop and preroll, under the butler's default config (its seek
    /// crossfade included), and take a live voice placed at beat 0.
    fn with(path: &Path, setup: Setup) -> Self {
        let mut config = DiskStreamerConfig::default();
        if setup.preroll > 0 {
            config.pdc = Some(Arc::new(tutti_core::RtPublish::new(vec![
                tutti_core::Samples(setup.preroll),
            ])));
        }
        let mut streamer = DiskStreamer::manual(SampleRate(SR), config).expect("builds");
        for channel in 0..=setup.neighbours {
            streamer
                .commands()
                .send(Command::Stream {
                    channel_index: channel,
                    file_path: path.to_path_buf(),
                    offset: SamplePosition(setup.beat / 2.0 * SR),
                })
                .expect("the butler is alive");
        }
        assert!(
            streamer.step_until_settled(1_000) < 1_000,
            "the ring primes"
        );
        if setup.loop_ != LoopSetting::Off {
            streamer
                .commands()
                .send(Command::Loop {
                    channel_index: 0,
                    setting: setup.loop_,
                })
                .expect("the butler is alive");
            assert!(
                streamer.step_until_settled(1_000) < 1_000,
                "the ring primes"
            );
        }
        let clock = MockTransport::rolling(Beat::new(setup.beat), Bpm::new(120.0));
        let mut voice = streamer
            .status()
            .take_disk_voice(0, clock.clone(), Beat(0.0), None)
            .expect("the link is installed");
        voice.set_sample_rate(SampleRate(SR));
        voice.set_speed(PlaybackRate::new(setup.speed));
        Self {
            streamer,
            voice,
            clock,
            output: BufferVec::new(2),
        }
    }

    /// One block of the voice, channel 0; then the clock moves and the
    /// butler runs a cycle, as the thread would before the next callback.
    fn block(&mut self) -> Vec<f32> {
        let input = BufferVec::new(0);
        self.voice
            .process(BLOCK, &input.buffer_ref(), &mut self.output.buffer_mut());
        let out = self.output.buffer_ref();
        let left = (0..BLOCK).map(|i| out.at_f32(0, i)).collect();
        self.clock.advance(BLOCK as i64, SR);
        let _ = self.streamer.step_once();
        left
    }

    fn render(&mut self, blocks: usize) -> Vec<f32> {
        (0..blocks).flat_map(|_| self.block()).collect()
    }

    fn loop_(&mut self, setting: LoopSetting) {
        self.streamer
            .commands()
            .send(Command::Loop {
                channel_index: 0,
                setting,
            })
            .expect("the butler is alive");
    }

    /// Window moves and retractions of the voice's ring so far.
    fn moves(&self) -> u64 {
        self.voice.inner.read.ring().moves()
    }

    /// Frames the voice could not read since the last call.
    fn underruns(&self) -> u64 {
        self.voice.shared_state.take_underruns()
    }
}

/// A placed memory voice at `speed` from beat `from`, looped by `loop_`,
/// on its own clock, rendered block by block like a [`Live`] one.
struct Memory {
    source: MemorySource,
    clock: Arc<MockTransport>,
    output: BufferVec,
}

impl Memory {
    fn new(wave: Arc<tutti_io::Wave>, loop_: LoopSetting, speed: f32, from: f64) -> Self {
        let clock = MockTransport::rolling(Beat::new(from), Bpm::new(120.0));
        let mut source = MemorySource::with_transport(wave, clock.clone(), Beat(0.0), None);
        source.set_speed(PlaybackRate::new(speed));
        source.set_loop_setting(loop_);
        source.set_sample_rate(SampleRate(SR));
        Self {
            source,
            clock,
            output: BufferVec::new(2),
        }
    }

    fn block(&mut self) -> Vec<f32> {
        let input = BufferVec::new(0);
        self.source
            .process(BLOCK, &input.buffer_ref(), &mut self.output.buffer_mut());
        let out = self.output.buffer_ref();
        let left = (0..BLOCK).map(|i| out.at_f32(0, i)).collect();
        self.clock.advance(BLOCK as i64, SR);
        left
    }

    fn render(&mut self, blocks: usize) -> Vec<f32> {
        (0..blocks).flat_map(|_| self.block()).collect()
    }
}

/// Assert `live` and `memory` bit-identical at every frame outside `allowed`
/// (half-open frame ranges), naming the first frame that is not.
fn assert_same_outside(what: &str, live: &[f32], memory: &[f32], allowed: &[(usize, usize)]) {
    assert_eq!(live.len(), memory.len(), "{what}: lengths");
    for (k, (&x, &y)) in live.iter().zip(memory).enumerate() {
        if allowed.iter().any(|&(a, b)| k >= a && k < b) {
            continue;
        }
        assert_eq!(
            x.to_bits(),
            y.to_bits(),
            "{what}: frame {k}: live {x} memory {y}"
        );
    }
}

/// **A live looped voice plays the memory tier's loop bit for bit, from its
/// first frame**, across many wraps: crossfaded, hard, from frame 0 (the fade
/// into the loop's head, `LoopSpan`'s fallback), a loop whose end is past the
/// file (clamped to it, on both tiers), at unity, 1.5×, 0.75× and — the rate
/// the old reader could only approximate — a 44.1 kHz file at 48 kHz. The
/// ring never moves and nothing underruns while it loops.
///
/// Mutation (run): the blend dropped from `Mapping::fill` → the crossfaded rows part in
/// the first fade → fails. Mutation (run): `place_frame` wrapping to `start`
/// rather than `resume` → the row from frame 0 parts at its first wrap →
/// fails. Mutation (run): the loop's end not clamped to the file on the ring
/// (the file's length → `usize::MAX` in `handle_set_stream_loop`) → the
/// past-the-file row parts at the file's end → fails. Mutation (run): a
/// refill that moves a looped ring's window every cycle (the old `AtEnd`
/// flush, in the new ring) → the ring moves → fails.
#[test]
fn a_live_looped_voice_plays_the_memory_tiers_loop_bit_for_bit() {
    struct Case {
        what: &'static str,
        file_rate: u32,
        speed: f32,
        loop_: LoopSetting,
    }
    let cases = [
        Case {
            what: "a crossfaded loop",
            file_rate: SR as u32,
            speed: 1.0,
            loop_: loop_on(3_000.0, 7_001.0, 700),
        },
        Case {
            what: "a crossfaded loop at 1.5x",
            file_rate: SR as u32,
            speed: 1.5,
            loop_: loop_on(3_000.0, 7_001.0, 700),
        },
        Case {
            what: "a hard loop at 1.5x",
            file_rate: SR as u32,
            speed: 1.5,
            loop_: loop_on(3_000.0, 7_001.0, 0),
        },
        Case {
            what: "a crossfaded loop at 0.75x",
            file_rate: SR as u32,
            speed: 0.75,
            loop_: loop_on(3_000.0, 5_001.0, 500),
        },
        Case {
            what: "a crossfaded loop from frame 0",
            file_rate: SR as u32,
            speed: 1.0,
            loop_: loop_on(0.0, 5_001.0, 700),
        },
        Case {
            what: "a loop whose end is past the file",
            file_rate: SR as u32,
            speed: 1.0,
            loop_: loop_on(36_000.0, 50_000.0, 300),
        },
        Case {
            what: "a 44.1 kHz file at 48 kHz",
            file_rate: 44_100,
            speed: 1.0,
            loop_: loop_on(3_000.0, 7_001.0, 700),
        },
    ];
    let dir = tempfile::tempdir().expect("a temp dir");
    for case in cases {
        let path = dir.path().join(format!("ramp{}.wav", case.file_rate));
        write_ramp(&path, case.file_rate, LEN);
        let mut live = Live::new(&path, case.loop_, case.speed);
        let moves = live.moves();
        const BLOCKS: usize = 900;
        let got = live.render(BLOCKS);
        assert_eq!(
            live.moves(),
            moves,
            "{}: the ring moved while looping",
            case.what
        );
        assert_eq!(live.underruns(), 0, "{}: frames went unread", case.what);
        let want =
            Memory::new(ramp_wave(case.file_rate, LEN), case.loop_, case.speed, 0.0).render(BLOCKS);
        let rate = case.speed as f64 * case.file_rate as f64 / SR;
        let LoopSetting::On { end, .. } = case.loop_ else {
            unreachable!()
        };
        let wraps = (got.len() as f64 * rate - end.get().min(LEN as f64)) / 2_000.0;
        assert!(wraps > 5.0, "{}: too few wraps to say anything", case.what);
        assert!(got.iter().any(|&s| s != 0.0), "{}: silent", case.what);
        assert_same_outside(case.what, &got, &want, &[]);
    }
}

/// **A live crossfaded loop is continuous at its wrap** (doc 013's S3, the
/// live tier): on a sine whose loop points click when cut hard — the loop
/// starts on a rising zero crossing and ends a quarter period later in the
/// cycle — no step in the output is larger than the sine's own (`2 sin(π /
/// PERIOD)`), round the loop three times; and a loop from frame 0 `[0,
/// 2281)`, whose fade goes into its head. The hard loops are asserted to click,
/// so the loop points have teeth.
///
/// Mutation (run): the blend dropped from `Mapping::fill` → the crossfaded
/// loops click → fails. Mutation (run): the fade's lead-in read from `resume`
/// rather than before it (the old head replay) → a step far above the sine's
/// own at the wrap → fails.
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
        let mut live = Live::new(&path, loop_on(start, end, fade), 1.0);
        let got = live.render((3_025 + 3 * 2_025) / BLOCK);
        let at_loop = format!("[{start}, {end}) fade {fade}");
        let (step, at) = got
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

/// **Entering the clip past the loop's end, and with a PDC preroll, plays
/// the memory tier's frames there**, bit for bit: the stream's position counts
/// the file straight on and the ring places it, so a voice entering at beat 1
/// (24 000 frames in, well past a loop `[3000, 7001)`) plays what a memory
/// voice plays there — and with a 12 000-frame preroll on its channel, what
/// the memory voice plays half a beat earlier.
///
/// Mutation (run): the ring writing the straight position unplaced
/// (`place_frame` → identity) → fails. Mutation (run): the reader not taking
/// the preroll off its position → the preroll row plays beat 1's frames →
/// fails.
#[test]
fn an_entry_past_the_loop_and_a_preroll_land_where_the_memory_tier_plays() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    write_ramp(&path, SR as u32, LEN);
    let loop_ = loop_on(3_000.0, 7_001.0, 700);
    for (preroll, memory_from) in [(0usize, 1.0), (12_000, 0.5)] {
        let mut live = Live::with(
            &path,
            Setup {
                beat: 1.0,
                preroll,
                ..Setup::looped(loop_, 1.0)
            },
        );
        let got = live.render(300);
        assert_eq!(live.underruns(), 0, "preroll {preroll}: frames went unread");
        let want = Memory::new(ramp_wave(SR as u32, LEN), loop_, 1.0, memory_from).render(300);
        assert_same_outside(&format!("preroll {preroll}"), &got, &want, &[]);
    }
}

/// **Reverse ignores the loop, and mirrors the file as the memory tier's
/// reverse does**: a reversed voice entering at beat 1 plays file frame
/// `len - 1 - p` at position `p`, straight through a loop `[3000, 7001)` and
/// on to silence past the file's first frame — exactly as the same voice with
/// no loop, frame for frame, and as a reversed memory voice once the turn
/// (a switch just past the block the voice was in, crossfaded) is done. A loop
/// changed while it plays reversed is only stored: the ring does not move.
///
/// Mutation (run): the reverse mapping honouring the loop (`forward_loop` not
/// filtering reverse) → the looped run parts at the loop → fails. Mutation
/// (run): the reverse fill read from `len - pos` (one frame off) → parts from
/// the mirror → fails. Mutation (run): `apply_mapping` switching even where
/// the mappings agree on every written slot → the mid-play change moves the
/// ring → fails.
#[test]
fn a_reversed_voice_ignores_its_loop_and_mirrors_the_file() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    write_ramp(&path, SR as u32, LEN);
    let reversed = |loop_| {
        let mut live = Live::with(
            &path,
            Setup {
                beat: 1.0,
                ..Setup::looped(loop_, 1.0)
            },
        );
        live.voice.set_direction(crate::Direction::Reverse);
        let _ = live.streamer.step_until_settled(1_000);
        live
    };
    const BLOCKS: usize = 420;
    let mut plain = reversed(LoopSetting::Off);
    let want = plain.render(BLOCKS);
    let mut looped = reversed(loop_on(3_000.0, 7_001.0, 700));
    let moves = looped.moves();
    let mut got = looped.render(20);
    looped.loop_(loop_on(10_000.0, 12_000.0, 0));
    got.extend(looped.render(BLOCKS - 20));
    assert_eq!(looped.moves(), moves, "a loop change moved a reversed ring");
    // The turn lands a guard past the block the voice was in, and fades
    // over the ring's fade length from what the old (looped or plain)
    // mapping held: past that, the two are the same frames.
    let settled = BLOCK + crate::butler::GUARD_FRAMES as usize + FADE + 8;
    assert_same_outside("looped against plain", &got, &want, &[(0, settled)]);

    // From there on, the memory tier's mirror (exact: whole positions).
    for (k, &v) in got.iter().enumerate().skip(settled) {
        let p = 24_000 + k;
        let want = if p < LEN { value(LEN - 1 - p) } else { 0.0 };
        assert_eq!(v, want, "reversed, frame {k} (position {p})");
    }
}

/// **A loop change takes effect where the memory tier's does, never lags,
/// and costs no frame**: the memory tier changes its loop at the edit; the
/// live ring switches a guard past the block its reader is in and crossfades
/// there from the butler's record of the old loop. Outside that span the two
/// are bit-identical, before and after — the live voice is on the clock — and
/// nothing underruns. With the default crossfade (512 frames).
///
/// Here: a crossfaded loop `[1000, 3000)`, changed 81 blocks in (at straight
/// 5 184, its frame 1 184) to a hard `[200, 1400)`, which places the same
/// position on frame 384: the memory tier jumps 800 frames of ramp there.
///
/// Mutation (run): the change only stored (no switch) → the ring plays on
/// through the old loop → fails. Mutation (run): the record's old frames
/// taken under the *new* mapping → the fade blends new with new and the
/// switch is a jump → the crossfade assertion fails.
#[test]
fn a_loop_change_takes_effect_on_the_clock_with_no_frame_lost() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    write_ramp(&path, SR as u32, LEN);
    let (a, b) = (loop_on(1_000.0, 3_000.0, 300), loop_on(200.0, 1_400.0, 0));
    let mut live = Live::new(&path, a, 1.0);
    let mut memory = Memory::new(ramp_wave(SR as u32, LEN), a, 1.0, 0.0);
    let mut got = live.render(81);
    let mut want = memory.render(81);
    let edit = got.len();
    live.loop_(b);
    memory.source.set_loop_setting(b);
    got.extend(live.render(120));
    want.extend(memory.render(120));
    assert_eq!(live.underruns(), 0, "frames went unread");
    let window = (
        edit,
        edit + BLOCK * 2 + crate::butler::GUARD_FRAMES as usize + FADE + 8,
    );
    assert_same_outside("across a loop change", &got, &want, &[window]);
    // Inside the window: the old loop, then a fade, never a hard jump — each
    // step is a fraction of the jump the memory tier made.
    let jump = (want[edit] - want[edit - 1]).abs().max(1e-6);
    let worst = got[window.0..window.1]
        .windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .fold(0.0f32, f32::max);
    assert!(
        worst < jump / 4.0,
        "a step of {worst} inside the switch, against the memory tier's jump of {jump}"
    );
}

/// **N loop edits and transport jumps leave the live voice exactly on the
/// clock** (the review of #48's blocker 2): with the default crossfade, a
/// sequence of loop edits, jumps inside and outside the ring's window, and a
/// varispeed change, each applied at the same block to a live voice and a
/// memory voice. Outside a bounded span after each event the two are
/// bit-identical — the last 200 blocks included, so nothing drifts — and the
/// live voice never underruns: every reposition costs 0 frames.
///
/// Mutation (run): the reader not taking the old continuation at a jump (no
/// scratch fade) → the refill gap underruns → fails. Mutation (run): the
/// refill not following a reader that jumped outside the window → the new
/// position is never filled → fails.
#[test]
fn edits_and_jumps_leave_the_live_voice_on_the_clock() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    write_ramp(&path, SR as u32, LEN);
    enum Event {
        Loop(LoopSetting),
        Jump(f64),
        Speed(f32),
    }
    let events = [
        (30, Event::Loop(loop_on(8_000.0, 12_001.0, 400))),
        (60, Event::Jump(beats(9_000.0))),
        (90, Event::Loop(loop_on(9_000.0, 11_001.0, 400))),
        (120, Event::Jump(beats(30_000.0))),
        (150, Event::Speed(1.5)),
        (180, Event::Loop(LoopSetting::Off)),
        (210, Event::Jump(beats(1_000.0))),
        (240, Event::Loop(loop_on(2_000.0, 6_001.0, 0))),
    ];
    let first = loop_on(3_000.0, 7_001.0, 700);
    let mut live = Live::new(&path, first, 1.0);
    let mut memory = Memory::new(ramp_wave(SR as u32, LEN), first, 1.0, 0.0);
    let (mut got, mut want, mut allowed) = (Vec::new(), Vec::new(), Vec::new());
    let mut at = 0;
    for (block, event) in &events {
        got.extend(live.render(block - at));
        want.extend(memory.render(block - at));
        at = *block;
        let span = BLOCK * 3 + crate::butler::GUARD_FRAMES as usize * 2 + FADE * 2;
        allowed.push((got.len(), got.len() + span));
        match event {
            Event::Loop(setting) => {
                live.loop_(*setting);
                memory.source.set_loop_setting(*setting);
            }
            Event::Jump(beat) => {
                live.clock.set_beat(Beat::new(*beat));
                memory.clock.set_beat(Beat::new(*beat));
            }
            Event::Speed(speed) => {
                live.voice.set_speed(PlaybackRate::new(*speed));
                memory.source.set_speed(PlaybackRate::new(*speed));
            }
        }
    }
    got.extend(live.render(200 + 30));
    want.extend(memory.render(200 + 30));
    assert_eq!(live.underruns(), 0, "a reposition cost frames");
    assert!(got.iter().filter(|&&s| s != 0.0).count() > got.len() / 2);
    assert_same_outside("edits and jumps", &got, &want, &allowed);
}

/// **Two loop changes in one butler cycle, and a jump with a loop change in
/// one cycle, settle to the last loop at the clock** (the review of #48's
/// blocker 1: a second reposition read the ring's head off a pending flush
/// and resumed at frame 0). The second change supersedes the first before the
/// reader reaches it; the jump is the reader's own, the loop the butler's.
///
/// Mutation (run): the second of two changes ignored while the first's switch
/// is pending (`apply_mapping` returning `Same` whenever a switch is pending)
/// → it settles to the first loop → fails. (The fold of a pending switch into
/// the next, `at.min(switch_at)`, is a guard no sequence of loop edits here
/// reaches: a divergence is found against what the ring holds, so it lies at
/// or below a pending switch whenever the new mapping differs there.)
#[test]
fn two_changes_in_one_cycle_settle_to_the_last() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    write_ramp(&path, SR as u32, LEN);
    let first = loop_on(1_000.0, 3_000.0, 0);
    for jump in [false, true] {
        let mut live = Live::new(&path, first, 1.0);
        let mut memory = Memory::new(ramp_wave(SR as u32, LEN), first, 1.0, 0.0);
        let mut got = live.render(81);
        let mut want = memory.render(81);
        let edit = got.len();
        let last = loop_on(500.0, 1_500.0, 300);
        if jump {
            live.clock.set_beat(Beat::new(beats(20_000.0)));
            memory.clock.set_beat(Beat::new(beats(20_000.0)));
        } else {
            live.loop_(loop_on(2_000.0, 2_500.0, 0));
        }
        live.loop_(last);
        memory.source.set_loop_setting(last);
        got.extend(live.render(150));
        want.extend(memory.render(150));
        assert_eq!(live.underruns(), 0, "jump {jump}: frames went unread");
        let span = BLOCK * 3 + crate::butler::GUARD_FRAMES as usize * 2 + FADE * 2;
        assert_same_outside(&format!("jump {jump}"), &got, &want, &[(edit, edit + span)]);
    }
}

/// **A loop edit that changes nothing near the playhead is not heard**: the
/// same loop set again is nothing at all (no switch, the ring does not move),
/// and a loop end moved far ahead switches exactly where the two loops part —
/// so the output is bit-identical to the memory tier with the edit throughout
/// (no crossfade, no guard), and to no edit at all until the old end.
///
/// Mutation (run): `apply_mapping` switching a guard past the reader
/// whatever the divergence (`at` = `busy + GUARD`) → the switch lands near
/// 1 000, dropping 29 000 buffered frames that agreed → fails. Mutation (run):
/// `apply_mapping` switching even where the mappings agree on every written
/// slot → the same loop set again moves the ring → fails.
#[test]
fn an_edit_that_changes_nothing_near_the_playhead_is_not_heard() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    write_ramp(&path, SR as u32, LEN);
    let near = loop_on(3_000.0, 30_000.0, 0);
    let far = loop_on(3_000.0, 31_000.0, 0);

    let mut same = Live::new(&path, near, 1.0);
    let before = same.render(10);
    let moves = same.moves();
    same.loop_(near);
    let mut got = before;
    got.extend(same.render(100));
    assert_eq!(same.moves(), moves, "setting the same loop moved the ring");
    let unedited = Memory::new(ramp_wave(SR as u32, LEN), near, 1.0, 0.0).render(110);
    assert_same_outside("the same loop again", &got, &unedited, &[]);

    let mut moved = Live::new(&path, near, 1.0);
    let mut memory = Memory::new(ramp_wave(SR as u32, LEN), near, 1.0, 0.0);
    let mut got = moved.render(10);
    let mut want = memory.render(10);
    moved.loop_(far);
    memory.source.set_loop_setting(far);
    got.extend(moved.render(1));
    let at = moved.voice.inner.read.ring().map().at;
    assert_eq!(at, 30_000, "the ring switched where the loops still agreed");
    got.extend(moved.render(599));
    want.extend(memory.render(600));
    assert_eq!(moved.underruns(), 0, "frames went unread");
    assert_same_outside("a loop end moved far ahead", &got, &want, &[]);
    let unedited = Memory::new(ramp_wave(SR as u32, LEN), near, 1.0, 0.0).render(610);
    assert_same_outside(
        "until the old end",
        &got[..30_000],
        &unedited[..30_000],
        &[],
    );
}

/// **A looped stream refilled in parallel loops as one refilled serially**
/// (three streams, so the butler's parallel refill runs): bit-identical to
/// the memory tier, with the loop travelling with its writer.
///
/// The loop lives on the ring's writer (`loops::Content`), which the
/// parallel pass hands each worker whole, so neither path can drop it on its
/// own (the old parallel work item carried a copy of the loop range, and
/// dropping it there was a live bug class). Mutation (run): the parallel pass
/// skipping its work items → nothing refills → underruns → fails.
#[test]
fn a_looped_stream_refilled_in_parallel_plays_the_memory_tiers_loop() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    write_ramp(&path, SR as u32, LEN);
    let loop_ = loop_on(3_000.0, 7_001.0, 700);
    let mut live = Live::with(
        &path,
        Setup {
            neighbours: 2,
            ..Setup::looped(loop_, 1.5)
        },
    );
    let got = live.render(600);
    assert_eq!(live.underruns(), 0, "frames went unread");
    let want = Memory::new(ramp_wave(SR as u32, LEN), loop_, 1.5, 0.0).render(600);
    assert_same_outside("parallel refill", &got, &want, &[]);
}

/// **An export fork taken after a loop edit renders the loop the edit ends
/// on, and agrees with the live voice past its switch**: a fork of the live
/// voice (isolated, rebound onto a render clock from beat 0, as a graph fork
/// does) plays the edited loop from the start of its render — the stream's
/// record holds the loop the butler runs — and, frame for frame, what the
/// live voice plays once its switch has landed and faded.
///
/// Mutation (run): the stream's record not told the new loop (`set_loop`
/// dropped from `handle_set_stream_loop`) → the fork plays the old loop →
/// fails.
#[test]
fn a_fork_after_a_loop_edit_renders_the_edited_loop() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    write_ramp(&path, SR as u32, LEN);
    let (a, b) = (loop_on(1_000.0, 3_000.0, 300), loop_on(200.0, 1_400.0, 100));
    let mut live = Live::new(&path, a, 1.0);
    let mut got = live.render(81);
    let edit = got.len();
    live.loop_(b);
    got.extend(live.render(150));

    let clock = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
    let render: tutti_core::transport::OfflineTransport = clock.clone();
    let mut fork = live.voice.clone();
    fork.isolate();
    fork.rebind_offline(&render);
    fork.reset();
    fork.set_sample_rate(SampleRate(SR));
    let input = BufferVec::new(0);
    let mut output = BufferVec::new(2);
    let mut forked = Vec::new();
    for _ in 0..81 + 150 {
        fork.process(BLOCK, &input.buffer_ref(), &mut output.buffer_mut());
        forked.extend((0..BLOCK).map(|i| output.buffer_ref().at_f32(0, i)));
        clock.advance(BLOCK as i64, SR);
    }
    let want = Memory::new(ramp_wave(SR as u32, LEN), b, 1.0, 0.0).render(81 + 150);
    assert_same_outside("the fork against the edited loop", &forked, &want, &[]);
    let settled = edit + BLOCK * 2 + crate::butler::GUARD_FRAMES as usize + FADE + 8;
    assert_same_outside(
        "the fork against the live voice past its switch",
        &forked,
        &got,
        &[(0, settled)],
    );
}
