//! Render a matrix of `PolySynth` cases to WAV for independent spectral judging.
//!
//! The Rust tests in `tests/synth_audio.rs` cover what can be asserted without a
//! reference: the pitch that comes out, the level, the channel layout, the
//! allocation strategies' audible consequences. What they cannot check cheaply
//! is *spectral shape* — whether a saw really has 1/k harmonics, whether a
//! square really suppresses even ones, whether a filter's rolloff is where the
//! cutoff says it is. That needs an FFT and a model, which is what the Python
//! judge brings.
//!
//! As with the sibling harnesses, the judge derives its expectations from the
//! case name and from first principles, never from this file. `saw_note69` is
//! expected to have harmonics at 1/k because that is what a sawtooth *is*, not
//! because the Rust said so.
//!
//! Run: `cargo run --release -p tutti-synth --example render_synth_cases -- <outdir>`

use std::path::Path;

use tutti_core::dsp::{BufferArray, U2};
use tutti_core::AudioUnit;
use tutti_midi_types::translation::scaling::midi1_velocity_to_midi2;
use tutti_midi_types::ump::MidiEvent;
use tutti_synth::{
    EnvelopeConfig, FilterType, OscillatorType, PolySynth, SvfMode, SynthConfig, UnisonConfig,
};

const SR: f64 = 48_000.0;
const BLOCK: usize = 64;
/// Rendered length. Long enough for a stable spectrum after the attack.
const BLOCKS: usize = 400;

/// A flat, organ-like envelope so the spectrum is stationary — a decaying one
/// would smear every harmonic measurement across the analysis window.
fn flat_envelope() -> EnvelopeConfig {
    EnvelopeConfig {
        attack: 0.001,
        decay: 0.0,
        sustain: 1.0,
        release: 0.3,
    }
}

fn render(cfg: SynthConfig, notes: &[u8]) -> (Vec<f32>, Vec<f32>) {
    let mut synth = PolySynth::new(cfg).expect("synth builds");
    let events: Vec<MidiEvent> = notes
        .iter()
        .map(|&n| MidiEvent::note_on(0, 0, n, midi1_velocity_to_midi2(100)))
        .collect();
    synth.midi_sender().queue(&events);

    let input = BufferArray::<U2>::new();
    let mut buf = BufferArray::<U2>::new();
    let (mut l, mut r) = (Vec::new(), Vec::new());
    for _ in 0..BLOCKS {
        synth.process(BLOCK, &input.buffer_ref(), &mut buf.buffer_mut());
        for i in 0..BLOCK {
            l.push(buf.buffer_ref().at_f32(0, i));
            r.push(buf.buffer_ref().at_f32(1, i));
        }
    }
    (l, r)
}

/// Minimal 32-bit-float stereo WAV writer.
///
/// Hand-rolled rather than pulling in `hound`: this crate has no WAV dependency
/// and adding one so an example can write a file would make the dependency graph
/// answer to a diagnostic. Float32 avoids any quantisation between what the
/// synth produced and what the judge reads.
fn write_wav(path: &Path, left: &[f32], right: &[f32]) {
    let n = left.len().min(right.len());
    let data_bytes = (n * 2 * 4) as u32;
    let mut out = Vec::with_capacity(44 + data_bytes as usize);

    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    out.extend_from_slice(&3u16.to_le_bytes()); // IEEE float
    out.extend_from_slice(&2u16.to_le_bytes()); // channels
    out.extend_from_slice(&(SR as u32).to_le_bytes());
    out.extend_from_slice(&((SR as u32) * 2 * 4).to_le_bytes()); // byte rate
    out.extend_from_slice(&8u16.to_le_bytes()); // block align
    out.extend_from_slice(&32u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_bytes.to_le_bytes());

    for i in 0..n {
        out.extend_from_slice(&left[i].to_le_bytes());
        out.extend_from_slice(&right[i].to_le_bytes());
    }

    std::fs::write(path, out).expect("write wav");
}

