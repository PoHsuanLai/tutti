//! The sampler and the export pipeline, wired together.
//!
//! Each half is tested on its own — `tutti-sampler` verifies its voices against
//! a transport, and `roundtrip.rs` verifies that a graph reaches a file intact.
//! Neither covers the join, and the join is where a DAW actually renders: a
//! placed voice, pitched or stretched, pulled by the offline clock, quantized,
//! and written. A defect that only appears when the sampler is driven by the
//! *export's* clock rather than a live one is invisible to both suites.
//!
//! # What makes this the interesting case
//!
//! A placed voice derives its read position from the playhead. Live, that
//! playhead is the audio callback; here it is `render_to_file` advancing an
//! [`OfflineTimeline`] once per block. One `OfflineTimeline` serves as both the
//! voice's `Timeline` and the export's `RenderClock`, so this test also pins
//! that the two agree — a render whose clock advanced at a different rate from
//! the voice's would produce a file that is silent, truncated, or transposed.
//!
//! # The assertion is a measured frequency
//!
//! The source is a 440 Hz tone, and each case states the ratio its name implies:
//! an octave is 2x, a fifth is 2^(7/12). Frequency is measured off the decoded
//! file by FFT peak with parabolic interpolation — the same measurement
//! `verify_sampler.py` and `voice_pool.rs` use, for the same reason: nearly every
//! defect in this path (an inert pitch shift, a stretch acting as varispeed, a
//! wrong clock rate) leaves the output emphatically non-zero, so `!= 0.0` cannot
//! see any of them.
//!
//! Gated on `wav`, because the assertions decode the exported file through
//! `hound` — which this crate only links when that feature is on. `wav` is in
//! `default`, so these run by default.
//!
//! This file previously read `#![cfg(feature = "sampler")]`, a feature that has
//! never existed in this crate's manifest. The gate was therefore always false
//! and all five tests below were silently compiled out from the day they
//! landed; the `start_beat` type error they had accumulated in the meantime is
//! what a never-compiled file collects.

#![cfg(feature = "wav")]

use std::f32::consts::TAU;
use std::sync::Arc;

use tutti_core::{
    Beat, Bpm, Cents, OfflineTimeline, OfflineTimelineConfig, SampleRate, StretchFactor, Wave,
};
use tutti_export::{
    render_to_file, AudioFormat, BitDepth, ChannelLayout, Dither, EncodeConfig, ExportConfig,
    RenderConfig,
};
use tutti_sampler::voice::{MemorySource, Playback, SlotId, Voice, VoicePool, VoiceSource};

const SR: f64 = 48_000.0;
const BASE_HZ: f32 = 440.0;
const TEMPO: f64 = 120.0;
/// Long enough for the stretch vocoder's analysis window to settle well before
/// the measurement window opens, at every factor rendered here.
const DUR_S: f64 = 1.0;

/// A 440 Hz sine, long enough that the slowest factor cannot run it dry.
///
/// Built by evaluating the sine at each integer index — a plain wave table. The
/// voice is what resamples it; a generator that stepped its phase by the read
/// rate would do the transposition itself and pre-cancel the very effect under
/// test, reporting a clean 440 Hz for every case.
fn tone(frames: usize) -> Arc<Wave> {
    let mut w = Wave::new(1, SR);
    for i in 0..frames {
        w.push((TAU * BASE_HZ * i as f32 / SR as f32).sin());
    }
    Arc::new(w)
}

/// A net whose output is one placed sampler voice, plus the clock driving it.
///
/// The returned `OfflineTimeline` is handed to `render_to_file` as its
/// `RenderClock`, so the voice and the render share one clock by construction
/// rather than by two configs that happen to match.
fn voice_net(stretch: f32, cents: f32) -> (tutti_core::dsp::Net, Arc<OfflineTimeline>) {
    let transport = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        start_beat: Beat(0.0),
        tempo: Bpm(TEMPO),
        sample_rate: SampleRate(SR),
        loop_range: None,
    }));

    // 4x the render length: at the slowest factor the source is consumed far
    // faster than it is emitted, and a short wave would run dry mid-measurement
    // and read as an attenuation bug rather than an exhausted source.
    let wave = tone((SR * DUR_S) as usize * 4);

    let source = MemorySource::with_transport(
        wave,
        transport.clone() as Arc<dyn tutti_core::Timeline>,
        Beat::new(0.0),
        None,
    );

    let play = Playback {
        stretch: StretchFactor::new(stretch),
        pitch: Cents::new(cents),
        ..Default::default()
    };

    let (mut pool, _handle) = VoicePool::new();
    // `insert_voice` directly rather than through the handle: this is the
    // control thread, there is no audio thread to hand a command to, and going
    // through the channel would need a pump step that proves nothing here.
    pool.insert_voice(
        SlotId(1),
        Voice {
            source: VoiceSource::Memory(source),
            play,
            channel_index: None,
        },
    );

    let mut net = tutti_core::dsp::Net::new(0, 2);
    let id = net.push(Box::new(pool));
    net.pipe_output(id);
    (net, transport)
}

