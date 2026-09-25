//! Reverse playback and loop crossfade, end to end.
//!
//! Two of the six `Playback` axes had no end-to-end coverage at all.
//! `Direction::Reverse` appeared in exactly two source files and no test;
//! `LoopSetting::On` appeared in no test or example. Both are distinct read
//! paths with their own index arithmetic, and both are the kind of thing that
//! sounds "roughly right" while being wrong — a reversed read off by one plays
//! fine, a crossfade that dips leaves an audible hole at every loop point.
//!
//! # Ramps, not tones
//!
//! The material here is a linear ramp whose value *is* its own sample index
//! (scaled). That makes every assertion a statement about position:
//! `output == f(index)` says exactly which sample was read, so a reversed read
//! that is off by one, or a loop that wraps to the wrong place, is a wrong
//! *number* rather than a plausible waveform. A tone cannot do this — every
//! period looks like every other, so an off-by-one is invisible.
//!
//! The one exception is the crossfade seam test, which needs a signal whose
//! discontinuity is audible; it uses two constants so the seam is a step.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tutti_core::BufferVec;
use tutti_core::{Amplitude, AudioUnit, Beat, Bpm, ChannelLayout, SamplePosition, Timeline};
use tutti_io::Wave;
use tutti_sampler::{
    Direction, LoopSetting, MemorySource, MemorySourceConfig, Playback, SlotId, Voice, VoicePool,
    VoiceSource,
};

const SR: f64 = 48_000.0;
const BLOCK: usize = 64;

/// A ramp whose sample `i` holds `i / len` on both channels.
///
/// Monotonic and unique per sample, so a read position can be recovered from a
/// value: `index = value * len`. That is what turns "the output sounds right"
/// into "the output came from sample N".
fn ramp(len: usize) -> Arc<Wave> {
    let mut w = Wave::new(2, SR);
    for i in 0..len {
        let v = i as f32 / len as f32;
        w.push_frame(&[v, v]);
    }
    Arc::new(w)
}

/// Recover the source index a sample value came from, given the ramp length.
fn index_of(value: f32, len: usize) -> f64 {
    value as f64 * len as f64
}

/// A rolling transport advanced by hand, once per block.
///
/// Needed because reverse only reaches its code path on a **placed** voice:
/// `PlaybackSlot` derives the read position from the playhead
/// (`MemorySource::seated_position`), which is `None` without a timeline, and
/// the slot then emits silence. A free-running
/// voice never reaches `read_clip_sample_into` at all — which is how the first
/// draft of this file measured index 0 for every reversed read and looked like
/// an engine bug.
struct Clock {
    beat: AtomicU64,
    tempo: f64,
}

impl Clock {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            beat: AtomicU64::new(0f64.to_bits()),
            tempo: 120.0,
        })
    }

    fn advance(&self, samples: usize) {
        let beats = samples as f64 * self.tempo / 60.0 / SR;
        let now = f64::from_bits(self.beat.load(Ordering::Relaxed));
        self.beat.store((now + beats).to_bits(), Ordering::Relaxed);
    }
}

impl Timeline for Clock {
    fn beat(&self) -> Beat {
        Beat::new(f64::from_bits(self.beat.load(Ordering::Relaxed)))
    }
    fn tempo(&self) -> Bpm {
        Bpm::new(self.tempo)
    }
    fn is_rolling(&self) -> bool {
        true
    }
}

