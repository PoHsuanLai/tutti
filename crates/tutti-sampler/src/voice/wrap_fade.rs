//! A placed memory voice across a transport loop wrap: the tail it was
//! playing past the loop's end fades out under the loop's start, over
//! `clock::WRAP_FADE`, instead of cutting. Each test runs on both hosts of
//! the read: a `VoicePool` slot and a bare `MemorySource` node.
//!
//! The source is a pure tone, so the render is continuous but for the wrap:
//! the largest step between neighbouring samples is bounded analytically, and
//! a cut shows as a step of the tone's amplitude.

use std::sync::Arc;

use tutti_codec::Wave;
use tutti_graph::{Env, Node, Offset, Prepare, Transport, TransportChanges, contract};
use tutti_types::{
    Beat, BeatDuration, Bpm, ChannelLayout, Frame, FrameClock, LoopRange, SampleRate, Samples,
};

use super::clock::WRAP_FADE;
use super::memory_source::MemorySource;
use super::node::VoiceNode;
use super::pool::VoicePool;
use super::types::{Playback, SlotId, Voice, VoiceSource};

const SR: f64 = 48_000.0;
/// Frames in a beat at 120 BPM.
const FPB: usize = 24_000;
/// The loop: 1.3 beats, so no tone divides it.
const LOOP_BEATS: f64 = 1.75;
/// The loop starts mid-source, so a voice placed at 0 is mid-sound at the wrap.
const LOOP_START_BEATS: f64 = 0.5;
const LS: usize = 12_000;
const WRAP: usize = 42_000;
const AMP: f32 = 0.5;
const FADE: usize = 240;

fn tone(freq: f64, i: usize) -> f32 {
    AMP * (std::f64::consts::TAU * freq * i as f64 / SR).sin() as f32
}

/// A wave of `channels` tones (437.3 Hz, then 311.7 Hz), `len` frames.
fn wave(channels: usize, len: usize, tone_at: impl Fn(f64, usize) -> f32) -> Arc<Wave> {
    let mut w = Wave::new(channels, SampleRate::new(SR));
    for i in 0..len {
        let frame: Vec<f32> = [437.3, 311.7][..channels]
            .iter()
            .map(|&f| tone_at(f, i))
            .collect();
        w.push_frame(&frame);
    }
    Arc::new(w)
}

fn region(start: f64, end: f64) -> LoopRange {
    LoopRange::new(Beat::new(start), Beat::new(end)).expect("a loop")
}

/// What renders the voice: a pool slot, or the source as a graph node.
#[derive(Clone, Copy, Debug)]
enum Host {
    Pool,
    Node,
    Voice,
}

enum Hosted {
    Pool(VoicePool),
    Node(MemorySource),
    Voice(Option<VoiceNode>),
}

/// Runs a test on each [`Host`], as `name::pool` and `name::node`.
macro_rules! on_each_host {
    ($($name:ident),* $(,)?) => {$(
        mod $name {
            #[test]
            fn pool() {
                super::$name(super::Host::Pool)
            }
            #[test]
            fn node() {
                super::$name(super::Host::Node)
            }
            #[test]
            fn voice() {
                super::$name(super::Host::Voice)
            }
        }
    )*};
}

/// One placed voice behind a host clock that loops.
struct Rig {
    host: Hosted,
    max_block: usize,
    clock: FrameClock,
    looping: Option<LoopRange>,
    frame: u64,
}

impl Rig {
    fn new(host: Host, looping: Option<LoopRange>, max_block: usize) -> Self {
        let host = match host {
            Host::Pool => {
                let mut pool = VoicePool::with_channels(ChannelLayout::STEREO).expect("stereo");
                pool.prepare(&Prepare::new(SampleRate(SR), Samples(max_block)));
                Hosted::Pool(pool)
            }
            // Placed by `place`.
            Host::Voice => Hosted::Voice(None),
            Host::Node => Hosted::Node(
                MemorySource::new(Arc::new(Wave::new(2, SampleRate::new(SR))))
                    .expect("a placeable wave"),
            ),
        };
        Self {
            host,
            max_block,
            clock: FrameClock::new(Beat::new(0.0), Bpm::of(120.0), SampleRate(SR)),
            looping,
            frame: 0,
        }
    }

    /// A voice placed at `start` beats for `duration` (to the end of the wave
    /// when `None`).
    fn place(&mut self, id: u128, wave: &Arc<Wave>, start: f64, duration: Option<f64>) {
        self.place_with(id, wave, start, duration, Playback::default());
    }

