//! A placed memory voice across a transport loop wrap: the tail it was
//! playing past the loop's end fades out under the loop's start, over
//! `clock::WRAP_FADE`, instead of cutting.
//!
//! The source is a pure tone, so the render is continuous but for the wrap:
//! the largest step between neighbouring samples is bounded analytically, and
//! a cut shows as a step of the tone's amplitude.

use std::sync::Arc;

use tutti_graph::{Env, Node, Offset, Prepare, Transport, TransportChanges, contract};
use tutti_io::Wave;
use tutti_types::{
    Beat, BeatDuration, Bpm, ChannelLayout, Frame, FrameClock, LoopRange, SampleRate, Samples,
};

use super::clock::WRAP_FADE;
use super::memory_source::MemorySource;
use super::pool::VoicePool;
use super::types::{Playback, SlotId, Voice, VoiceSource};

const SR: f64 = 48_000.0;
/// Frames in a beat at 120 BPM.
const FPB: usize = 24_000;
/// The loop: 1.3 beats, so no tone divides it.
const LOOP_BEATS: f64 = 1.3;
const WRAP: usize = 31_200;
const AMP: f32 = 0.5;
const FADE: usize = 240;

fn tone(freq: f64, i: usize) -> f32 {
    AMP * (std::f64::consts::TAU * freq * i as f64 / SR).sin() as f32
}

/// A wave of `channels` tones (437.3 Hz, then 311.7 Hz), `len` frames.
fn wave(channels: usize, len: usize, tone_at: impl Fn(f64, usize) -> f32) -> Arc<Wave> {
    let mut w = Wave::new(channels, SR);
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
    LoopRange::new(start, end).expect("a loop")
}

/// A pool of placed voices behind a host clock that loops.
struct Rig {
    pool: VoicePool,
    clock: FrameClock,
    looping: Option<LoopRange>,
    frame: u64,
}

impl Rig {
    fn new(looping: Option<LoopRange>, max_block: usize) -> Self {
        let mut pool = VoicePool::with_channels(None, ChannelLayout::STEREO).expect("stereo");
        pool.prepare(&Prepare::new(SampleRate(SR), Samples(max_block)));
        Self {
            pool,
            clock: FrameClock::new(Beat::new(0.0), Bpm::new(120.0), SampleRate(SR)),
            looping,
            frame: 0,
        }
    }

    /// A voice placed at `start` beats for `duration` (to the end of the wave
    /// when `None`).
    fn place(&mut self, id: u128, wave: &Arc<Wave>, start: f64, duration: Option<f64>) {
        let mut source = MemorySource::placed(
            Arc::clone(wave),
            Beat::new(start),
            duration.map(BeatDuration::new),
        );
        source.set_render_rate(SampleRate(SR));
        self.pool.insert_voice(
            SlotId(id),
            Voice {
                source: VoiceSource::Memory(source),
                play: Playback::default(),
                channel_index: None,
            },
        );
    }

