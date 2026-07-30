//! Emit tutti's pitch and loudness readings over a matrix of synthetic signals,
//! for independent judging in Python.
//!
//! The pattern is the sampler harness's, with one deliberate difference: that
//! one writes *audio* and lets Python analyse it, because what was under test
//! was the audio. Here what is under test is the **analysis**, so this writes
//! the analyser's *answers* and Python recomputes them from the same signal.
//!
//! That is why the signal generators are duplicated in the judge rather than
//! shared through a file. A generator both halves read from would make an
//! agreement between them prove only that they read the same bytes. Each side
//! synthesises from the case name — `sine_440` is 440 Hz because the name says
//! so — and a disagreement is then a real disagreement about the DSP.
//!
//! Why this crate needs it: `pitch.rs` holds the entire YIN implementation (the
//! FFT autocorrelation, the cumulative-mean normalisation, the parabolic
//! interpolation) and has **no test module at all**. Its wrapper `yin.rs` is
//! well tested, but for *behaviour* — ranges refused, silence unvoiced, a reused
//! detector matching a fresh one. Exactly one test checks an actual frequency
//! (`finds_a440`, ±1 Hz on one tone). Octave errors, YIN's signature failure
//! mode, are unasserted anywhere.
//!
//! Run: `cargo run --release -p tutti-analysis --example render_analysis_cases -- <outdir>`

use std::f64::consts::TAU;
use std::fmt::Write as _;
use std::path::Path;

use tutti_analysis::{
    loudness::{measure_loudness, LoudnessConfig},
    yin, ChannelLayout, YinConfig,
};
use tutti_core::SampleRate;
use tutti_types::Interleaved;

const SR: f64 = 48_000.0;

/// Build a tone by summing harmonics at explicit relative amplitudes.
///
/// `harmonics[k]` is the amplitude of partial `k+1`. Built from the sum
/// directly, at f64, rather than by shaping a fundamental: a generator that
/// derived partials from a table lookup could smear the very harmonic structure
/// these cases exist to vary.
fn harmonic_tone(freq: f64, harmonics: &[f64], secs: f64, amp: f64) -> Vec<f32> {
    let n = (SR * secs) as usize;
    (0..n)
        .map(|i| {
            let t = i as f64 / SR;
            let s: f64 = harmonics
                .iter()
                .enumerate()
                .map(|(k, &a)| a * (TAU * freq * (k as f64 + 1.0) * t).sin())
                .sum();
            (s * amp) as f32
        })
        .collect()
}

/// A pure sine — the one-harmonic case, named for what it is.
fn sine(freq: f64, secs: f64, amp: f64) -> Vec<f32> {
    harmonic_tone(freq, &[1.0], secs, amp)
}

/// Deterministic value-noise, seeded, so an "unvoiced" assertion is not flaky.
///
/// A plain LCG rather than `rand`: this must produce the identical sequence on
/// every machine and every run, and the judge does not need to reproduce it (it
/// only checks that noise is *not* confidently pitched).
fn noise(secs: f64, amp: f64) -> Vec<f32> {
    let n = (SR * secs) as usize;
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    (0..n)
        .map(|_| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let u = ((state >> 33) as f64 / (1u64 << 31) as f64) - 1.0;
            (u * amp) as f32
        })
        .collect()
}

/// Interleave a mono buffer to stereo (both channels identical).
fn to_stereo(mono: &[f32]) -> Vec<f32> {
    mono.iter().flat_map(|&s| [s, s]).collect()
}