    fn place_with(
        &mut self,
        id: u128,
        wave: &Arc<Wave>,
        start: f64,
        duration: Option<f64>,
        play: Playback,
    ) {
        let mut source = MemorySource::placed(
            Arc::clone(wave),
            Beat::new(start),
            duration.map(BeatDuration::new),
        )
        .expect("a placeable wave");
        match &mut self.host {
            Hosted::Pool(pool) => {
                source.set_render_rate(SampleRate(SR));
                pool.insert_voice(
                    SlotId(id),
                    Voice {
                        source: VoiceSource::Memory(source),
                        play,
                    },
                );
            }
            Hosted::Voice(slot) => {
                let mut node = VoiceNode::new(Voice {
                    source: VoiceSource::Memory(source),
                    play,
                });
                node.prepare(&Prepare::new(SampleRate(SR), Samples(self.max_block)));
                *slot = Some(node);
            }
            Hosted::Node(node) => {
                source.set_gain(play.gain);
                source.set_loop_setting(play.loop_);
                source.prepare(&Prepare::new(SampleRate(SR), Samples(self.max_block)));
                *node = source;
            }
        }
    }

    /// One block under `env`.
    fn drive(&mut self, env: &Env) -> Vec<Vec<f32>> {
        match &mut self.host {
            Hosted::Pool(pool) => contract::drive_input(pool, env, &[], &[], &[]).audio,
            Hosted::Node(node) => contract::drive_input(node, env, &[], &[], &[]).audio,
            Hosted::Voice(node) => {
                let node = node.as_mut().expect("placed");
                contract::drive_input(node, env, &[], &[], &[]).audio
            }
        }
    }

    fn transport(&self, playing: bool) -> Transport {
        let looping = self.looping.map(|r| tutti_graph::LoopRange {
            start: r.start(),
            end: r.end(),
        });
        Transport::new(playing, self.clock.tempo(), self.clock.beat(), looping)
    }

    /// One block of `len` frames; `change` is a transport change inside it.
    fn block(&mut self, len: usize, change: Option<(usize, Transport)>) -> Vec<Vec<f32>> {
        let mut changes = TransportChanges::NONE;
        if let Some((at, to)) = change {
            changes
                .push(Offset::new(at, Samples(len)).expect("inside"), to)
                .expect("room");
        }
        let env = Env {
            frame: Frame(self.frame),
            sample_rate: SampleRate(SR),
            block_len: Samples(len),
            transport: self.transport(true),
            changes,
            play: tutti_graph::PlayClock::ZERO,
        };
        let out = self.drive(&env);
        self.frame += len as u64;
        self.clock.advance(Samples(len), self.looping);
        out
    }

    /// One block of `len` frames whose transport changes at `at` to `tempo`
    /// and `looping`, the host clock following it.
    fn block_changed(
        &mut self,
        len: usize,
        at: usize,
        tempo: f64,
        looping: Option<LoopRange>,
    ) -> Vec<Vec<f32>> {
        let mut here = self.clock;
        here.advance(Samples(at), self.looping);
        let to = Transport::new(
            true,
            Bpm::of(tempo),
            here.beat(),
            looping.map(|r| tutti_graph::LoopRange {
                start: r.start(),
                end: r.end(),
            }),
        );
        let out = self.block(len, Some((at, to)));
        self.clock = to.clock(SampleRate(SR));
        self.looping = looping;
        self.clock.advance(Samples(len - at), looping);
        out
    }

    /// `frames` frames in blocks of `block` (the last one shorter).
    fn run(&mut self, frames: usize, block: usize) -> Vec<Vec<f32>> {
        let mut out = vec![Vec::new(); 2];
        let mut left = frames;
        while left > 0 {
            let len = left.min(block);
            for (o, b) in out.iter_mut().zip(self.block(len, None)) {
                o.extend(b);
            }
            left -= len;
        }
        out
    }
}

/// The largest step between neighbouring samples of `x[from..to]`.
fn max_step(x: &[f32], from: usize, to: usize) -> f32 {
    (from + 1..to)
        .map(|i| (x[i] - x[i - 1]).abs())
        .fold(0.0, f32::max)
}

/// The tone's own largest step: amplitude times the phase step.
fn natural_step(freq: f64) -> f32 {
    AMP * (std::f64::consts::TAU * freq / SR) as f32
}