fn config(format: AudioFormat) -> ExportConfig {
    ExportConfig {
        render: RenderConfig {
            sample_rate: SampleRate(SR),
            duration_seconds: DUR_S,
            ..Default::default()
        },
        encode: EncodeConfig {
            format,
            bit_depth: BitDepth::Float32,
            channels: ChannelLayout::STEREO,
        },
        // Float32 never dithers anyway; stated so the intent is on the page.
        dither: Dither::Off,
        ..Default::default()
    }
}

/// Channel 0 of a float WAV.
fn read_left(path: &std::path::Path) -> Vec<f32> {
    let reader = hound::WavReader::open(path).expect("open wav");
    let channels = reader.spec().channels as usize;
    reader
        .into_samples::<f32>()
        .map(|s| s.expect("read sample"))
        .step_by(channels)
        .collect()
}

/// Dominant frequency by FFT peak with parabolic interpolation.
///
/// Parabolic rather than a bare `argmax` so sub-bin error is visible instead of
/// rounded away: at this window a bin is ~5.9 Hz, which is wider than the 2%
/// tolerance at 440 Hz, so a bare peak would pass a genuinely wrong pitch.
fn dominant_hz(x: &[f32]) -> f64 {
    let n = x.len();
    // Hann window, so the peak is not smeared by the rectangular window's
    // sidelobes into whichever neighbour happens to be larger.
    let windowed: Vec<f64> = x
        .iter()
        .enumerate()
        .map(|(i, &s)| {
            let w = 0.5 - 0.5 * (TAU as f64 * i as f64 / n as f64).cos();
            s as f64 * w
        })
        .collect();

    // Direct DFT over the band of interest. O(n*k), but k is small and this
    // avoids pulling an FFT dependency into a test whose whole purpose is to be
    // an independent second opinion on the engine's own DSP.
    let bins = n / 2;
    let mag = |k: usize| -> f64 {
        let (mut re, mut im) = (0.0f64, 0.0f64);
        let w = -2.0 * std::f64::consts::PI * k as f64 / n as f64;
        for (i, &s) in windowed.iter().enumerate() {
            let (sin, cos) = (w * i as f64).sin_cos();
            re += s * cos;
            im += s * sin;
        }
        (re * re + im * im).sqrt()
    };

    // Search 100..2000 Hz — comfortably around every ratio under test (110 Hz
    // at two octaves down, 1760 Hz at two up).
    let lo = (100.0 * n as f64 / SR).floor() as usize;
    let hi = ((2000.0 * n as f64 / SR).ceil() as usize).min(bins - 2);
    let mags: Vec<f64> = (lo..=hi).map(mag).collect();

    let (rel_peak, _) = mags
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
        .expect("non-empty spectrum");
    let k = lo + rel_peak;

    if rel_peak == 0 || rel_peak + 1 >= mags.len() {
        return k as f64 * SR / n as f64;
    }
    let (a, b, c) = (mags[rel_peak - 1], mags[rel_peak], mags[rel_peak + 1]);
    let denom = a - 2.0 * b + c;
    let delta = if denom != 0.0 {
        0.5 * (a - c) / denom
    } else {
        0.0
    };
    (k as f64 + delta) * SR / n as f64
}