/// Drive a unit for `blocks` blocks, returning channel 0.
fn render(unit: &mut dyn AudioUnit, blocks: usize) -> Vec<f32> {
    let input = BufferVec::new(2);
    let mut output = BufferVec::new(2);
    let mut out = Vec::with_capacity(blocks * BLOCK);
    for _ in 0..blocks {
        unit.process(BLOCK, &input.buffer_ref(), &mut output.buffer_mut());
        let b = output.buffer_ref();
        for i in 0..BLOCK {
            out.push(b.at_f32(0, i));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Reverse
// ---------------------------------------------------------------------------

/// Build a free-running voice in a pool, at `direction`.
///
/// Reverse lives on `PlaybackSlot` (`read_clip_sample_into`), not on
/// `MemorySource` — the source has no direction verb, and `apply_direction` is
/// a deliberate no-op on the memory tier. So exercising it means going through a
/// pool, which is also the assembly a real render uses.
fn reversed_pool(wave: Arc<Wave>, direction: Direction) -> (VoicePool, Arc<Clock>) {
    let clock = Clock::new();
    // **Placed**, not free-running. `PlaybackSlot` reads `seated_position()`, which
    // needs a timeline; without one the slot returns early and emits silence, so
    // the reverse arm is never reached.
    let source = MemorySource::with_config(
        wave,
        MemorySourceConfig {
            channels: ChannelLayout::STEREO,
            timeline: Some(clock.clone() as Arc<dyn Timeline>),
            ..Default::default()
        },
    );

    let (mut pool, _handle) = VoicePool::new();
    pool.insert_voice(
        SlotId(1),
        Voice {
            source: VoiceSource::Memory(source),
            play: Playback {
                direction,
                gain: Amplitude::new(1.0),
                ..Default::default()
            },
            channel_index: None,
        },
    );
    (pool, clock)
}

/// Render a placed pool, advancing its clock once per block.
fn render_placed(pool: &mut VoicePool, clock: &Clock, blocks: usize) -> Vec<f32> {
    let input = BufferVec::new(2);
    let mut output = BufferVec::new(2);
    let mut out = Vec::with_capacity(blocks * BLOCK);
    for _ in 0..blocks {
        pool.process(BLOCK, &input.buffer_ref(), &mut output.buffer_mut());
        let b = output.buffer_ref();
        for i in 0..BLOCK {
            out.push(b.at_f32(0, i));
        }
        clock.advance(BLOCK);
    }
    out
}

/// Reverse playback must read the source backwards, from its last sample.
///
/// The implementation maps `pos -> len - 1 - pos`, so the first output frame is
/// the *last* source frame and the ramp comes out descending. Asserted on the
/// recovered index rather than on "it changed", because a reverse that started
/// at `len` (one past the end), or that reversed within the wrong length, still
/// produces a descending ramp.
#[test]
fn reverse_playback_reads_the_source_backwards() {
    const LEN: usize = 4096;
    let (mut pool, clock) = reversed_pool(ramp(LEN), Direction::Reverse);
    let out = render_placed(&mut pool, &clock, 8);

    // The first frame must be the LAST source sample: len-1-0 = len-1.
    let first = index_of(out[0], LEN);
    assert!(
        (first - (LEN - 1) as f64).abs() < 2.0,
        "reverse must start at the final sample ({}), but the first output frame \
         came from index {first:.1}",
        LEN - 1
    );

    // ...and it must descend by one source sample per output sample.
    let hundredth = index_of(out[100], LEN);
    let step = (first - hundredth) / 100.0;
    assert!(
        (step - 1.0).abs() < 0.05,
        "reverse must advance one source sample per output sample, measured {step:.4}"
    );

    // Monotonically decreasing — no wrap, no stall, no forward segment.
    for (i, w) in out[..400].windows(2).enumerate() {
        assert!(
            w[1] <= w[0] + 1e-6,
            "reverse output rose at sample {i}: {} -> {}",
            w[0],
            w[1]
        );
    }
}

/// Forward is the control, and the two must be mirror images.
///
/// Without this, `reverse_playback_reads_the_source_backwards` cannot
/// distinguish "reverse works" from "the ramp is upside down for some unrelated
/// reason". Comparing the two directions on the same source pins the
/// relationship rather than each end separately.
#[test]
fn forward_and_reverse_are_mirror_images_of_each_other() {
    const LEN: usize = 4096;
    let (mut fwd_pool, fwd_clock) = reversed_pool(ramp(LEN), Direction::Forward);
    let (mut rev_pool, rev_clock) = reversed_pool(ramp(LEN), Direction::Reverse);
    let fwd = render_placed(&mut fwd_pool, &fwd_clock, 4);
    let rev = render_placed(&mut rev_pool, &rev_clock, 4);

    // Forward starts at index 0, reverse at index len-1; their indices must sum
    // to len-1 at every sample.
    for i in [0usize, 1, 50, 199] {
        let sum = index_of(fwd[i], LEN) + index_of(rev[i], LEN);
        assert!(
            (sum - (LEN - 1) as f64).abs() < 2.0,
            "at output {i}: forward read index {:.1} and reverse read {:.1}, \
             which sum to {sum:.1} — the mirror is about {}",
            index_of(fwd[i], LEN),
            index_of(rev[i], LEN),
            LEN - 1
        );
    }
}

/// Reverse must not read past either end of the source.
///
/// `(len - 1.0 - pos).max(0.0)` clamps the low end. Rendering well past the
/// source length drives `pos` beyond `len`, where the clamp is the only thing
/// standing between the read and a negative index.
#[test]
fn reverse_does_not_read_outside_the_source() {
    const LEN: usize = 512;
    let (mut pool, clock) = reversed_pool(ramp(LEN), Direction::Reverse);
    // 64 blocks = 4096 samples against a 512-sample source: eight times over.
    let out = render_placed(&mut pool, &clock, 64);

    for (i, &s) in out.iter().enumerate() {
        assert!(
            s.is_finite(),
            "sample {i} is {s} — reverse read outside the source"
        );
        // The ramp is [0, 1); a read past either end would leave this range.
        assert!(
            (-1e-6..=1.0).contains(&s),
            "sample {i} is {s}, outside the source's own [0, 1) range"
        );
    }
}

/// **Reverse falls silent past the source's first frame** (doc 013 follow-up
/// S1, the memory tier), as forward falls silent past its last: the mirror of
/// a read past the end is a read before the start. Holding frame 0 there
/// played the source's first sample as DC for as long as the voice's window
/// stayed open.
///
/// The source here starts at a non-zero value, so a held frame 0 is not
/// silence by accident.
///
/// Mutation (run): the silence removed from `MemorySource::read_placed_into`'s
/// reverse arm (the old `(len - 1 - pos).max(0.0)` alone) → frame 0 held from
/// output `LEN` on → fails.
#[test]
fn reverse_past_the_first_frame_is_silent() {
    const LEN: usize = 512;
    let mut w = Wave::new(2, SR);
    for i in 0..LEN {
        let v = (i + 1) as f32 / LEN as f32;
        w.push_frame(&[v, v]);
    }
    let (mut pool, clock) = reversed_pool(Arc::new(w), Direction::Reverse);
    let out = render_placed(&mut pool, &clock, 16);

    for (k, &s) in out[..LEN].iter().enumerate() {
        let want = (LEN - k) as f32 / LEN as f32;
        assert!(
            (s - want).abs() < 1e-6,
            "output {k} read {s}, want the source's frame {} ({want})",
            LEN - 1 - k
        );
    }
    for (k, &s) in out.iter().enumerate().skip(LEN) {
        assert_eq!(s, 0.0, "output {k}, past the source's first frame, is {s}");
    }
}

// ---------------------------------------------------------------------------
// Loop
// ---------------------------------------------------------------------------

/// A sine of period [`PERIOD`] frames, `len` long, on both channels.
fn sine(len: usize) -> Arc<Wave> {
    let mut w = Wave::new(2, SR);
    for i in 0..len {
        let v = (std::f64::consts::TAU * i as f64 / PERIOD as f64).sin() as f32;
        w.push_frame(&[v, v]);
    }
    Arc::new(w)
}

/// The period of [`sine`], in frames.
const PERIOD: usize = 100;

/// A loop on [`sine`] that clicks when cut hard: it starts on a rising zero
/// crossing and ends a quarter period later in the cycle, so the frame before
/// the wrap is at the crest and the one after it at zero — a jump of the whole
/// amplitude. 256 frames of fade fit in the 1000 before the start.
const SEAM: (f64, f64, usize) = (1_000.0, 3_025.0, 256);

/// The largest step [`sine`] takes between two frames: `2 sin(π / PERIOD)`,
/// its slope at a zero crossing. A loop that plays continuously takes no
/// larger one anywhere, the seam included; `f32` rounding gets a hair.
fn sine_step() -> f32 {
    (2.0 * (std::f64::consts::PI / PERIOD as f64).sin()) as f32 + 1e-5
}

/// The largest step between consecutive frames of `x`, and where.
fn largest_step(x: &[f32]) -> (f32, usize) {
    x.windows(2)
        .enumerate()
        .map(|(i, w)| ((w[1] - w[0]).abs(), i))
        .fold((0.0, 0), |a, b| if b.0 > a.0 { b } else { a })
}

/// **A crossfaded loop is continuous at its wrap** (doc 013 follow-up S3, the
/// memory tier): on a sine whose loop points would click cut hard, no step in
/// the output is larger than the sine's own, round the loop three times. The
/// fade leads into the loop's start — the last blended frame is almost all
/// the frame before `start`, and the wrap plays `start` next — so the seam is
/// the sine's own step.
///
/// Free-running, and placed (a placed voice loops as a disk voice's stream
/// does): the two read the loop through the same `LoopSpan`.
///
/// The hard loop is asserted to click first, so the loop points have teeth.
///
/// Mutation (run): `LoopSpan::fade_at`'s lead-in `start + k` (the old head
/// replay: the fade blends toward the loop's first frames, then the wrap plays
/// them again) → a step far above the sine's own at the wrap → fails.
/// Mutation (run): the fade dropped (`LoopTap::fade` always `None`) → the hard
/// cut → fails. Mutation (run): `MemorySource::read_placed_into` ignoring the
/// loop → the placed voice plays straight on → fails. Not pinned here: the
/// fade weight `k / fade` (the last blended frame keeps `1 / fade` of the
/// tail) stays under the bound at this length; `loop_span`'s own tests pin
/// the weight.
#[test]
fn a_crossfaded_loop_is_continuous_at_its_wrap() {
    const LEN: usize = 4_000;
    let (start, end, fade) = SEAM;

    let hard = render(&mut looping_source(sine(LEN), start, end, 0), 150);
    let (step, at) = largest_step(&hard);
    assert!(
        step > 0.9,
        "the hard loop does not click ({step} at {at}): the loop points have no teeth"
    );

    let free = {
        // From the file's start, as the placed voice below plays it.
        let mut source = looping_source(sine(LEN), start, end, fade);
        source.trigger_at(SamplePosition(0.0));
        render(&mut source, 150)
    };
    let placed = {
        let clock = Clock::new();
        let mut source = MemorySource::with_config(
            sine(LEN),
            MemorySourceConfig {
                channels: ChannelLayout::STEREO,
                timeline: Some(clock.clone() as Arc<dyn Timeline>),
                ..Default::default()
            },
        );
        source.set_loop_setting(LoopSetting::On {
            start: SamplePosition(start),
            end: SamplePosition(end),
            crossfade_frames: fade,
        });
        let input = BufferVec::new(2);
        let mut output = BufferVec::new(2);
        let mut out = Vec::new();
        for _ in 0..150 {
            source.process(BLOCK, &input.buffer_ref(), &mut output.buffer_mut());
            let b = output.buffer_ref();
            out.extend((0..BLOCK).map(|i| b.at_f32(0, i)));
            clock.advance(BLOCK);
        }
        out
    };
    // 150 blocks = 9600 frames: 3025 to the first wrap, then three passes.
    for (what, out) in [("free-running", &free), ("placed", &placed)] {
        let (step, at) = largest_step(out);
        assert!(
            step <= sine_step(),
            "{what}: a step of {step} at output {at}, larger than the sine's own {}",
            sine_step()
        );
    }
    assert_eq!(free, placed, "the two read the loop the same way");
}

/// A free-running looping source over `[start, end)` with `xfade` frames of
/// crossfade.
///
/// Looping is driven by the source's own position accumulator, not by a
/// playhead — the wrap happens in `MemorySource`'s free-running arm. So this
/// deliberately does *not* attach a timeline.
fn looping_source(wave: Arc<Wave>, start: f64, end: f64, xfade: usize) -> MemorySource {
    let mut source = MemorySource::with_config(
        wave,
        MemorySourceConfig {
            channels: ChannelLayout::STEREO,
            ..Default::default()
        },
    );
    source.set_loop_setting(LoopSetting::On {
        start: SamplePosition(start),
        end: SamplePosition(end),
        crossfade_frames: xfade,
    });
    source.trigger_at(SamplePosition(start));
    source.play();
    source
}

/// A loop must wrap back into its own range, and stay there.
///
/// The wrap is a modulo rather than a subtraction — deliberately, per the source
/// comment, because at high varispeed one advance can overshoot a short loop by
/// more than its own length and the subtraction form would escape permanently.
/// This renders far past the loop end to check the range holds indefinitely, not
/// just on the first pass.
#[test]
fn a_looping_source_stays_inside_its_loop_range() {
    const LEN: usize = 8192;
    let (start, end) = (1000.0f64, 3000.0f64);
    // No crossfade: this test is about the wrap, and a fade would blend
    // out-of-range material into the seam and blur the bound.
    let mut source = looping_source(ramp(LEN), start, end, 0);

    // 200 blocks = 12800 samples over a 2000-sample loop: six times around.
    let out = render(&mut source, 200);

    for (i, &s) in out.iter().enumerate() {
        let idx = index_of(s, LEN);
        // One sample of slack at each edge for interpolation at the boundary.
        assert!(
            idx >= start - 1.0 && idx <= end + 1.0,
            "output {i} came from index {idx:.1}, outside the loop [{start}, {end})"
        );
    }

    // And it must actually have wrapped — a source that stopped at `end` would
    // also satisfy the bound above.
    let wraps = out.windows(2).filter(|w| w[1] < w[0] - 0.1).count();
    assert!(
        wraps >= 5,
        "expected ~6 wraps over 12800 samples of a 2000-sample loop, saw {wraps}"
    );
}

/// The loop must return to `start`, not to zero or to some other point.
///
/// A wrap that reset to 0 instead of `loop_start` still loops, still stays
/// bounded below `end`, and still sounds periodic — it just plays the wrong
/// material. Only checking the value at the wrap catches it.
#[test]
fn a_loop_wraps_to_its_start_not_to_zero() {
    const LEN: usize = 8192;
    let (start, end) = (2000.0f64, 4000.0f64);
    let mut source = looping_source(ramp(LEN), start, end, 0);
    let out = render(&mut source, 100);

    // Find the first wrap: a large downward step.
    let wrap_at = out
        .windows(2)
        .position(|w| w[1] < w[0] - 0.1)
        .expect("the source never wrapped");

    let landed = index_of(out[wrap_at + 1], LEN);
    assert!(
        (landed - start).abs() < 2.0,
        "the loop wrapped to index {landed:.1}, expected {start} — \
         wrapping to 0 would play the wrong region while still sounding periodic"
    );
}

/// A crossfaded loop must not leave a hole at the seam.
///
/// This is the property that matters and the one nothing checked. The crossfade
/// is **linear** (`fade_out = 1-t`, `fade_in = t`), not equal-power, so on
/// *uncorrelated* material the seam dips ~3 dB by construction. On the material
/// used here — a constant either side of the loop point — a linear fade is
/// exactly flat, so any dip is a defect rather than the known trade-off.
///
/// Constants rather than a ramp: the question is "does the level hold across the
/// join", and a ramp's own slope would be indistinguishable from a fade.
#[test]
fn a_crossfaded_loop_holds_its_level_across_the_seam() {
    const LEN: usize = 8192;
    let (start, end) = (1000.0f64, 3000.0f64);
    const XFADE: usize = 256;

    // Constant 0.5 everywhere: the pre-loop tail and the loop start carry the
    // same value, so a correct linear crossfade between them is flat at 0.5.
    let mut w = Wave::new(2, SR);
    for _ in 0..LEN {
        w.push_frame(&[0.5f32, 0.5f32]);
    }
    let mut source = looping_source(Arc::new(w), start, end, XFADE);

    let out = render(&mut source, 150);

    // Skip the first block: the source starts mid-range and the very first
    // frames are the read settling, not a seam.
    for (i, &s) in out.iter().enumerate().skip(BLOCK) {
        assert!(
            (s - 0.5).abs() < 0.02,
            "sample {i} is {s}, expected 0.5 — the crossfade leaves a \
             {:.1}% hole at the loop seam",
            (0.5 - s) / 0.5 * 100.0
        );
    }
}

/// A hard loop (zero crossfade) is legal and must not be silently faded.
///
/// `crossfade_frames: 0` is documented as "hard loop, no crossfade". A stage
/// that treated 0 as "use the default" would round-trip fine and quietly change
/// the sound of every hard loop.
#[test]
fn a_zero_length_crossfade_is_a_hard_loop() {
    const LEN: usize = 8192;
    let (start, end) = (1000.0f64, 3000.0f64);
    let mut source = looping_source(ramp(LEN), start, end, 0);
    let out = render(&mut source, 100);

    let wrap_at = out
        .windows(2)
        .position(|w| w[1] < w[0] - 0.1)
        .expect("the source never wrapped");

    // A hard loop steps discontinuously from the loop end straight to the loop
    // start. With a crossfade the transition would be spread over its length,
    // so the single-sample step would be much smaller.
    let step = out[wrap_at] - out[wrap_at + 1];
    let expected = (end - start) / LEN as f64;
    assert!(
        (step as f64 - expected).abs() < 0.02,
        "a hard loop must step the full loop length at the wrap \
         (expected {expected:.4}, measured {step:.4}) — a fade was applied"
    );
}

/// Looping and reverse compose without reading outside the source.
///
/// The two features touch different code — `PlaybackSlot` reverses the read,
/// `MemorySource` wraps the position — and nothing exercised them together.
///
/// # Reversed, and therefore not looping
///
/// Worth stating, because the combination is narrower than it looks. A placed
/// voice loops going forward (`MemorySource::read_placed_into`, as a disk
/// voice's stream loops), but a reversed read ignores the loop, as the
/// butler's reverse refill does — so the loop setting is inert here.
///
/// That is worth knowing rather than asserting around: this test pins that the
/// combination is *safe* (bounded, finite, audible) rather than claiming it
/// loops. A test asserting wraps would fail for a design reason, not a defect.
#[test]
fn a_reversed_voice_with_a_loop_set_stays_bounded() {
    const LEN: usize = 4096;
    let clock = Clock::new();
    let mut source = MemorySource::with_config(
        ramp(LEN),
        MemorySourceConfig {
            channels: ChannelLayout::STEREO,
            timeline: Some(clock.clone() as Arc<dyn Timeline>),
            ..Default::default()
        },
    );
    source.set_loop_setting(LoopSetting::On {
        start: SamplePosition(500.0),
        end: SamplePosition(2500.0),
        crossfade_frames: 0,
    });

    let (mut pool, _handle) = VoicePool::new();
    pool.insert_voice(
        SlotId(1),
        Voice {
            source: VoiceSource::Memory(source),
            play: Playback {
                direction: Direction::Reverse,
                gain: Amplitude::new(1.0),
                ..Default::default()
            },
            channel_index: None,
        },
    );

    let out = render_placed(&mut pool, &clock, 100);
    for (i, &s) in out.iter().enumerate() {
        assert!(
            s.is_finite() && (-1e-6..=1.0).contains(&s),
            "sample {i} is {s} — a reversed voice with a loop set read outside \
             the source"
        );
    }
    let peak = out.iter().fold(0.0f32, |a, &b| a.max(b.abs()));
    assert!(
        peak > 0.01,
        "a reversed voice with a loop set produced silence"
    );
}

/// A one-shot source stops at the end rather than looping or reading past it.
///
/// The `LoopMode::OneShot` arm of the same wrap logic. Its failure mode is the
/// mirror of a broken loop: instead of wrapping it should clear `playing` and
/// hold position at the end.
#[test]
fn a_one_shot_source_stops_at_the_end() {
    const LEN: usize = 2048;
    let source = MemorySource::with_config(
        ramp(LEN),
        MemorySourceConfig {
            channels: ChannelLayout::STEREO,
            ..Default::default()
        },
    );
    source.trigger_at(SamplePosition(0.0));
    source.play();
    let mut source = source;

    // Well past the source length.
    let out = render(&mut source, 100);

    // Once past the end the output must be silent, not wrapped material.
    let tail = &out[LEN + BLOCK..];
    for (i, &s) in tail.iter().enumerate() {
        assert!(
            s.abs() < 1e-6,
            "sample {} past the end is {s}, expected silence — \
             a one-shot must not wrap",
            i + LEN + BLOCK
        );
    }
}

/// A single-channel frame must work for both features.
///
/// The crossfade's per-channel loop and the reverse read both index by channel,
/// and width 1 is the edge the stereo-shaped code is least likely to have been
/// tried at.
#[test]
fn reverse_and_loop_work_at_mono_width() {
    const LEN: usize = 2048;
    let mut w = Wave::new(1, SR);
    for i in 0..LEN {
        w.push_frame(&[i as f32 / LEN as f32]);
    }
    let mut source = MemorySource::with_config(
        Arc::new(w),
        MemorySourceConfig {
            channels: ChannelLayout::MONO,
            ..Default::default()
        },
    );
    source.set_loop_setting(LoopSetting::On {
        start: SamplePosition(200.0),
        end: SamplePosition(1200.0),
        crossfade_frames: 64,
    });
    source.trigger_at(SamplePosition(200.0));
    source.play();

    let input = BufferVec::new(1);
    let mut output = BufferVec::new(1);
    for _ in 0..50 {
        source.process(BLOCK, &input.buffer_ref(), &mut output.buffer_mut());
        let b = output.buffer_ref();
        for i in 0..BLOCK {
            let s = b.at_f32(0, i);
            assert!(s.is_finite(), "mono looping produced {s}");
        }
    }
}