/// What a wrap's crossfade of `fade` frames can step by: each of the two
/// tones moves by its own step under gains summing to at most root 2, and the
/// gain moves by at most `pi/2` over `fade + 1` frames across their gap of at
/// most `2 * AMP`.
fn step_bound(freq: f64, fade: usize) -> f32 {
    std::f32::consts::SQRT_2 * natural_step(freq)
        + 2.0 * AMP * std::f32::consts::FRAC_PI_2 / (fade + 1) as f32
        + 1e-4
}

/// The fade in frames at 48 kHz.
#[test]
fn the_fade_is_five_milliseconds() {
    assert_eq!(WRAP_FADE.to_samples(SampleRate(SR)), Samples(FADE));
    assert_eq!(
        WRAP_FADE.to_samples(SampleRate(96_000.0)),
        Samples(2 * FADE),
        "a time, not a frame count"
    );
}

/// A tone through a loop wrap has no step beyond the fade's, at any block
/// length (the fade crosses block edges), in mono and stereo, and the render
/// sounds nowhere else different from the tone itself.
///
/// Mutation (run): `Wrap::new` giving no fade (`len > 0` never true) → the
/// wrap steps by 0.4 or more → fails. Mutation (run): `Wrap::after` always
/// `None` (the fade dropped at the first block edge) → fails at the 64 and 100
/// frame blocks, where the fade crosses edges → fails.
fn a_tone_has_no_cut_at_the_wrap_at_any_block_length(host: Host) {
    for channels in [1, 2] {
        for block in [64, 100, 480, 1000] {
            let w = wave(channels, 4 * FPB, tone);
            let mut rig = Rig::new(host, Some(region(LOOP_START_BEATS, LOOP_BEATS)), 1000);
            rig.place(1, &w, 0.0, None);
            let out = rig.run(2 * WRAP, block);
            for (c, lane) in out.iter().enumerate() {
                let freq = if channels == 1 {
                    437.3
                } else {
                    [437.3, 311.7][c]
                };
                let bound = step_bound(freq, FADE);
                let around = max_step(lane, WRAP - 4, WRAP + FADE + 4);
                assert!(
                    around <= bound,
                    "{channels} channels, block {block}, lane {c}: step {around} > {bound}"
                );
                // Away from the wraps the voice is the tone, to the kernel's
                // rounding.
                assert!(max_step(lane, 100, WRAP - 100) <= natural_step(freq) * 1.001);
                assert!(lane[WRAP..WRAP + FADE].iter().any(|s| s.abs() > 0.1));
            }
        }
    }
}

/// Before the wrap the voice is the source, bit for bit, and the crossfade
/// starts on the wrap frame, not before: the first frame of the loop is the
/// loop's start at the fade's first gain, the tail under it at its first.
///
/// Mutation (run): the blend starting a frame early (`first` one less in
/// `crossfade_wraps`'s tail read) → the formula frames are off → fails.
fn the_fade_begins_on_the_wrap_frame_and_changes_nothing_before_it(host: Host) {
    let w = wave(1, 4 * FPB, tone);
    let mut rig = Rig::new(host, Some(region(LOOP_START_BEATS, LOOP_BEATS)), 1000);
    rig.place(1, &w, 0.0, None);
    let out = rig.run(WRAP + FADE + 8, 480);
    for (i, s) in out[0].iter().enumerate().take(WRAP) {
        assert_eq!(s.to_bits(), tone(437.3, i).to_bits(), "frame {i}");
    }
    let x = 1.0 / (FADE + 1) as f32 * std::f32::consts::FRAC_PI_2;
    let want = x.sin() * tone(437.3, LS) + x.cos() * tone(437.3, WRAP);
    assert!(
        (out[0][WRAP] - want).abs() < 1e-5,
        "{} vs {want}",
        out[0][WRAP]
    );
    // Past the fade, the loop's start alone.
    for k in 0..8 {
        assert_eq!(
            out[0][WRAP + FADE + k].to_bits(),
            tone(437.3, LS + FADE + k).to_bits()
        );
    }
}

/// Across a wrap between unrelated material the fade holds the level:
/// the energy over the fade is the steady tone's, where a linear fade
/// would dip a third. The tail is a 3 kHz tone and the loop's start a
/// 1 kHz one, each a whole number of cycles over the fade.
///
/// Mutation (run): `CrossfadeCurve::EqualAmplitude` in `crossfade_wraps` →
/// the energy is 0.67 of the steady tone's → fails.
fn the_fade_holds_the_energy_of_unrelated_material(host: Host) {
    let w = wave(1, 4 * FPB, |_, i| {
        let f = if i < WRAP { 1_000.0 } else { 3_000.0 };
        tone(f, i)
    });
    let mut rig = Rig::new(host, Some(region(LOOP_START_BEATS, LOOP_BEATS)), 1000);
    rig.place(1, &w, 0.0, None);
    let out = rig.run(WRAP + FADE, 1000);
    let energy: f32 = out[0][WRAP..WRAP + FADE].iter().map(|s| s * s).sum();
    let steady = AMP * AMP / 2.0 * FADE as f32;
    assert!(
        (energy / steady - 1.0).abs() < 0.1,
        "energy {energy} against the steady tone's {steady}"
    );
}

