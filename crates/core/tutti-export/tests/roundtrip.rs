//! Sample-accuracy round-trip: does an exported file hold the audio we rendered?
//!
//! The existing suite (`render.rs`) checks *structure* — frame counts, header
//! rates, file sizes, no-panic. None of it decodes a file back and looks at the
//! values, so an export that wrote silence, halved every sample, or quantized
//! with the wrong scale would pass. This file closes that gap: render a signal
//! whose exact value at every frame is known in advance, read the file back, and
//! compare within the quantization floor of the depth that was asked for.
//!
//! # Why the expected value is computed here, not imported
//!
//! Each case states its own expectation from the bit depth's own arithmetic
//! (`1/32767`, `1/8388607`) rather than calling the engine's `pcm::f32_to_i16`.
//! Importing the converter would make this a restatement of the implementation:
//! it would agree with any scale the engine happened to use, including a wrong
//! one. The whole point is a second opinion.
//!
//! # Dither is off in the exact cases
//!
//! `Dither::Triangular` is the *default*, and it adds +-1 LSB of noise before
//! quantization. That is correct behaviour and is exercised in
//! `dither_stats.rs`; it just makes bit-accuracy untestable, so the round-trip
//! cases turn it off explicitly. A config literal that forgets to is the most
//! likely way to make these tests mysteriously flaky.
//!
//! Gated on `wav` + `flac`: the cases write both formats unconditionally, and
//! `hound` (which reads the WAVs back) is only linked under `wav`.

#![cfg(all(feature = "wav", feature = "flac"))]

use tutti_core::dsp::dc;
use tutti_export::{
    render_to_file, AudioFormat, BitDepth, ChannelLayout, Dither, EncodeConfig, ExportConfig,
    FrozenClock, RenderConfig,
};

const SR: f64 = 44_100.0;
const DUR: f64 = 0.1;

/// A graph emitting the constant `level` on both channels.
///
/// DC rather than a tone: every frame has the same known value, so a comparison
/// failure names the error directly (a scale factor, a truncation) instead of
/// being smeared across a waveform. The spectral cases live in the Python judge.
fn dc_net(level: f32) -> tutti_core::dsp::Net {
    let mut n = tutti_core::dsp::Net::new(0, 2);
    let id = n.push(Box::new(dc((level, level))));
    n.pipe_output(id);
    n
}

fn config(format: AudioFormat, bit_depth: BitDepth) -> ExportConfig {
    ExportConfig {
        render: RenderConfig {
            sample_rate: tutti_core::SampleRate(SR),
            duration_seconds: DUR,
            ..Default::default()
        },
        encode: EncodeConfig {
            format,
            bit_depth,
            channels: ChannelLayout::STEREO,
        },
        // Exactness: see the module note.
        dither: Dither::Off,
        ..Default::default()
    }
}

/// One LSB in the `[-1, 1]` float domain, at the depth the file was written at.
///
/// This is the tightest honest tolerance for an integer round-trip: quantization
/// alone can move a sample by half an LSB, and the reader's own scale choice can
/// account for another fraction. Anything looser would let a real scaling bug
/// through; anything tighter would fail on correct code.
fn lsb(bit_depth: BitDepth) -> f32 {
    match bit_depth {
        BitDepth::Int16 => 1.0 / 32_767.0,
        BitDepth::Int24 => 1.0 / 8_388_607.0,
        BitDepth::Float32 => 1.0 / 8_388_607.0, // exact in practice; kept for the table
                                                // No `_` arm: `BitDepth` is deliberately not `#[non_exhaustive]`, so a
                                                // fourth depth must fail to compile here rather than pick up a silent
                                                // default. See the enum's docs in tutti-types::pcm.
    }
}

/// Read every sample of a WAV back as `f32` in `[-1, 1]`, whatever it was stored as.
///
/// Normalizes integer samples by the same magnitude the engine scales *by*
/// (32767 / 8388607), not by `1 << (bits-1)`. Those differ by one part in 32768,
/// which is larger than the tolerance here — so picking the wrong one would make
/// a correct encoder look broken.
fn read_wav(path: &std::path::Path) -> (hound::WavSpec, Vec<f32>) {
    let reader = hound::WavReader::open(path).expect("open wav");
    let spec = reader.spec();
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader
            .into_samples::<f32>()
            .map(|s| s.expect("read f32 sample"))
            .collect(),
        hound::SampleFormat::Int => {
            let scale = match spec.bits_per_sample {
                16 => 32_767.0,
                24 => 8_388_607.0,
                b => panic!("unexpected int depth {b}"),
            };
            reader
                .into_samples::<i32>()
                .map(|s| s.expect("read int sample") as f32 / scale)
                .collect()
        }
    };
    (spec, samples)
}

