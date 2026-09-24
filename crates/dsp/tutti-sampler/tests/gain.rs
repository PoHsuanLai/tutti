//! Voice gain, end to end.
//!
//! The last `Playback` axis with no end-to-end coverage. It looks like the
//! least interesting — one multiply — and the arithmetic is indeed trivial. What
//! is not trivial is **where** the multiply happens, because there are two gain
//! fields and two entry points, and the failure mode is applying gain twice.
//!
//! - [`MemorySource`] owns a `gain`, applied by `get_sample_into`.
//! - [`Playback`] owns a `gain`, applied by `PlaybackSlot::read_clip_sample_into`.
//!
//! A voice played through a pool must take the second and **not** the first:
//! the slot deliberately calls `get_sample_raw_into` (ungained) so gain lands
//! exactly once. Its own doc says so — "gain is applied ONCE at this
//! Voice/Playback level" — which is the kind of invariant that holds until
//! someone rewrites one of the two paths. Squaring the gain is inaudible at
//! unity, subtle at 0.9, and 12 dB down at 0.25.
//!
//! The two tiers also reach it differently: the memory tier is scaled at the
//! slot, while the streaming tier applies its own gain inside `DiskSource` and
//! ignores `Playback::gain` entirely. That asymmetry is deliberate and
//! documented, but it means "same clip, same gain, same level" is a claim worth
//! checking rather than assuming.
//!
//! # DC, not a tone
//!
//! Gain is a pure scalar, so the cleanest probe is a constant: the expected
//! output is the input times the gain, exactly, with no windowing or phase to
//! reason about. A wrong exponent or a doubled application is then a plain
//! numeric mismatch.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tutti_core::BufferVec;
use tutti_core::{Amplitude, AudioUnit, Beat, Bpm, ChannelLayout, SamplePosition, Timeline, Wave};
use tutti_sampler::{
    MemorySource, MemorySourceConfig, Playback, SlotId, Voice, VoicePool, VoiceSource,
};

const SR: f64 = 48_000.0;
const BLOCK: usize = 64;
/// The constant every source below carries, on both channels.
const LEVEL: f32 = 0.5;

/// A rolling transport advanced by hand, once per block.
///
/// Gain reaches the memory tier through `PlaybackSlot`, which only reads a
/// **placed** voice — `window_position()` is `None` without a timeline and the
/// slot emits silence. Same reason `reverse_and_loop.rs` needs one.
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

/// A constant-valued source, long enough not to run dry.
fn dc_wave(len: usize) -> Arc<Wave> {
    let mut w = Wave::new(2, SR);
    for _ in 0..len {
        w.push((LEVEL, LEVEL));
    }
    Arc::new(w)
}

/// A placed voice in a pool at `play_gain`, plus the clock driving it.
fn pool_at_gain(play_gain: f32) -> (VoicePool, Arc<Clock>) {
    let clock = Clock::new();
    let source = MemorySource::with_config(
        dc_wave(SR as usize),
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
                gain: Amplitude::new(play_gain),
                ..Default::default()
            },
            channel_index: None,
        },
    );
    (pool, clock)
}

/// Render a placed pool, advancing its clock once per block. Returns channel 0.
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

/// The mean of a settled span, skipping the first block.
fn settled_mean(out: &[f32]) -> f64 {
    let body = &out[BLOCK..];
    body.iter().map(|&s| s as f64).sum::<f64>() / body.len() as f64
}

/// Gain scales the output by exactly its own value — no more, no less.
///
/// The headline claim, checked across the range where a squared gain is
/// distinguishable from a linear one. At 1.0 the two coincide, which is why
/// unity alone would prove nothing; 0.5 squared is 0.25, a 6 dB error.
#[test]
fn voice_gain_scales_the_output_by_exactly_its_value() {
    for g in [1.0f32, 0.5, 0.25, 0.75, 0.1] {
        let (mut pool, clock) = pool_at_gain(g);
        let out = render_placed(&mut pool, &clock, 8);
        let got = settled_mean(&out);
        let want = (LEVEL * g) as f64;

        assert!(
            (got - want).abs() < 1e-4,
            "gain {g}: output is {got:.6}, expected {want:.6} \
             (a squared gain would give {:.6})",
            (LEVEL * g * g) as f64
        );
    }
}