/// A voice with nothing past the loop's end has no tail: its loop start is
/// the source from the wrap on, with no ramp, and it is silent before.
///
/// Mutation (run): `place_tail` not gating on the window (`e` taken as `a`) →
/// the loop's start is crossfaded with a tone it was not playing → fails.
fn a_voice_that_is_not_playing_across_the_wrap_is_untouched(host: Host) {
    let w = wave(1, 4 * FPB, tone);
    // One beat of a loop of 1.3: it ends 0.3 beats before the wrap.
    let mut rig = Rig::new(host, Some(region(LOOP_START_BEATS, LOOP_BEATS)), 1000);
    rig.place(1, &w, 0.0, Some(1.0));
    let out = rig.run(WRAP + 2 * FADE, 1000);
    for (i, s) in out[0].iter().enumerate().take(FPB) {
        assert_eq!(s.to_bits(), tone(437.3, i).to_bits());
    }
    assert!(out[0][FPB + 8..WRAP].iter().all(|&s| s == 0.0));
    for k in 0..2 * FADE {
        assert_eq!(
            out[0][WRAP + k].to_bits(),
            tone(437.3, LS + k).to_bits(),
            "{k}"
        );
    }
}

/// A voice that stops sounding at the loop's end fades out under a loop
/// start it does not play: it does not cut either.
///
/// Mutation (run): the tail read gated out when the head is silent
/// (`!sounds` returning `Silent` before the tail) → the voice cuts → fails.
fn a_tail_with_no_loop_start_under_it_fades_out(host: Host) {
    let w = wave(1, 4 * FPB, tone);
    // Placed at beat 1 for ever: the loop's start (beat 0) is before it.
    let mut rig = Rig::new(host, Some(region(LOOP_START_BEATS, LOOP_BEATS)), 1000);
    rig.place(1, &w, 1.0, None);
    let out = rig.run(WRAP + FADE + 8, 1000);
    let step = max_step(&out[0], WRAP - 4, WRAP + FADE + 4);
    assert!(step <= step_bound(437.3, FADE), "step {step}");
    assert!(out[0][WRAP + FADE - 1].abs() < 0.01, "faded out");
    assert!(out[0][WRAP + FADE..].iter().all(|&s| s == 0.0));
}

/// A voice that begins at or after the loop's end never played in the loop,
/// so it has no tail: the crossfade does not play it.
///
/// Mutation (run): `place_tail` skipping the `reached` check → a ghost of the
/// voice's first 120 frames under the loop's start → fails.
fn a_voice_that_begins_after_the_loop_ends_leaves_no_ghost(host: Host) {
    for start in [LOOP_BEATS, LOOP_BEATS + 0.005] {
        let w = wave(1, 4 * FPB, tone);
        let mut rig = Rig::new(host, Some(region(LOOP_START_BEATS, LOOP_BEATS)), 1000);
        rig.place(1, &w, start, None);
        let out = rig.run(WRAP + 2 * FADE, 1000);
        assert!(out[0].iter().all(|&s| s == 0.0), "a voice at beat {start}");
    }
}

/// A loop shorter than the fade fades over half a pass, several wraps to a
/// block: the second half of each pass is the loop's start alone, and no
/// step is beyond that fade's.
///
/// Mutation (run): `Wrap::new` not halving (`pass` for `pass / 2.0`) → the
/// fade runs the whole pass and the second half is blended → fails.
fn a_loop_shorter_than_the_fade_fades_over_half_a_pass(host: Host) {
    // 0.005 beats: 120 frames, from the middle of the source.
    let w = wave(1, FPB, tone);
    let mut rig = Rig::new(
        host,
        Some(region(LOOP_START_BEATS, LOOP_START_BEATS + 0.005)),
        1000,
    );
    rig.place(1, &w, 0.0, None);
    let out = rig.run(LS + 1000, 1000);
    let bound = step_bound(437.3, 60);
    let step = max_step(&out[0], LS + 130, LS + 1000);
    assert!(step <= bound, "step {step} > {bound}");
    for pass in 1..8 {
        for k in 60..120 {
            assert_eq!(
                out[0][LS + 120 * pass + k].to_bits(),
                tone(437.3, LS + k).to_bits()
            );
        }
    }
}

