//! **How many synth voices fit in a block?**
//!
//! `voices/held` is the answer, and the one to read first. At 48 kHz a
//! 64-frame block has **1.333 ms**, so a case costing 133 µs is 10% of the
//! whole budget — for one instrument, before any other track, effect or
//! plugin. Divide criterion's elem/s by 48 000 for the realtime multiple.
//!
//! # Two traps the first draft of this file fell into
//!
//! **`max_voices` must be raised with the note count, and it caps at 16.** It
//! defaults to 8, so holding 16, 32 or 64 notes against the default steals
//! back down to 8 and every case above 8 measured *identically* — the axis
//! looked flat and the flatness was the benchmark's fault, not the synth's.
//! Each case here sets `max_voices` to its own note count.
//!
//! The 16-voice ceiling these axes were originally written against is gone:
//! `finished_indices` became a `Vec` sized at construction, so `max_voices`
//! has no upper bound. The axis still stops at 16 because that is where the
//! numbers in `docs/benchmarks.md` were taken and the scaling is linear —
//! extend it if you need a figure past there, rather than extrapolating.
//!
//! **The block size is fixed at 64 frames.** `BufferArray<U2>` is
//! `MAX_BUFFER_SIZE` frames wide and `MAX_BUFFER_SIZE` is 64, so there is no
//! 512-frame case to measure — an earlier draft had one and it reported the
//! 64-frame cost under a 512-frame label. Block size is `tutti-core`'s axis,
//! where the graph really does render longer segments; here it is a constant.
//!
//! `voices/unison` matters more than it looks: unison *multiplies* the voice
//! count, so 8 notes at 7-way unison is 56 voices of work. A synth that
//! comfortably carries 16 notes may not carry 4 with a wide unison.
//!
//! Criterion is the right instrument here for the reason
//! `tutti-core/benches/engine_render.rs` sets out: steady-state,
//! fixed-working-set, allocation-free per block. The envelope is flat so the
//! measured region is a stationary sustain rather than a decaying attack —
//! the same reason `examples/render_synth_cases.rs` uses one.

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use tutti_core::dsp::{BufferArray, U2};
use tutti_core::{Amplitude, AudioUnit, Hz, Resonance, Seconds, Q};
use tutti_midi_types::translation::scaling::midi1_velocity_to_midi2;
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::{MidiChannel, MidiGroup};
use tutti_polysynth::{
    EnvelopeConfig, FilterType, OscillatorType, PolySynth, SvfMode, SynthConfig, UnisonConfig,
};

const SR: f64 = 48_000.0;
/// `BufferArray<U2>` is `MAX_BUFFER_SIZE` frames wide, and that is 64. A
/// synth block cannot be longer, so this is a constant rather than an axis.
const BLOCK: usize = 64;

/// Flat and organ-like, so the measured block is a stationary sustain.
fn flat_envelope() -> EnvelopeConfig {
    EnvelopeConfig {
        attack: Seconds(0.001),
        decay: Seconds(0.0),
        sustain: Amplitude(1.0),
        release: Seconds(0.3),
    }
}

fn config() -> SynthConfig {
    SynthConfig {
        sample_rate: tutti_core::SampleRate::from(SR),
        envelope: flat_envelope(),
        ..Default::default()
    }
}

/// A synth with `n` notes held and the attack already elapsed.
///
/// Raises `max_voices` to `n`: at the default of 8 every case above 8 steals
/// voices and measures the same work. Panics above 16, which is the synth's
/// own hard ceiling — see the module header.
fn held(cfg: SynthConfig, n: usize) -> PolySynth {
    let cfg = SynthConfig {
        max_voices: n.max(cfg.max_voices),
        ..cfg
    };
    let mut synth = PolySynth::new(cfg).expect("synth builds");
    let events: Vec<MidiEvent> = (0..n)
        .map(|i| {
            MidiEvent::note_on(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                // Spread across the keyboard: identical pitches would let a
                // voice allocator collapse them.
                36 + (i as u8 % 60),
                midi1_velocity_to_midi2(100),
            )
        })
        .collect();
    synth.midi_sender().queue(&events);

    // Run past the attack so the benchmark measures sustain.
    let input = BufferArray::<U2>::new();
    let mut buf = BufferArray::<U2>::new();
    for _ in 0..64 {
        synth.process(64, &input.buffer_ref(), &mut buf.buffer_mut());
    }
    synth
}

fn drive(synth: &mut PolySynth, frames: usize) {
    let input = BufferArray::<U2>::new();
    let mut buf = BufferArray::<U2>::new();
    synth.process(frames, &input.buffer_ref(), &mut buf.buffer_mut());
    black_box(buf.buffer_ref().at_f32(0, 0));
}

/// **The headline: cost against held-voice count.**
fn bench_voices(c: &mut Criterion) {
    let mut group = c.benchmark_group("voices/held");
    group.throughput(Throughput::Elements(BLOCK as u64));
    for n in [1usize, 2, 4, 8, 16] {
        let mut synth = held(config(), n);
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| drive(&mut synth, BLOCK))
        });
    }
    group.finish();
}

/// Oscillator cost at a fixed voice count.
fn bench_oscillator(c: &mut Criterion) {
    let mut group = c.benchmark_group("voices/oscillator");
    group.throughput(Throughput::Elements(BLOCK as u64));
    for (name, osc) in [
        ("sine", OscillatorType::Sine),
        ("saw", OscillatorType::Saw),
        ("square", OscillatorType::Square { pulse_width: 0.5 }),
        ("triangle", OscillatorType::Triangle),
    ] {
        let mut synth = held(
            SynthConfig {
                oscillator: osc,
                ..config()
            },
            16,
        );
        group.bench_function(name, |b| b.iter(|| drive(&mut synth, BLOCK)));
    }
    group.finish();
}

/// What the per-voice filter costs.
fn bench_filter(c: &mut Criterion) {
    let mut group = c.benchmark_group("voices/filter");
    group.throughput(Throughput::Elements(BLOCK as u64));
    for (name, filter) in [
        ("none", FilterType::None),
        (
            "svf-lowpass",
            FilterType::Svf {
                cutoff: Hz(2_000.0),
                q: Q(0.707),
                mode: SvfMode::Lowpass,
            },
        ),
        (
            "moog-ladder",
            FilterType::Moog {
                cutoff: Hz(2_000.0),
                resonance: Resonance(0.3),
            },
        ),
    ] {
        let mut synth = held(SynthConfig { filter, ..config() }, 16);
        group.bench_function(name, |b| b.iter(|| drive(&mut synth, BLOCK)));
    }
    group.finish();
}

/// **Unison multiplies the voice count**, so this is where a budget actually
/// goes. 8 notes at 7 voices each is 56 voices of DSP.
fn bench_unison(c: &mut Criterion) {
    let mut group = c.benchmark_group("voices/unison");
    group.throughput(Throughput::Elements(BLOCK as u64));
    for n in [1usize, 3, 7] {
        let mut synth = held(
            SynthConfig {
                // `Option`, not a count: a synth built with `None` has no
                // unison engine at all, which is the 1-voice baseline.
                unison: (n > 1).then(|| UnisonConfig {
                    voice_count: n as u8,
                    ..Default::default()
                }),
                ..config()
            },
            8,
        );
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| drive(&mut synth, BLOCK))
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_voices,
    bench_oscillator,
    bench_filter,
    bench_unison
);
criterion_main!(benches);