    fn transport(&self, playing: bool) -> Transport {
        let looping = self.looping.map(|r| tutti_graph::LoopRange {
            start: r.start(),
            end: r.end(),
        });
        Transport::new(playing, Bpm(120.0), self.clock.beat(), looping)
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
        let out = contract::drive_input(&mut self.pool, &env, &[], &[], &[]).audio;
        self.frame += len as u64;
        self.clock.advance(Samples(len), self.looping);
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
#[test]
fn a_tone_has_no_cut_at_the_wrap_at_any_block_length() {
    for channels in [1, 2] {
        for block in [64, 100, 480, 1000] {
            let w = wave(channels, 4 * FPB, tone);
            let mut rig = Rig::new(Some(region(0.0, LOOP_BEATS)), 1000);
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
#[test]
fn the_fade_begins_on_the_wrap_frame_and_changes_nothing_before_it() {
    let w = wave(1, 4 * FPB, tone);
    let mut rig = Rig::new(Some(region(0.0, LOOP_BEATS)), 1000);
    rig.place(1, &w, 0.0, None);
    let out = rig.run(WRAP + FADE + 8, 480);
    for (i, s) in out[0].iter().enumerate().take(WRAP) {
        assert_eq!(s.to_bits(), tone(437.3, i).to_bits(), "frame {i}");
    }
    let x = 1.0 / (FADE + 1) as f32 * std::f32::consts::FRAC_PI_2;
    let want = x.sin() * tone(437.3, 0) + x.cos() * tone(437.3, WRAP);
    assert!(
        (out[0][WRAP] - want).abs() < 1e-5,
        "{} vs {want}",
        out[0][WRAP]
    );
    // Past the fade, the loop's start alone.
    for k in 0..8 {
        assert_eq!(
            out[0][WRAP + FADE + k].to_bits(),
            tone(437.3, FADE + k).to_bits()
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
#[test]
fn the_fade_holds_the_energy_of_unrelated_material() {
    let w = wave(1, 4 * FPB, |_, i| {
        let f = if i < WRAP { 1_000.0 } else { 3_000.0 };
        tone(f, i)
    });
    let mut rig = Rig::new(Some(region(0.0, LOOP_BEATS)), 1000);
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
#[test]
fn a_voice_that_is_not_playing_across_the_wrap_is_untouched() {
    let w = wave(1, 4 * FPB, tone);
    // One beat of a loop of 1.3: it ends 0.3 beats before the wrap.
    let mut rig = Rig::new(Some(region(0.0, LOOP_BEATS)), 1000);
    rig.place(1, &w, 0.0, Some(1.0));
    let out = rig.run(WRAP + 2 * FADE, 1000);
    for (i, s) in out[0].iter().enumerate().take(FPB) {
        assert_eq!(s.to_bits(), tone(437.3, i).to_bits());
    }
    assert!(out[0][FPB + 8..WRAP].iter().all(|&s| s == 0.0));
    for k in 0..2 * FADE {
        assert_eq!(out[0][WRAP + k].to_bits(), tone(437.3, k).to_bits(), "{k}");
    }
}

/// A voice that stops sounding at the loop's end fades out under a loop
/// start it does not play: it does not cut either.
///
/// Mutation (run): the tail read gated out when the head is silent
/// (`!sounds` returning `Silent` before the tail) → the voice cuts → fails.
#[test]
fn a_tail_with_no_loop_start_under_it_fades_out() {
    let w = wave(1, 4 * FPB, tone);
    // Placed at beat 1 for ever: the loop's start (beat 0) is before it.
    let mut rig = Rig::new(Some(region(0.0, LOOP_BEATS)), 1000);
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
#[test]
fn a_voice_that_begins_after_the_loop_ends_leaves_no_ghost() {
    for start in [LOOP_BEATS, LOOP_BEATS + 0.005] {
        let w = wave(1, 4 * FPB, tone);
        let mut rig = Rig::new(Some(region(0.0, LOOP_BEATS)), 1000);
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
#[test]
fn a_loop_shorter_than_the_fade_fades_over_half_a_pass() {
    // 0.005 beats: 120 frames.
    let w = wave(1, FPB, tone);
    let mut rig = Rig::new(Some(region(0.0, 0.005)), 1000);
    rig.place(1, &w, 0.0, None);
    let out = rig.run(1000, 1000);
    let bound = step_bound(437.3, 60);
    let step = max_step(&out[0], 130, 1000);
    assert!(step <= bound, "step {step} > {bound}");
    for pass in 1..8 {
        for k in 60..120 {
            assert_eq!(out[0][120 * pass + k].to_bits(), tone(437.3, k).to_bits());
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
#[test]
fn a_seek_or_a_stop_during_the_fade_ends_it() {
    let w = wave(1, 4 * FPB, tone);
    let mut rig = Rig::new(Some(region(0.0, LOOP_BEATS)), 1000);
    rig.place(1, &w, 0.0, None);
    rig.run(WRAP - 100, 1000);
    // Block of 400: the wrap is at frame 100, a seek to beat 2 at 150.
    let to = Transport::new(true, Bpm(120.0), Beat(2.0), None);
    let out = rig.block(400, Some((150, to)));
    for k in 0..250 {
        assert_eq!(
            out[0][150 + k].to_bits(),
            tone(437.3, 2 * FPB + k).to_bits(),
            "frame {k} after the seek"
        );
    }

    let mut rig = Rig::new(Some(region(0.0, LOOP_BEATS)), 1000);
    rig.place(1, &w, 0.0, None);
    rig.run(WRAP - 100, 1000);
    let stopped = Transport::new(false, Bpm(120.0), Beat(0.01), None);
    let out = rig.block(400, Some((150, stopped)));
    assert!(out[0][150..].iter().all(|&s| s == 0.0));
    // Rolling again where it stands: the loop's start alone.
    let at = rig.clock.beat();
    let restart = Transport::new(true, Bpm(120.0), at, None);
    let env = Env {
        frame: Frame(rig.frame),
        sample_rate: SampleRate(SR),
        block_len: Samples(64),
        transport: restart,
        changes: TransportChanges::NONE,
        play: tutti_graph::PlayClock::ZERO,
    };
    let out = contract::drive_input(&mut rig.pool, &env, &[], &[], &[]).audio;
    let first = (at.get() * FPB as f64).round() as usize;
    for (k, s) in out[0].iter().enumerate() {
        assert_eq!(s.to_bits(), tone(437.3, first + k).to_bits());
    }
}