/// A seek inside the fade ends it on the seek's frame, and a stop ends
/// it: what sounds after is the source at the new playhead, untouched.
///
/// Mutation (run): the change arm of `Runs::next` keeping the crossfade
/// across a seek (the `broke` reset removed) → the seek's target is blended
/// with a tail → fails; across a stop (the `!rolls` reset removed) → the
/// restart blends → fails.
fn a_seek_or_a_stop_during_the_fade_ends_it(host: Host) {
    let w = wave(1, 4 * FPB, tone);
    let mut rig = Rig::new(host, Some(region(LOOP_START_BEATS, LOOP_BEATS)), 1000);
    rig.place(1, &w, 0.0, None);
    rig.run(WRAP - 100, 1000);
    // Block of 400: the wrap is at frame 100, a seek to beat 2 at 150.
    let to = Transport::new(true, Bpm::of(120.0), Beat(2.0), None);
    let out = rig.block(400, Some((150, to)));
    for k in 0..250 {
        assert_eq!(
            out[0][150 + k].to_bits(),
            tone(437.3, 2 * FPB + k).to_bits(),
            "frame {k} after the seek"
        );
    }

    let mut rig = Rig::new(host, Some(region(LOOP_START_BEATS, LOOP_BEATS)), 1000);
    rig.place(1, &w, 0.0, None);
    rig.run(WRAP - 100, 1000);
    let stopped = Transport::new(false, Bpm::of(120.0), Beat(0.01), None);
    let out = rig.block(400, Some((150, stopped)));
    assert!(out[0][150..].iter().all(|&s| s == 0.0));
    // Rolling again where it stands: the loop's start alone.
    let at = rig.clock.beat();
    let restart = Transport::new(true, Bpm::of(120.0), at, None);
    let env = Env {
        frame: Frame(rig.frame),
        sample_rate: SampleRate(SR),
        block_len: Samples(64),
        transport: restart,
        changes: TransportChanges::NONE,
        play: tutti_graph::PlayClock::ZERO,
    };
    let out = rig.drive(&env);
    let first = (at.get() * FPB as f64).round() as usize;
    for (k, s) in out[0].iter().enumerate() {
        assert_eq!(s.to_bits(), tone(437.3, first + k).to_bits());
    }
}

/// A NaN or infinite gain is silence, never NaN, on every host, in frames
/// inside and outside the voice's window (it starts 100 frames into the run
/// and the run crosses a wrap).
///
/// Mutation (run): `audible` returning the gain unguarded -> the frames
/// outside the window are inf * 0.0 = NaN and those inside are NaN -> fails
/// on each host.
fn a_non_finite_gain_renders_silence(host: Host) {
    for gain in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let w = wave(1, 4 * FPB, tone);
        let mut rig = Rig::new(host, Some(region(LOOP_START_BEATS, LOOP_BEATS)), 1000);
        let play = Playback {
            gain: tutti_types::Amplitude(gain),
            ..Playback::default()
        };
        rig.place_with(1, &w, 100.0 / FPB as f64, None, play);
        let out = rig.run(WRAP + 2 * FADE, 480);
        for lane in &out {
            assert!(lane.iter().all(|&s| s == 0.0), "{host:?}, gain {gain}");
        }
    }
}

/// A mono wave of a constant that steps at the wrap: `before` below `WRAP`,
/// `after` from it, per channel `c` offset by `0.1 * c`. Every read is exact.
fn steps(channels: usize, before: f32, after: f32) -> Arc<Wave> {
    steps_at(channels, before, after, WRAP)
}

/// [`steps`] with the step at frame `split`.
fn steps_at(channels: usize, before: f32, after: f32, split: usize) -> Arc<Wave> {
    let mut w = Wave::new(channels, SampleRate::new(SR));
    for i in 0..4 * FPB {
        let v = if i < split { before } else { after };
        let frame: Vec<f32> = (0..channels).map(|c| v + 0.1 * c as f32).collect();
        w.push_frame(&frame);
    }
    Arc::new(w)
}

fn equal_power(k: usize) -> (f32, f32) {
    tutti_graph::CrossfadeCurve::EqualPower.gains(k, FADE)
}