fn main() {
    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/tutti-analysis".to_string());
    let out = Path::new(&out);
    std::fs::create_dir_all(out).expect("create output dir");

    let mut pitch_csv = String::from("case,expected_hz,tutti_hz,confidence,voiced,note\n");
    let mut loud_csv = String::from("case,amp,tutti_lufs,tutti_true_peak,tutti_range\n");

    // ---- Pitch cases -------------------------------------------------------
    //
    // Chosen for the failure modes YIN actually has, not for coverage of the
    // frequency axis. A detector can nail every pure sine and still be wrong on
    // everything musical.
    let cfg = YinConfig::standard(SR).expect("standard range");

    // Pure sines across the range, including the extremes where the period
    // bounds bite. 55 Hz needs a long window; 1975 Hz sits near min_period,
    // where one sample of period error is ~2% of the frequency.
    let mut cases: Vec<(String, f64, Vec<f32>)> = Vec::new();
    for &f in &[55.0, 110.0, 220.0, 440.0, 880.0, 1320.0, 1975.0] {
        cases.push((format!("sine_{f:.0}"), f, sine(f, 0.5, 0.5)));
    }

    // A sawtooth-like stack: every harmonic at 1/k. This is where octave errors
    // live — the strong second harmonic tempts the detector to report 2f, and
    // the rich low end tempts it to report f/2.
    for &f in &[110.0, 220.0, 440.0] {
        let h: Vec<f64> = (1..=8).map(|k| 1.0 / k as f64).collect();
        cases.push((format!("saw_{f:.0}"), f, harmonic_tone(f, &h, 0.5, 0.3)));
    }

    // A *missing fundamental*: partials 2,3,4 only. The perceived (and correct)
    // pitch is still f, and YIN should find it from the periodicity even though
    // there is no energy at f. A spectral-peak detector reports 2f here.
    for &f in &[220.0, 440.0] {
        cases.push((
            format!("missingfund_{f:.0}"),
            f,
            harmonic_tone(f, &[0.0, 1.0, 0.7, 0.5], 0.5, 0.4),
        ));
    }

    // Odd harmonics only (square-like). A different periodicity signature from
    // the saw, and a classic octave-error trap.
    for &f in &[220.0, 440.0] {
        cases.push((
            format!("square_{f:.0}"),
            f,
            harmonic_tone(f, &[1.0, 0.0, 0.33, 0.0, 0.2, 0.0, 0.14], 0.5, 0.4),
        ));
    }

    // Quiet material: the same tone 40 dB down. Amplitude must not move the
    // frequency — YIN normalises by energy, so a level-dependent reading would
    // mean the normalisation is wrong.
    cases.push(("quiet_440".into(), 440.0, sine(440.0, 0.5, 0.005)));

    // Controls. Neither should come back confidently pitched; `expected_hz` of
    // 0 marks "must be unvoiced" for the judge.
    cases.push(("noise".into(), 0.0, noise(0.5, 0.5)));
    cases.push(("silence".into(), 0.0, vec![0.0; (SR * 0.5) as usize]));

    for (name, expected, samples) in &cases {
        let est = yin(&cfg, samples).expect("buffer is long enough");
        let (hz, conf, voiced, note) = match est.pitch() {
            Some(p) => (p.frequency.get(), p.confidence.get(), true, p.note_name()),
            None => (0.0, 0.0, false, String::from("-")),
        };
        writeln!(
            pitch_csv,
            "{name},{expected},{hz:.6},{conf:.6},{voiced},{note}"
        )
        .unwrap();
    }

    // A frequency sweep judged per frame, which is what `yin_track` is for and
    // what no test measures the accuracy of.
    let mut sweep_csv = String::from("frame,expected_hz,tutti_hz,confidence\n");
    let frame = cfg.buffer_size().get();
    let sweep_len = frame * 8;
    // Linear sweep 200 -> 800 Hz. Phase is the integral of frequency, so it is
    // accumulated rather than computed as f*t -- the latter sweeps at twice the
    // intended rate and would make every frame's expectation wrong.
    let mut phase = 0.0f64;
    let sweep: Vec<f32> = (0..sweep_len)
        .map(|i| {
            let t = i as f64 / sweep_len as f64;
            let f = 200.0 + 600.0 * t;
            let s = (phase).sin() * 0.5;
            phase += TAU * f / SR;
            s as f32
        })
        .collect();
    let tracked = yin::yin_track(&cfg, &sweep, tutti_types::Samples(frame)).expect("track");
    for (i, est) in tracked.iter().enumerate() {
        // The expectation is the mean frequency over the part of the frame YIN
        // actually analyses, which is **not** the whole frame.
        //
        // `compute_difference` sets `window = max_period` and correlates
        // `x[0..window]` against `x[0..window+max_period]`, so with the standard
        // 50 Hz floor a 1920-sample frame is judged on its first 960 samples.
        // Taking the frame's centre instead put the expectation 14-17 Hz above
        // every reading and looked like a systematic downward bias; against the
        // real window the same numbers land within a few Hz.
        //
        // The judge re-derives this from `min_freq` on its own side; the value
        // here is for a human reading the CSV.
        let analysed = frame / 2;
        let centre = (i * frame + analysed / 2) as f64 / sweep_len as f64;
        let expected = 200.0 + 600.0 * centre;
        let (hz, conf) = match est.pitch() {
            Some(p) => (p.frequency.get(), p.confidence.get()),
            None => (0.0, 0.0),
        };
        writeln!(sweep_csv, "{i},{expected:.6},{hz:.6},{conf:.6}").unwrap();
    }

    // ---- Loudness cases ----------------------------------------------------
    //
    // EBU R128 is a published spec with absolute answers, so unlike pitch these
    // are checkable against a number rather than against another implementation.
    let lcfg = LoudnessConfig::new(SampleRate(SR), ChannelLayout::Stereo);
    for &amp in &[1.0, 0.5, 0.25, 0.1, 0.0891, 0.01] {
        let mono = sine(1000.0, 3.0, amp);
        let st = to_stereo(&mono);
        let l = measure_loudness(&lcfg, Interleaved::new(&st, ChannelLayout::Stereo))
            .expect("stereo layout meters");
        writeln!(
            loud_csv,
            "sine1k_{amp},{amp},{:.6},{:.6},{:.6}",
            l.lufs.get(),
            l.true_peak.get(),
            l.range.get()
        )
        .unwrap();
    }

    // Rate independence: the same *musical* signal (1 kHz, 3 s) at three rates
    // must read the same loudness. This is the axis with a documented bug
    // history — two true-peak sites hardcoded 48 kHz — so it gets its own cases.
    let mut rate_csv = String::from("rate,tutti_lufs,tutti_true_peak\n");
    for &rate in &[44_100.0f64, 48_000.0, 96_000.0] {
        let n = (rate * 3.0) as usize;
        let mono: Vec<f32> = (0..n)
            .map(|i| (0.5 * (TAU * 1000.0 * i as f64 / rate).sin()) as f32)
            .collect();
        let st = to_stereo(&mono);
        let c = LoudnessConfig::new(SampleRate(rate), ChannelLayout::Stereo);
        let l = measure_loudness(&c, Interleaved::new(&st, ChannelLayout::Stereo))
            .expect("stereo layout meters");
        writeln!(
            rate_csv,
            "{rate},{:.6},{:.6}",
            l.lufs.get(),
            l.true_peak.get()
        )
        .unwrap();
    }

    std::fs::write(out.join("pitch.csv"), pitch_csv).expect("write pitch.csv");
    std::fs::write(out.join("sweep.csv"), sweep_csv).expect("write sweep.csv");
    std::fs::write(out.join("loudness.csv"), loud_csv).expect("write loudness.csv");
    std::fs::write(out.join("loudness_rate.csv"), rate_csv).expect("write loudness_rate.csv");

    println!("wrote {} pitch cases to {}", cases.len(), out.display());
    println!("wrote {} sweep frames", tracked.len());
    println!("wrote loudness + rate cases");
}