fn main() {
    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/tutti-synth".to_string());
    let out = Path::new(&out);
    std::fs::create_dir_all(out).expect("create output dir");

    let base = |osc: OscillatorType| SynthConfig {
        sample_rate: SR,
        oscillator: osc,
        envelope: flat_envelope(),
        ..Default::default()
    };

    let mut count = 0usize;

    // --- Waveform spectra ---------------------------------------------------
    //
    // Each classic waveform has a textbook harmonic series, and the judge checks
    // against that series rather than against a stored reference: a saw falls as
    // 1/k over all harmonics, a square as 1/k over odd ones only, a triangle as
    // 1/k^2 over odd ones. Getting the *shape* right is a much stronger claim
    // than "the fundamental is at the right frequency", which the Rust tests
    // already pin.
    for (name, osc) in [
        ("sine", OscillatorType::Sine),
        ("saw", OscillatorType::Saw),
        ("square", OscillatorType::Square { pulse_width: 0.5 }),
        ("triangle", OscillatorType::Triangle),
    ] {
        for note in [45u8, 57, 69] {
            let (l, r) = render(base(osc), &[note]);
            write_wav(&out.join(format!("wave_{name}_note{note}.wav")), &l, &r);
            count += 1;
        }
    }

    // --- Filters ------------------------------------------------------------
    //
    // A lowpass on a saw is the canonical subtractive-synthesis test: the source
    // is harmonically rich, so the filter's effect on each harmonic is directly
    // measurable. Cutoffs are placed so a fixed number of harmonics of note 45
    // (110 Hz) survive, which the judge re-derives.
    for cutoff in [500.0f32, 1000.0, 2000.0, 4000.0] {
        let mut cfg = base(OscillatorType::Saw);
        cfg.filter = FilterType::Svf {
            cutoff,
            q: 0.707,
            mode: SvfMode::Lowpass,
        };
        let (l, r) = render(cfg, &[45]);
        write_wav(
            &out.join(format!("filter_lp{cutoff:.0}_note45.wav")),
            &l,
            &r,
        );
        count += 1;
    }

    // Highpass, so a judge that merely detected "less energy" cannot pass both.
    for cutoff in [500.0f32, 2000.0] {
        let mut cfg = base(OscillatorType::Saw);
        cfg.filter = FilterType::Svf {
            cutoff,
            q: 0.707,
            mode: SvfMode::Highpass,
        };
        let (l, r) = render(cfg, &[45]);
        write_wav(
            &out.join(format!("filter_hp{cutoff:.0}_note45.wav")),
            &l,
            &r,
        );
        count += 1;
    }

    // --- Unison -------------------------------------------------------------
    //
    // Detuned unison voices beat against each other; the beat rate is set by the
    // detune in cents and is measurable from the amplitude envelope. Stereo
    // spread should also decorrelate the channels, which the Rust tests
    // explicitly assert does *not* happen without unison.
    for (voices, detune) in [(2u8, 10.0f32), (3, 20.0), (7, 25.0)] {
        let mut cfg = base(OscillatorType::Saw);
        cfg.unison = Some(UnisonConfig {
            voice_count: voices,
            detune_cents: detune.into(),
            stereo_spread: 0.8,
            ..Default::default()
        });
        let (l, r) = render(cfg, &[57]);
        write_wav(
            &out.join(format!("unison_v{voices}_d{detune:.0}_note57.wav")),
            &l,
            &r,
        );
        count += 1;
    }

    // --- Polyphony ----------------------------------------------------------
    //
    // A chord: every note must be present in the spectrum simultaneously. This
    // is the spectral counterpart to `two_simultaneous_notes_both_sound`, which
    // can only check that the mix got louder.
    let (l, r) = render(base(OscillatorType::Sine), &[60, 64, 67]);
    write_wav(&out.join("chord_60_64_67.wav"), &l, &r);
    count += 1;

    // --- Control ------------------------------------------------------------
    //
    // An unprocessed sine. Twice in this repo's history a harness was itself
    // wrong and the dry case is what caught it; keeping one is cheap insurance
    // against a judge that reports success on silence.
    let (l, r) = render(base(OscillatorType::Sine), &[69]);
    write_wav(&out.join("control_sine_note69.wav"), &l, &r);
    count += 1;

    println!("wrote {count} synth cases to {}", out.display());
}