/// The gains of the crossfade, frame for frame: a mid-sound head (0.25) and
/// the tail past the loop's end (0.5) are `g_in * 0.25 + g_out * 0.5` on each
/// of the 240 frames after the wrap, in mono and stereo, at any block length,
/// and the loop's start alone from there.
///
/// Mutation (run): a linear `g_in = x` in `crossfade_wraps` → fails (the
/// curves differ by up to 0.2 of the gap). Mutation (run): the gain index one
/// frame late (`wrap.ago + at - run.start + 1`) → fails (a step of 1.6e-3
/// against a tolerance of 1e-6).
fn the_gains_are_the_equal_power_curve_frame_for_frame(host: Host) {
    for channels in [1, 2] {
        for block in [7, 64, 100, 480, 1000] {
            let w = steps(channels, 0.25, 0.5);
            let mut rig = Rig::new(host, Some(region(LOOP_START_BEATS, LOOP_BEATS)), 1000);
            rig.place(1, &w, 0.0, None);
            let out = rig.run(WRAP + FADE + 16, block);
            for (c, lane) in out.iter().enumerate() {
                let off = if channels == 2 { 0.1 * c as f32 } else { 0.0 };
                assert_eq!(lane[WRAP - 1], 0.25 + off);
                for k in 0..FADE {
                    let (g_in, g_out) = equal_power(k);
                    let want = g_in * (0.25 + off) + g_out * (0.5 + off);
                    assert!(
                        (lane[WRAP + k] - want).abs() < 1e-6,
                        "{channels} ch, block {block}, lane {c}, frame {k}: {} vs {want}",
                        lane[WRAP + k]
                    );
                }
                for &s in &lane[WRAP + FADE..] {
                    assert_eq!(s, 0.25 + off);
                }
            }
        }
    }
}

/// A tail that ends inside the fade (the voice is 100 frames longer than the
/// loop) leaves the head to finish the ramp it began: no pop to unity.
///
/// Mutation (run): `crossfade_wraps` skipping the frames without a tail
/// (`continue` on `positions[i].is_none()`) → the head is unscaled from frame
/// 100 and steps from 0.59 times the head to the head → fails.
fn a_tail_that_ends_inside_the_fade_leaves_the_head_to_finish_its_ramp(host: Host) {
    let w = steps(1, 0.25, 0.5);
    let mut rig = Rig::new(host, Some(region(LOOP_START_BEATS, LOOP_BEATS)), 1000);
    rig.place(1, &w, 0.0, Some(LOOP_BEATS + 100.0 / FPB as f64));
    let out = rig.run(WRAP + FADE + 16, 1000);
    for k in 0..FADE {
        let (g_in, g_out) = equal_power(k);
        let tail = if k < 100 { g_out * 0.5 } else { 0.0 };
        let want = g_in * 0.25 + tail;
        assert!(
            (out[0][WRAP + k] - want).abs() < 1e-6,
            "frame {k}: {} vs {want}",
            out[0][WRAP + k]
        );
    }
}

/// The head is the clip's own start (the voice begins on the loop's start, or
/// a few frames into the fade): it keeps unity, so the render after the wrap
/// is the first pass's attack plus the tail's fade-out, whose gain is the
/// curve's `g_out`. Subtract the tail and what is left is the first pass.
///
/// Mutation (run): `begins_with_loop` always `false` → the head is ramped by
/// `g_in` → fails. Mutation (run): `begins_with_loop` always `true` → the
/// sustained-tone tests (`a_tone_has_no_cut…`, the energy and the gain
/// tests) fail.
fn a_clip_that_starts_with_the_loop_keeps_its_attack_at_unity(host: Host) {
    for lag in [0usize, 100] {
        let w = wave(1, 4 * FPB, tone);
        let mut rig = Rig::new(host, Some(region(LOOP_START_BEATS, LOOP_BEATS)), 1000);
        rig.place(1, &w, LOOP_START_BEATS + lag as f64 / FPB as f64, None);
        let out = rig.run(WRAP + FADE + 16, 480);
        // The first pass: the clip from its first frame.
        for k in 0..FADE + 16 {
            assert_eq!(out[0][LS + lag + k].to_bits(), tone(437.3, k).to_bits());
        }
        for k in 0..FADE {
            let (_, g_out) = equal_power(k);
            // The clip's tail, a loop's length on.
            let tail = tone(437.3, WRAP - LS + k - lag);
            let head = if k >= lag { tone(437.3, k - lag) } else { 0.0 };
            let got = out[0][WRAP + k];
            assert!(
                (got - g_out * tail - head).abs() < 1e-5,
                "lag {lag}, frame {k}: {got}"
            );
        }
        for k in FADE..FADE + 16 {
            assert_eq!(
                out[0][WRAP + k].to_bits(),
                tone(437.3, k - lag).to_bits(),
                "lag {lag}, frame {k}"
            );
        }
    }
}

