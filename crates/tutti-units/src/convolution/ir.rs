//! Impulse-response generators for convolution reverb.
//!
//! The `*_into` variants write into a caller-provided slice so they
//! can run under `no_std` and without touching the allocator on a hot
//! reload. The `Vec`-returning wrappers remain available for
//! ergonomics (crate already requires `std` when the `convolution`
//! feature is enabled).

use tutti_core::{SampleRate, Seconds};

/// Fill `out` with a simple exponential-decay test IR.
///
/// The decay envelope is `exp(-t / decay_time)` sampled at `sample_rate`.
pub fn generate_test_ir_into(
    out: &mut [f32],
    decay_time: impl Into<Seconds>,
    sample_rate: impl Into<SampleRate>,
) {
    let decay_time = decay_time.into().get();
    let sample_rate = sample_rate.into().get();
    let decay_rate = -1.0 / (decay_time * sample_rate as f32);
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = (decay_rate * i as f32).exp();
    }
}

/// Allocating convenience wrapper around [`generate_test_ir_into`].
pub fn generate_test_ir(
    length_samples: usize,
    decay_time: impl Into<Seconds>,
    sample_rate: impl Into<SampleRate>,
) -> Vec<f32> {
    let mut out = vec![0.0f32; length_samples];
    generate_test_ir_into(&mut out, decay_time, sample_rate);
    out
}

/// Fill `out` with a synthetic room IR: an initial impulse, a handful
/// of early reflections scaled by `room_size`, and a noisy
/// exponential tail.
///
/// The caller sizes `out`; a reasonable length is `decay_time * 1.5 *
/// sample_rate`. `out` is zeroed before synthesis.
pub fn generate_room_ir_into(
    out: &mut [f32],
    room_size: f32,
    decay_time: impl Into<Seconds>,
    sample_rate: impl Into<SampleRate>,
) {
    let decay_time = decay_time.into().get();
    let sample_rate = sample_rate.into().get();
    out.fill(0.0);
    if out.is_empty() {
        return;
    }

    out[0] = 1.0;

    let reflections = [
        (0.010 * room_size, 0.7_f32),
        (0.015 * room_size, 0.5),
        (0.023 * room_size, 0.4),
        (0.031 * room_size, 0.3),
        (0.041 * room_size, 0.25),
        (0.053 * room_size, 0.2),
    ];
    for (delay_factor, amplitude) in reflections {
        let idx = (delay_factor * sample_rate as f32) as usize;
        if idx < out.len() {
            out[idx] = amplitude;
        }
    }

    let tail_start = (0.05 * sample_rate as f32) as usize;
    if tail_start >= out.len() {
        return;
    }
    let decay_rate = -1.0 / (decay_time * sample_rate as f32);

    let mut rng = 12345u32;
    for (offset, slot) in out[tail_start..].iter_mut().enumerate() {
        rng = rng.wrapping_mul(1103515245).wrapping_add(12345);
        let noise = ((rng >> 16) as f32 / 32768.0) - 1.0;
        let envelope = (decay_rate * offset as f32).exp();
        *slot += noise * envelope * 0.1;
    }
}

/// Allocating convenience wrapper around [`generate_room_ir_into`].
///
/// Allocates a buffer of `decay_time * 1.5 * sample_rate` samples.
pub fn generate_room_ir(
    room_size: f32,
    decay_time: impl Into<Seconds>,
    sample_rate: impl Into<SampleRate>,
) -> Vec<f32> {
    let decay_time = decay_time.into();
    let sample_rate = sample_rate.into();
    let length = (decay_time.get() as f64 * sample_rate.get() * 1.5) as usize;
    let mut out = vec![0.0f32; length];
    generate_room_ir_into(&mut out, room_size, decay_time, sample_rate);
    out
}