fn rms(x: &[f32]) -> f64 {
    (x.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
}

/// Measure the settled portion — past the vocoder's warm-up, short of the end.
fn window(x: &[f32]) -> &[f32] {
    let start = (SR * 0.4) as usize;
    let len = 8192;
    assert!(
        x.len() >= start + len,
        "render too short to measure: {} frames",
        x.len()
    );
    &x[start..start + len]
}

/// Render one case to a WAV and return the measured (frequency, RMS) of the left
/// channel's settled window.
fn render_and_measure(dir: &std::path::Path, name: &str, stretch: f32, cents: f32) -> (f64, f64) {
    let (net, clock) = voice_net(stretch, cents);
    let path = dir.join(format!("{name}.wav"));
    render_to_file(net, &config(AudioFormat::Wav), clock.as_ref(), &path).unwrap();
    let left = read_left(&path);
    let w = window(&left);
    (dominant_hz(w), rms(w))
}

const SEMITONE: f64 = 1.059_463_094_359_295_3; // 2^(1/12)

/// The control: an unprocessed voice exports at its own pitch and level.
///
/// Every other case is measured against this one. Both times the sampler's own
/// Python harness was wrong, its `dry` control is what said so — the same logic
/// applies here, where a broken clock would transpose *every* case uniformly and
/// look like a consistent, plausible result.
#[test]
fn a_dry_voice_exports_at_its_source_pitch() {
    let d = tempfile::tempdir().unwrap();
    let (hz, level) = render_and_measure(d.path(), "dry", 1.0, 0.0);

    assert!(
        (hz - BASE_HZ as f64).abs() / BASE_HZ as f64 <= 0.02,
        "an unprocessed voice must export at {BASE_HZ} Hz, measured {hz:.1} Hz. \
         If this fails, the export clock and the voice's timeline disagree and \
         every other case in this file is meaningless."
    );
    assert!(
        level > 0.1,
        "dry render is near-silent (rms {level:.5}) — the voice never played"
    );
}

/// Pitch shift survives the export pipeline, at the ratio its interval implies.
///
/// The expectations come from music theory, not from the sampler: an octave is
/// 2x, a fifth is 2^(7/12). A test that asked the engine what a fifth was would
/// agree with it however wrong it became.
#[test]
fn a_pitched_voice_exports_at_the_shifted_frequency() {
    let d = tempfile::tempdir().unwrap();

    for (name, cents, ratio) in [
        ("up_octave", 1200.0f32, 2.0f64),
        ("down_octave", -1200.0, 0.5),
        ("up_fifth", 700.0, SEMITONE.powi(7)),
        ("down_fourth", -500.0, SEMITONE.powi(-5)),
    ] {
        let (hz, level) = render_and_measure(d.path(), name, 1.0, cents);
        let want = BASE_HZ as f64 * ratio;
        let err = (hz - want).abs() / want;

        assert!(
            err <= 0.02,
            "{name}: expected ~{want:.1} Hz, exported file measures {hz:.1} Hz \
             ({:.2}% off)",
            err * 100.0
        );
        assert!(
            level > 0.05,
            "{name}: exported near-silence (rms {level:.5})"
        );
    }
}

/// Time-stretch must change duration, **not** pitch — through the export too.
///
/// This is the assertion that caught a real defect in the sampler: `VoicePool`
/// never applied `stretch::Unit::input_rate`, so the factor behaved as plain
/// varispeed and 2.0x turned 440 Hz into 880 Hz. It is pinned in-tree at
/// `voice_pool.rs`, but only against a live transport. Here it is pinned against
/// the *export* clock, which is a different driver of the same code.
#[test]
fn a_stretched_voice_exports_without_transposing() {
    let d = tempfile::tempdir().unwrap();

    for (name, factor) in [
        ("stretch_half", 0.5f32),
        ("stretch_1p5", 1.5),
        ("stretch_double", 2.0),
    ] {
        let (hz, level) = render_and_measure(d.path(), name, factor, 0.0);
        let err = (hz - BASE_HZ as f64).abs() / BASE_HZ as f64;

        assert!(
            err <= 0.02,
            "{name}: stretch must not move pitch — expected ~{BASE_HZ} Hz, \
             exported file measures {hz:.1} Hz ({:.2}% off). A factor acting as \
             varispeed is the `input_rate` bug returning.",
            err * 100.0
        );
        assert!(
            level > 0.05,
            "{name}: exported near-silence (rms {level:.5})"
        );
    }
}

/// Stretch and pitch compose: pitch follows the cents alone, whatever the stretch.
#[test]
fn stretch_and_pitch_compose_independently_through_the_export() {
    let d = tempfile::tempdir().unwrap();

    for (name, stretch, cents, ratio) in [
        ("stretch_double_up_octave", 2.0f32, 1200.0f32, 2.0f64),
        ("stretch_1p5_up_fifth", 1.5, 700.0, SEMITONE.powi(7)),
    ] {
        let (hz, level) = render_and_measure(d.path(), name, stretch, cents);
        let want = BASE_HZ as f64 * ratio;
        let err = (hz - want).abs() / want;

        assert!(
            err <= 0.02,
            "{name}: pitch must follow the cents alone — expected ~{want:.1} Hz, \
             measured {hz:.1} Hz ({:.2}% off)",
            err * 100.0
        );
        assert!(
            level > 0.05,
            "{name}: exported near-silence (rms {level:.5})"
        );
    }
}

/// A sampler render survives quantization to 16-bit integer, not just float.
///
/// The cases above export Float32 to isolate the sampler. This one closes the
/// loop through the path a real export takes — quantize, dither, write — and
/// checks the pitch is still there on the other side.
#[test]
fn a_sampler_render_survives_integer_quantization() {
    let d = tempfile::tempdir().unwrap();
    let (net, clock) = voice_net(1.0, 1200.0);
    let path = d.path().join("int16.wav");

    let mut cfg = config(AudioFormat::Wav);
    cfg.encode.bit_depth = BitDepth::Int16;
    cfg.dither = Dither::Triangular; // the default a real export uses

    render_to_file(net, &cfg, clock.as_ref(), &path).unwrap();

    let reader = hound::WavReader::open(&path).unwrap();
    let channels = reader.spec().channels as usize;
    let left: Vec<f32> = reader
        .into_samples::<i32>()
        .map(|s| s.unwrap() as f32 / 32_767.0)
        .step_by(channels)
        .collect();

    let w = window(&left);
    let hz = dominant_hz(w);
    let want = BASE_HZ as f64 * 2.0;
    assert!(
        (hz - want).abs() / want <= 0.02,
        "an octave-up voice must still measure ~{want:.1} Hz after 16-bit \
         quantization, got {hz:.1} Hz"
    );
    assert!(rms(w) > 0.05, "quantized render is near-silent");
}