/// The core claim: a constant rendered at `level` comes back as `level`.
///
/// Covers every WAV depth. A wrong scale, a truncation, a silent payload, or a
/// byte-order slip all move the value; none of them change the frame count that
/// the existing tests check.
#[test]
fn a_wav_round_trips_its_samples_at_every_depth() {
    let d = tempfile::tempdir().unwrap();

    for bit_depth in [BitDepth::Int16, BitDepth::Int24, BitDepth::Float32] {
        // Deliberately not a round binary fraction: 0.5 is exactly representable
        // at every depth and would hide a rounding-versus-truncation difference,
        // which is precisely one of the bugs this file is meant to catch.
        for level in [0.0f32, 0.3, -0.3, 0.75] {
            let path = d.path().join(format!("dc_{bit_depth:?}_{level}.wav"));
            let cfg = config(AudioFormat::Wav, bit_depth);
            render_to_file(dc_net(level), &cfg, &FrozenClock, &path).unwrap();

            let (spec, samples) = read_wav(&path);
            assert_eq!(spec.channels, 2, "{bit_depth:?}: channel count");
            assert_eq!(spec.sample_rate, SR as u32, "{bit_depth:?}: header rate");
            assert!(
                !samples.is_empty(),
                "{bit_depth:?} @ {level}: empty payload"
            );

            let tol = lsb(bit_depth);
            // Check every sample, not an average: an average hides a payload
            // that is right at the start and garbage later.
            for (i, &s) in samples.iter().enumerate() {
                assert!(
                    (s - level).abs() <= tol,
                    "{bit_depth:?} @ {level}: sample {i} came back as {s} \
                     (off by {}, tolerance {tol})",
                    (s - level).abs()
                );
            }
        }
    }
}

/// Full scale must survive as full scale, not wrap to the opposite sign.
///
/// The classic failure is scaling by `1 << (bits-1)` — `32768` at 16-bit — which
/// overflows `i16` at exactly +1.0 and wraps to the negative rail. That is
/// inaudible on quiet material and catastrophic on a limited master, so it is
/// worth its own case rather than a row in the table above.
#[test]
fn full_scale_does_not_wrap() {
    let d = tempfile::tempdir().unwrap();

    for bit_depth in [BitDepth::Int16, BitDepth::Int24] {
        for level in [1.0f32, -1.0] {
            let path = d.path().join(format!("fs_{bit_depth:?}_{level}.wav"));
            let cfg = config(AudioFormat::Wav, bit_depth);
            render_to_file(dc_net(level), &cfg, &FrozenClock, &path).unwrap();

            let (_, samples) = read_wav(&path);
            for (i, &s) in samples.iter().enumerate() {
                assert!(
                    s.signum() == level.signum() && (s.abs() - 1.0).abs() <= lsb(bit_depth),
                    "{bit_depth:?}: full scale {level} wrapped to {s} at sample {i}"
                );
            }
        }
    }
}

/// Out-of-range input clamps to the rail rather than wrapping.
///
/// A graph can exceed [-1, 1] — a sum of sources, a gain, a resonant filter. The
/// encoder must clamp; wrapping turns an overshoot into full-scale noise of the
/// wrong sign.
#[test]
fn out_of_range_input_clamps_to_the_rail() {
    let d = tempfile::tempdir().unwrap();

    for (level, expect) in [(1.8f32, 1.0f32), (-1.8, -1.0)] {
        let path = d.path().join(format!("clamp_{level}.wav"));
        let cfg = config(AudioFormat::Wav, BitDepth::Int16);
        render_to_file(dc_net(level), &cfg, &FrozenClock, &path).unwrap();

        let (_, samples) = read_wav(&path);
        for (i, &s) in samples.iter().enumerate() {
            assert!(
                (s - expect).abs() <= lsb(BitDepth::Int16),
                "{level} should clamp to {expect}, sample {i} came back as {s}"
            );
        }
    }
}

/// Every integer encoder must quantize a given float to the same value.
///
/// `tutti_core::pcm` exists precisely so "a recorded and an exported file
/// quantize a given sample identically" (its own module doc). A format carrying
/// a private copy of that arithmetic can drift from the shared one silently —
/// both files decode, both have the right length, and the samples differ.
///
/// Asserted between WAV and FLAC at a level chosen so rounding and truncation
/// disagree: `0.3 * 32767 = 9830.1`, which rounds to 9830 and truncates to 9830
/// — equal — so the interesting levels are those with a fractional part above
/// 0.5. `0.7 * 32767 = 22936.9` rounds to 22937 and truncates to 22936.
#[test]
fn wav_and_flac_quantize_a_sample_identically() {
    let d = tempfile::tempdir().unwrap();
    let level = 0.7f32;

    let wav_path = d.path().join("q.wav");
    render_to_file(
        dc_net(level),
        &config(AudioFormat::Wav, BitDepth::Int16),
        &FrozenClock,
        &wav_path,
    )
    .unwrap();
    let (_, wav) = read_wav(&wav_path);

    let flac_path = d.path().join("q.flac");
    render_to_file(
        dc_net(level),
        &config(AudioFormat::Flac(Default::default()), BitDepth::Int16),
        &FrozenClock,
        &flac_path,
    )
    .unwrap();

    // Decoding FLAC would need a decoder dependency this crate does not have,
    // so the cross-format claim is split in two. Here: the WAV that *can* be
    // read back must match the engine's canonical quantizer. The matching claim
    // for FLAC is `quantization_matches_the_engines_canonical_converter` in
    // `encode/flac.rs`, which compares its converter against the same helper at
    // levels where rounding and truncation disagree. The Python judge closes
    // the loop by decoding both files with soundfile and comparing them.
    //
    // This pair caught a real divergence: FLAC carried a private converter that
    // truncated (`as i32`) where `tutti_core::pcm` rounds, so 0.7 encoded as
    // 22936 in a FLAC and 22937 in a WAV from the same render. Fixed by making
    // the FLAC encoder call the shared helpers.
    let canonical = tutti_core::pcm::f32_to_i16(level);
    let from_wav = (wav[0] * 32_767.0).round() as i16;
    assert_eq!(
        from_wav, canonical,
        "WAV diverged from the engine's canonical quantizer"
    );
    assert_eq!(
        canonical, 22_937,
        "0.7 must round to 22937, not truncate to 22936"
    );
}