/// An in-phase join (head and tail the same signal) sums the two gains, which
/// peak at root 2 mid-fade (+3.01 dB) and average 4/pi (+2.1 dB) against the
/// level of either. A head that is the clip's own start sums at unity: both
/// near full level at the join, the peak is the two together, twice the
/// tone's amplitude. Nothing clips or limits it.
///
/// Mutation (run): `EqualAmplitude` in `crossfade_wraps` → the gain sum is 1
/// everywhere → fails. Mutation (run): unity applied to a mid-sound head →
/// the sum at mid-fade is 1.5 rather than root 2 → fails.
fn an_in_phase_join_peaks_at_three_decibels_and_a_clip_start_at_twice_the_tone(host: Host) {
    // 440 Hz is a whole number of cycles over the 30 000 frame loop, so the
    // tail and the loop's start are the same samples.
    let cosine = |_, i: usize| AMP * (std::f64::consts::TAU * 440.0 * i as f64 / SR).cos() as f32;
    let w = wave(1, 4 * FPB, cosine);
    let mut rig = Rig::new(host, Some(region(LOOP_START_BEATS, LOOP_BEATS)), 1000);
    rig.place(1, &w, 0.0, None);
    let out = rig.run(WRAP + FADE, 1000);
    let (mut peak, mut mean) = (0.0f32, 0.0f32);
    for k in 0..FADE {
        let (g_in, g_out) = equal_power(k);
        let want = (g_in + g_out) * cosine(0.0, LS + k);
        assert!((out[0][WRAP + k] - want).abs() < 1e-5, "frame {k}");
        peak = peak.max(g_in + g_out);
        mean += (g_in + g_out) / FADE as f32;
    }
    assert!(
        (peak - std::f32::consts::SQRT_2).abs() < 1e-3,
        "peak {peak}"
    );
    assert!(
        (mean - 4.0 / std::f32::consts::PI).abs() < 5e-3,
        "mean {mean}"
    );

    let mut rig = Rig::new(host, Some(region(LOOP_START_BEATS, LOOP_BEATS)), 1000);
    rig.place(1, &w, LOOP_START_BEATS, None);
    let out = rig.run(WRAP + FADE, 1000);
    let top = out[0][WRAP..].iter().fold(0.0f32, |m, s| m.max(s.abs()));
    assert!(
        top > 2.0 * AMP * 0.9999 && top <= 2.0 * AMP * 1.0001,
        "peak {top}, twice the tone's {AMP} is {}",
        2.0 * AMP
    );
}

/// A tempo change or a loop edit inside a fade, and a new wrap during one,
/// leave the output finite and inside twice the tone's amplitude. The tail's
/// origin is re-derived from the new tempo with the wrap's loop length in
/// beats, so it jumps where the tempo changes (a glitch the size of the
/// change, for the rest of the fade); a loop edit does not move it. A
/// sample-rate change mid-fade keeps the fade's length in frames, so it runs
/// for that many frames at the new rate.
///
/// Mutation (run): `Wrap::left` returning `len` (the fade not shortened by
/// `ago`) → the fade outlasts its frames and the tail read runs off its span
/// → fails (a panic).
fn a_tempo_change_a_loop_edit_or_a_new_wrap_in_a_fade_stays_bounded(host: Host) {
    let w = wave(1, 4 * FPB, tone);
    for (tempo, edit) in [
        (90.0, Some(region(LOOP_START_BEATS, LOOP_BEATS))),
        (120.0, Some(region(LOOP_START_BEATS, 1.81))),
        (
            150.0,
            Some(region(LOOP_START_BEATS, LOOP_START_BEATS + 0.004)),
        ),
        (120.0, None),
    ] {
        let mut rig = Rig::new(host, Some(region(LOOP_START_BEATS, LOOP_BEATS)), 1000);
        rig.place(1, &w, 0.0, None);
        rig.run(WRAP - 50, 1000);
        let mut out = rig.block_changed(400, 150, tempo, edit);
        out[0].extend(rig.run(2000, 333)[0].iter());
        assert!(
            out[0]
                .iter()
                .all(|s| s.is_finite() && s.abs() <= 2.0 * AMP + 1e-3),
            "tempo {tempo}, edit {edit:?}"
        );
    }
}