/// Gain is applied exactly **once**.
///
/// The specific hazard this file exists for. `MemorySource` carries its own
/// `gain` and `PlaybackSlot` carries `Playback::gain`; the slot reads through
/// `get_sample_raw_into` precisely so the source's copy is skipped. If a rewrite
/// ever routed the slot through `get_sample_into` instead, both would apply and
/// the output would be the gain *squared*.
///
/// Set the two gains to different values so the possible answers are all
/// distinct. With a 0.5 source level, `Playback::gain = 0.5` and
/// `MemorySource::gain = 0.25`, the output is 0.25 if only the slot's gain
/// applied (correct), 0.125 if the source's won instead, and 0.0625 if both
/// did. Only the first is right, and the number reported on failure says which
/// of the other two happened.
///
/// Verified by sabotage: routing the slot through `get_sample_into` makes this
/// fail with 0.125.
#[test]
fn a_pooled_voice_applies_playback_gain_and_not_the_sources_own() {
    let clock = Clock::new();
    let source = MemorySource::with_config(
        dc_wave(SR as usize),
        MemorySourceConfig {
            channels: ChannelLayout::STEREO,
            timeline: Some(clock.clone() as Arc<dyn Timeline>),
            // Deliberately NOT 1.0 and NOT equal to the playback gain below.
            gain: Amplitude::new(0.25),
            ..Default::default()
        },
    );

    let (mut pool, _handle) = VoicePool::new();
    pool.insert_voice(
        SlotId(1),
        Voice {
            source: VoiceSource::Memory(source),
            play: Playback {
                gain: Amplitude::new(0.5),
                ..Default::default()
            },
            channel_index: None,
        },
    );

    let out = render_placed(&mut pool, &clock, 8);
    let got = settled_mean(&out);

    let slot_only = (LEVEL * 0.5) as f64; // correct: 0.25
    let source_only = (LEVEL * 0.25) as f64; // 0.125
    let both = (LEVEL * 0.5 * 0.25) as f64; // 0.0625

    // The three outcomes are distinct by construction, so the number reported on
    // failure names the defect. Verified: routing the slot through
    // `get_sample_into` (the gained read) yields `source_only`, and a slot that
    // gained on top of it would yield `both`.
    assert!(
        (got - slot_only).abs() < 1e-4,
        "output is {got:.6}. Expected {slot_only:.6} — Playback gain applied once, \
         through the ungained `get_sample_raw_into`. \
         {source_only:.6} means the slot read through `get_sample_into` and the \
         source's own gain won instead; \
         {both:.6} means BOTH were applied, which is the squared-gain bug \
         `read_clip_sample_into` exists to prevent."
    );
}

/// Zero gain is silence, not near-silence.
///
/// The boundary a clamp or a `max(epsilon)` would quietly break. A voice muted
/// by gain must contribute nothing at all, or a "silent" track still sums into
/// the mix.
#[test]
fn zero_gain_is_exactly_silent() {
    let (mut pool, clock) = pool_at_gain(0.0);
    let out = render_placed(&mut pool, &clock, 8);

    for (i, &s) in out.iter().enumerate() {
        assert!(
            s == 0.0,
            "sample {i} is {s} at zero gain — a muted voice must be exactly silent"
        );
    }
}

/// Gain above unity amplifies rather than clamping to 1.0.
///
/// `Amplitude` is not bounded at unity, and the sampler is not the place to
/// limit — that is the mixer's job. A reader that clamped here would silently
/// cap every boosted clip.
#[test]
fn gain_above_unity_amplifies() {
    for g in [1.5f32, 2.0, 4.0] {
        let (mut pool, clock) = pool_at_gain(g);
        let out = render_placed(&mut pool, &clock, 8);
        let got = settled_mean(&out);
        let want = (LEVEL * g) as f64;

        assert!(
            (got - want).abs() < 1e-4,
            "gain {g}: output is {got:.6}, expected {want:.6} — \
             a clamp at unity would give {:.6}",
            LEVEL as f64
        );
    }
}