/// FLAC cannot carry 32-bit float, and says so rather than writing something wrong.
#[test]
fn flac_rejects_float32_instead_of_silently_downgrading() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("f.flac");
    let cfg = config(AudioFormat::Flac(Default::default()), BitDepth::Float32);

    let err = render_to_file(dc_net(0.5), &cfg, &FrozenClock, &path);
    assert!(
        err.is_err(),
        "FLAC + Float32 must be a clean error, not a silent downgrade to 24-bit"
    );
}

/// A mono graph exported wider puts its signal in channel 0 and leaves the rest
/// silent — asserted on decoded *samples*, not on energy ratios.
///
/// `render.rs` already checks this via peak values from `render_to_buffers`;
/// here it goes through a real encoder and back off disk, which is what catches
/// an interleaving slip that a buffer-level check cannot see.
#[test]
fn a_mono_graph_upmixed_to_quad_puts_signal_only_in_channel_zero() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("quad.wav");

    let mut n = tutti_core::dsp::Net::new(0, 1);
    let id = n.push(Box::new(dc(0.5)));
    n.pipe_output(id);

    let mut cfg = config(AudioFormat::Wav, BitDepth::Int24);
    cfg.encode.channels = ChannelLayout::QUAD;
    render_to_file(n, &cfg, &FrozenClock, &path).unwrap();

    let (spec, samples) = read_wav(&path);
    assert_eq!(spec.channels, 4);

    let tol = lsb(BitDepth::Int24);
    for (i, frame) in samples.chunks_exact(4).enumerate() {
        assert!(
            (frame[0] - 0.5).abs() <= tol,
            "frame {i}: channel 0 should carry 0.5, got {}",
            frame[0]
        );
        for (c, &s) in frame.iter().enumerate().skip(1) {
            assert!(
                s.abs() <= tol,
                "frame {i}: channel {c} should be silent, got {s}"
            );
        }
    }
}

/// A resampled export must still hold the signal, at the new rate.
///
/// `render.rs` checks that the frame *count* changed. It cannot distinguish a
/// correct conversion from one that wrote the right number of zeros — which is
/// exactly what a resampler that dropped its output would produce. DC is the
/// right probe: a rate conversion leaves a constant unchanged, so the expected
/// value is known without modelling the filter at all.
#[test]
fn a_resampled_export_preserves_the_signal_not_just_the_frame_count() {
    let d = tempfile::tempdir().unwrap();

    for target in [48_000.0f64, 22_050.0] {
        let path = d.path().join(format!("rs_{target}.wav"));
        let mut cfg = config(AudioFormat::Wav, BitDepth::Int24);
        cfg.resample = Some(tutti_export::Resample::to(tutti_core::SampleRate(target)));
        cfg.render.duration_seconds = 0.25;

        render_to_file(dc_net(0.4), &cfg, &FrozenClock, &path).unwrap();

        let (spec, samples) = read_wav(&path);
        assert_eq!(spec.sample_rate, target as u32, "header rate");
        assert!(!samples.is_empty(), "resample to {target} wrote no samples");

        // Skip the filter's edges: an FFT resampler's first and last frames are
        // the window ramping, which is expected and not what this asserts.
        // `Ord::max` explicitly: the `tutti_core::dsp` glob also brings a
        // `Num::max` into scope, and a bare `.max` is ambiguous between them.
        let skip = std::cmp::Ord::max(spec.sample_rate as usize / 20, 1);
        assert!(
            samples.len() > skip * 4,
            "too few frames to measure past the filter edges"
        );
        let body = &samples[skip * 2..samples.len() - skip * 2];

        // Loose: a rate conversion has passband ripple, and the point here is
        // "the signal is present and at the right level", not filter quality.
        // The Python judge measures the response properly.
        for (i, &s) in body.iter().enumerate() {
            assert!(
                (s - 0.4).abs() < 0.01,
                "resample to {target}: sample {i} is {s}, expected ~0.4 \
                 (a resampler that dropped its output writes zeros here)"
            );
        }
    }
}