/// The unity rule is decided to the frame at the loop's start: a voice one
/// frame before it is mid-sound (equal power), on it or one frame after it
/// is the clip's own start (unity). The source steps from 0.25 to 0.5 so the
/// head is 0.25 and the tail 0.5 whichever way the voice is placed.
///
/// Not covered: a tempo change inside the fade can flip the rule for a voice
/// within about a frame of the loop's start (the frame it is judged on is
/// re-derived at the new tempo), and the head's gain then jumps by up to the
/// gap between the two curves, 1 - g_in, for the rest of the fade. It is
/// accepted: the voice is already on the edge of the rule, and a tempo change
/// inside a 5 ms window is a glitch of its own.
///
/// Mutation (run): `begins_with_loop` testing the loop's start frame itself
/// (no `- 1 / fpb`) → the voice a frame before it reads as unity → fails.
fn the_unity_rule_is_decided_to_the_frame_at_the_loop_start(host: Host) {
    for (lag, own) in [(-1i32, false), (0, true), (1, true)] {
        let w = steps_at(1, 0.25, 0.5, LS - 100);
        let mut rig = Rig::new(host, Some(region(LOOP_START_BEATS, LOOP_BEATS)), 1000);
        rig.place(1, &w, LOOP_START_BEATS + f64::from(lag) / FPB as f64, None);
        let out = rig.run(WRAP + FADE, 1000);
        for k in 0..FADE {
            let (g_in, g_out) = equal_power(k);
            let head = if lag > 0 && k < lag as usize {
                0.0
            } else {
                0.25
            };
            let head = if own { head } else { g_in * head };
            let want = head + g_out * 0.5;
            assert!(
                (out[0][WRAP + k] - want).abs() < 1e-6,
                "lag {lag}, frame {k}: {} vs {want}",
                out[0][WRAP + k]
            );
        }
    }
}

/// A voice with a loop of its own, under a transport wrap, is judged by its
/// window like any other: placed before the loop's start it is mid-sound, so
/// the equal-power rule, and the output stays inside the sum of the two
/// levels. The voice's loop ends 100 frames into the fade, so the tail
/// crosses the voice's own wrap there; only the frames before its crossfade
/// are pinned.
///
/// Mutation (run): `begins_with_loop` always `true` → the head is unity →
/// fails.
fn a_voice_with_its_own_loop_is_judged_by_its_window(host: Host) {
    let w = steps(1, 0.25, 0.5);
    let mut rig = Rig::new(host, Some(region(LOOP_START_BEATS, LOOP_BEATS)), 1000);
    let play = Playback {
        loop_: super::memory_source::LoopSetting::On {
            start: tutti_types::SamplePosition(20_000.0),
            end: tutti_types::SamplePosition((WRAP + 100) as f64),
            crossfade_frames: 64,
        },
        ..Playback::default()
    };
    rig.place_with(1, &w, 0.0, None, play);
    let out = rig.run(WRAP + FADE, 1000);
    for k in 0..30 {
        let (g_in, g_out) = equal_power(k);
        let want = g_in * 0.25 + g_out * 0.5;
        assert!((out[0][WRAP + k] - want).abs() < 1e-6, "frame {k}");
    }
    assert!(out[0][WRAP..].iter().all(|s| s.is_finite() && *s <= 0.75));
}

on_each_host!(
    a_non_finite_gain_renders_silence,
    a_tone_has_no_cut_at_the_wrap_at_any_block_length,
    the_fade_begins_on_the_wrap_frame_and_changes_nothing_before_it,
    the_fade_holds_the_energy_of_unrelated_material,
    a_voice_that_is_not_playing_across_the_wrap_is_untouched,
    a_tail_with_no_loop_start_under_it_fades_out,
    a_voice_that_begins_after_the_loop_ends_leaves_no_ghost,
    a_loop_shorter_than_the_fade_fades_over_half_a_pass,
    a_seek_or_a_stop_during_the_fade_ends_it,
    the_gains_are_the_equal_power_curve_frame_for_frame,
    a_tail_that_ends_inside_the_fade_leaves_the_head_to_finish_its_ramp,
    a_clip_that_starts_with_the_loop_keeps_its_attack_at_unity,
    an_in_phase_join_peaks_at_three_decibels_and_a_clip_start_at_twice_the_tone,
    a_tempo_change_a_loop_edit_or_a_new_wrap_in_a_fade_stays_bounded,
    the_unity_rule_is_decided_to_the_frame_at_the_loop_start,
    a_voice_with_its_own_loop_is_judged_by_its_window,
);