/// Gain applies equally to every channel.
///
/// One scalar across the frame, per the source's own doc: per-channel level is
/// the mixer strip's job. A gain that reached only channel 0 would shift the
/// stereo image toward one side while the level meter still looked plausible.
#[test]
fn gain_scales_every_channel_equally() {
    let clock = Clock::new();
    let source = MemorySource::with_config(
        dc_wave(SR as usize),
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
                gain: Amplitude::new(0.4),
                ..Default::default()
            },
            channel_index: None,
        },
    );

    let input = BufferVec::new(2);
    let mut output = BufferVec::new(2);
    let want = (LEVEL * 0.4) as f64;

    // Skip one block, then check both channels of every frame.
    pool.process(BLOCK, &input.buffer_ref(), &mut output.buffer_mut());
    clock.advance(BLOCK);
    for _ in 0..4 {
        pool.process(BLOCK, &input.buffer_ref(), &mut output.buffer_mut());
        let b = output.buffer_ref();
        for i in 0..BLOCK {
            let (l, r) = (b.at_f32(0, i) as f64, b.at_f32(1, i) as f64);
            assert!(
                (l - want).abs() < 1e-4 && (r - want).abs() < 1e-4,
                "frame {i}: L {l:.6} R {r:.6}, both expected {want:.6}"
            );
        }
        clock.advance(BLOCK);
    }
}

/// A bare `MemorySource` applies its **own** gain.
///
/// The other half of the two-field story. Ticked directly — not through a pool —
/// the source is the only thing that can apply a gain, so `get_sample_into` must
/// do it. This is what makes the pooled case above a real distinction rather
/// than a claim about a field nobody reads.
#[test]
fn a_bare_memory_source_applies_its_own_gain() {
    for g in [1.0f32, 0.5, 0.25] {
        let mut source = MemorySource::with_config(
            dc_wave(SR as usize),
            MemorySourceConfig {
                channels: ChannelLayout::STEREO,
                gain: Amplitude::new(g),
                ..Default::default()
            },
        );
        source.trigger_at(SamplePosition(0.0));
        source.play();

        let input = BufferVec::new(2);
        let mut output = BufferVec::new(2);
        source.process(BLOCK, &input.buffer_ref(), &mut output.buffer_mut());

        let b = output.buffer_ref();
        let got = b.at_f32(0, 10) as f64;
        let want = (LEVEL * g) as f64;
        assert!(
            (got - want).abs() < 1e-4,
            "bare source at gain {g}: got {got:.6}, expected {want:.6}"
        );
    }
}

/// Two voices at half gain sum to one voice at full gain.
///
/// Gain has to be linear in amplitude for a mix to behave, and a pool is a
/// summing mixer. This is the property a dB/amplitude confusion breaks:
/// `Db * 2.0` squares the amplitude (the engine's unit docs call this out
/// explicitly), so a gain path that went through dB by mistake would fail here
/// while still passing every single-voice test above.
#[test]
fn two_half_gain_voices_sum_to_one_full_gain_voice() {
    let clock = Clock::new();
    let (mut pool, _handle) = VoicePool::new();

    for id in [1u128, 2] {
        let source = MemorySource::with_config(
            dc_wave(SR as usize),
            MemorySourceConfig {
                channels: ChannelLayout::STEREO,
                timeline: Some(clock.clone() as Arc<dyn Timeline>),
                ..Default::default()
            },
        );
        pool.insert_voice(
            SlotId(id),
            Voice {
                source: VoiceSource::Memory(source),
                play: Playback {
                    gain: Amplitude::new(0.5),
                    ..Default::default()
                },
                channel_index: None,
            },
        );
    }

    let out = render_placed(&mut pool, &clock, 8);
    let got = settled_mean(&out);
    // 2 x (0.5 level x 0.5 gain) == 0.5, i.e. the source level at unity.
    let want = LEVEL as f64;

    assert!(
        (got - want).abs() < 1e-4,
        "two voices at 0.5 gain summed to {got:.6}, expected {want:.6} — \
         gain is not linear in amplitude"
    );
}
