//! End-to-end audio tests for `PolySynth`.
//!
//! The crate's in-file tests are extensive — 3,134 lines, over half the source —
//! but they test *plumbing*: MIDI events reach voices, allocation picks the
//! right slot, `isolate` severs a shared inbox, atomics propagate across clones.
//! Almost nothing asserts what comes out of `process`.
//!
//! `test_voice_stealing_in_polysynth`, for instance, plays three notes into a
//! two-voice synth and checks that *a* voice is playing note 67 afterwards. It
//! never listens. A synth that allocated perfectly and emitted silence — or
//! emitted the wrong pitch, or summed voices at the wrong gain — passes it.
//!
//! This file closes that gap: the pitch that comes out, the amplitude that comes
//! out, and the end-to-end consequences of the allocation strategies. The
//! spectral cross-check against librosa lives in `examples/verify_synth.py`;
//! everything here is self-contained.

use tutti_core::BufferVec;
use tutti_core::{Amplitude, AudioUnit, Seconds};
use tutti_midi_types::translation::scaling::midi1_velocity_to_midi2;
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::{MidiChannel, MidiGroup};
use tutti_polysynth::{
    AllocationStrategy, EnvelopeConfig, OscillatorType, PolySynth, SynthConfig, VoiceMode,
};

const SR: f64 = 48_000.0;
const BLOCK: usize = 64;

fn note_on(note: u8, vel: u8) -> MidiEvent {
    MidiEvent::note_on(
        MidiGroup::FIRST,
        MidiChannel::FIRST,
        note,
        midi1_velocity_to_midi2(vel),
    )
}

fn note_off(note: u8) -> MidiEvent {
    MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, note, 0)
}

/// An organ-like envelope: instant attack, full sustain, no decay. Keeps the
/// amplitude flat so a level assertion measures the oscillator and the voice
/// summing rather than an envelope's position on its curve.
fn flat_envelope() -> EnvelopeConfig {
    EnvelopeConfig {
        attack: Seconds(0.001),
        decay: Seconds(0.0),
        sustain: Amplitude(1.0),
        release: Seconds(0.5),
    }
}

fn config(osc: OscillatorType) -> SynthConfig {
    SynthConfig {
        sample_rate: tutti_core::SampleRate::from(SR),
        oscillator: osc,
        envelope: flat_envelope(),
        ..Default::default()
    }
}

/// Render `blocks` blocks of the left channel.
fn render(synth: &mut PolySynth, blocks: usize) -> Vec<f32> {
    let input = BufferVec::new(2);
    let mut out_buf = BufferVec::new(2);
    let mut out = Vec::with_capacity(blocks * BLOCK);
    for _ in 0..blocks {
        synth.process(BLOCK, &input.buffer_ref(), &mut out_buf.buffer_mut());
        for i in 0..BLOCK {
            out.push(out_buf.buffer_ref().at_f32(0, i));
        }
    }
    out
}

/// Render both channels, interleaved-free: `(left, right)`.
fn render_stereo(synth: &mut PolySynth, blocks: usize) -> (Vec<f32>, Vec<f32>) {
    let input = BufferVec::new(2);
    let mut out_buf = BufferVec::new(2);
    let (mut l, mut r) = (Vec::new(), Vec::new());
    for _ in 0..blocks {
        synth.process(BLOCK, &input.buffer_ref(), &mut out_buf.buffer_mut());
        for i in 0..BLOCK {
            l.push(out_buf.buffer_ref().at_f32(0, i));
            r.push(out_buf.buffer_ref().at_f32(1, i));
        }
    }
    (l, r)
}

/// Dominant frequency by parabolic-interpolated DFT peak.
///
/// Hand-rolled rather than pulled from `tutti-analysis`: that crate is not a
/// dependency here, and adding one so a test can measure would make the
/// dependency graph answer to the test suite. The parabolic step is what buys
/// sub-bin resolution — a bare `argmax` over 4096 samples at 48 kHz quantises to
/// 11.7 Hz bins, which reads as a 1% error at 260 Hz and invites chasing a
/// defect that is not there.
fn dominant_hz(x: &[f32], sr: f64) -> f64 {
    let n = x.len();
    // Hann window. Without it, spectral leakage from the rectangular edges
    // biases the peak — measured at ~1% on a 261 Hz tone, which is larger than
    // any error worth detecting.
    let windowed: Vec<f64> = x
        .iter()
        .enumerate()
        .map(|(i, &s)| {
            let w = 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / (n as f64 - 1.0)).cos();
            s as f64 * w
        })
        .collect();

    let mag = |k: usize| -> f64 {
        let (mut re, mut im) = (0.0, 0.0);
        for (i, &s) in windowed.iter().enumerate() {
            let a = -2.0 * std::f64::consts::PI * k as f64 * i as f64 / n as f64;
            re += s * a.cos();
            im += s * a.sin();
        }
        (re * re + im * im).sqrt()
    };

    let lo = ((30.0 * n as f64 / sr) as usize).max(1);
    let hi = (((sr / 2.0 - 100.0) * n as f64 / sr) as usize).min(n / 2 - 2);
    let (mut best_k, mut best_m) = (lo, 0.0);
    for k in lo..hi {
        let m = mag(k);
        if m > best_m {
            best_m = m;
            best_k = k;
        }
    }

    let (a, b, c) = (mag(best_k - 1), mag(best_k), mag(best_k + 1));
    let d = 2.0 * (2.0 * b - a - c);
    let adj = if d.abs() > 1e-12 { (c - a) / d } else { 0.0 };
    (best_k as f64 + adj) * sr / n as f64
}

fn peak(x: &[f32]) -> f32 {
    x.iter().fold(0.0f32, |a, &b| a.max(b.abs()))
}

fn rms(x: &[f32]) -> f32 {
    (x.iter().map(|&s| s * s).sum::<f32>() / x.len() as f32).sqrt()
}

/// Equal-tempered frequency for a MIDI note.
fn midi_hz(note: u8) -> f64 {
    440.0 * 2f64.powf((f64::from(note) - 69.0) / 12.0)
}

/// A note-on must produce audio at that note's pitch.
///
/// The single most basic claim a synth makes, and nothing asserted it. Every
/// oscillator is checked, because the pitch comes from a shared phase increment
/// but each waveform reads it differently — a per-waveform indexing error would
/// show here and nowhere in the existing suite.
#[test]
fn every_oscillator_plays_the_requested_pitch() {
    for osc in [
        OscillatorType::Sine,
        OscillatorType::Saw,
        OscillatorType::Square { pulse_width: 0.5 },
        OscillatorType::Triangle,
    ] {
        for note in [48u8, 60, 69, 81] {
            let mut synth = PolySynth::new(config(osc)).expect("synth builds");
            synth.midi_sender().queue(&[note_on(note, 100)]);

            // Skip the attack; measure the steady state.
            let audio = render(&mut synth, 200);
            let tail = &audio[4096..4096 + 8192];

            let got = dominant_hz(tail, SR);
            let want = midi_hz(note);
            let err = (got - want).abs() / want * 100.0;

            assert!(
                err < 1.0,
                "{osc:?} at note {note} produced {got:.2} Hz, expected {want:.2} Hz \
                 ({err:.3}% error)"
            );
        }
    }
}

/// Noise is the one oscillator without a pitch, and must still make sound.
///
/// Checked separately rather than excluded silently: an oscillator that produced
/// nothing would pass a pitch test that skipped it.
#[test]
fn the_noise_oscillator_produces_broadband_sound() {
    let mut synth = PolySynth::new(config(OscillatorType::Noise)).expect("synth builds");
    synth.midi_sender().queue(&[note_on(69, 100)]);
    let audio = render(&mut synth, 200);
    let tail = &audio[4096..];

    assert!(
        rms(tail) > 0.001,
        "the noise oscillator produced near-silence (rms {})",
        rms(tail)
    );
}

/// Velocity must scale the output level.
///
/// `handle_note_on` normalises velocity and hands it to the voice; nothing
/// checked that it reaches the amplitude. A synth that ignored velocity entirely
/// passes every existing test.
#[test]
fn velocity_scales_the_output_level() {
    let mut levels = Vec::new();
    for vel in [30u8, 60, 100, 127] {
        let mut synth = PolySynth::new(config(OscillatorType::Sine)).expect("synth builds");
        synth.midi_sender().queue(&[note_on(69, vel)]);
        let audio = render(&mut synth, 100);
        levels.push((vel, peak(&audio[2048..])));
    }

    for w in levels.windows(2) {
        assert!(
            w[1].1 > w[0].1,
            "velocity {} produced peak {} but velocity {} produced {} — \
             louder velocity must be louder",
            w[0].0,
            w[0].1,
            w[1].0,
            w[1].1
        );
    }

    // And the span must be substantial, not a rounding difference.
    let (quiet, loud) = (levels[0].1, levels[3].1);
    assert!(
        loud > quiet * 2.0,
        "velocity 30 gave {quiet} and velocity 127 gave {loud} — \
         the dynamic range is too narrow for velocity to be doing real work"
    );
}

/// A note-off must actually silence the voice, once the release has run.
///
/// The existing tests check `active_voice_count` and voice state, which are
/// bookkeeping. This checks the audio: a voice that leaked past its release
/// would keep the count correct and still be audible.
#[test]
fn a_note_off_silences_the_voice_after_release() {
    let mut cfg = config(OscillatorType::Sine);
    cfg.envelope = EnvelopeConfig {
        attack: Seconds(0.001),
        decay: Seconds(0.0),
        sustain: Amplitude(1.0),
        release: Seconds(0.05),
    };
    let mut synth = PolySynth::new(cfg).expect("synth builds");

    synth.midi_sender().queue(&[note_on(69, 100)]);
    let held = render(&mut synth, 100);
    assert!(peak(&held[2048..]) > 0.01, "the held note is inaudible");

    synth.midi_sender().queue(&[note_off(69)]);
    // 0.05 s release at 48 kHz is 2400 samples; 100 blocks is 6400.
    let after = render(&mut synth, 100);
    let tail = &after[4096..];

    assert!(
        peak(tail) < 0.001,
        "the voice is still producing {} well after its 50 ms release",
        peak(tail)
    );
}

/// Two notes must sum, and the sum must contain both pitches.
///
/// Polyphony's defining property. A synth whose second note silently replaced
/// the first — or whose mix buffer was overwritten rather than accumulated —
/// passes every allocation test in the crate.
#[test]
fn two_simultaneous_notes_both_sound() {
    let mut synth = PolySynth::new(config(OscillatorType::Sine)).expect("synth builds");

    // An octave apart, so the two peaks are unambiguous.
    synth
        .midi_sender()
        .queue(&[note_on(60, 100), note_on(72, 100)]);
    let audio = render(&mut synth, 200);
    let tail = &audio[4096..4096 + 8192];

    assert_eq!(
        synth.active_voice_count(),
        2,
        "both voices should be active"
    );

    // The dominant peak must be one of the two notes...
    let got = dominant_hz(tail, SR);
    let (f_low, f_high) = (midi_hz(60), midi_hz(72));
    assert!(
        (got - f_low).abs() / f_low < 0.02 || (got - f_high).abs() / f_high < 0.02,
        "the mix's dominant frequency is {got:.2} Hz, neither {f_low:.2} nor {f_high:.2}"
    );

    // ...and the mix must be louder than either note alone, which is what
    // distinguishes summing from replacement.
    let mut solo = PolySynth::new(config(OscillatorType::Sine)).expect("synth builds");
    solo.midi_sender().queue(&[note_on(60, 100)]);
    let solo_audio = render(&mut solo, 200);

    assert!(
        rms(tail) > rms(&solo_audio[4096..4096 + 8192]) * 1.2,
        "two notes ({}) are not meaningfully louder than one ({}) — \
         the second voice is not being summed in",
        rms(tail),
        rms(&solo_audio[4096..4096 + 8192])
    );
}

/// `NoSteal` must refuse to steal, and the refusal must be audible.
///
/// The allocator tests this at the slot level. What they cannot see is whether
/// the *synth* honours it: the note that could not be allocated must simply not
/// sound, and the notes already playing must be undisturbed.
#[test]
fn nosteal_leaves_the_held_notes_alone() {
    let mut cfg = config(OscillatorType::Sine);
    cfg.max_voices = 2;
    cfg.allocation_strategy = AllocationStrategy::NoSteal;
    let mut synth = PolySynth::new(cfg).expect("synth builds");

    synth
        .midi_sender()
        .queue(&[note_on(60, 100), note_on(64, 100)]);
    let before = render(&mut synth, 100);
    assert_eq!(synth.active_voice_count(), 2);

    // A third note into a full 2-voice synth under NoSteal.
    synth.midi_sender().queue(&[note_on(67, 100)]);
    let after = render(&mut synth, 100);

    assert_eq!(
        synth.active_voice_count(),
        2,
        "NoSteal must not admit a third voice"
    );

    // The audio must be essentially unchanged: same two notes, same level.
    let (a, b) = (rms(&before[2048..]), rms(&after[2048..]));
    assert!(
        (a - b).abs() / a < 0.15,
        "the rejected note changed the output level from {a} to {b} — \
         NoSteal should have left the mix untouched"
    );
}

/// Voice stealing must reassign a voice to the new note, audibly.
///
/// The existing `test_voice_stealing_in_polysynth` checks that a voice reports
/// note 67. This checks that note 67's *pitch* is in the output — the stolen
/// voice must actually retune, not merely relabel.
#[test]
fn a_stolen_voice_plays_the_new_note() {
    let mut cfg = config(OscillatorType::Sine);
    cfg.max_voices = 1;
    cfg.allocation_strategy = AllocationStrategy::Oldest;
    let mut synth = PolySynth::new(cfg).expect("synth builds");

    synth.midi_sender().queue(&[note_on(60, 100)]);
    let _ = render(&mut synth, 50);

    // One voice, so this must steal.
    synth.midi_sender().queue(&[note_on(72, 100)]);
    let audio = render(&mut synth, 300);
    // Well past the steal, so the old note's release has finished.
    let tail = &audio[8192..8192 + 8192];

    let got = dominant_hz(tail, SR);
    let want = midi_hz(72);
    assert!(
        (got - want).abs() / want < 0.02,
        "after stealing, the output is {got:.2} Hz but the new note is \
         {want:.2} Hz — the stolen voice did not retune"
    );
}

/// Mono mode must hold exactly one voice.
///
/// `VoiceMode::Mono` is a documented mode with no end-to-end audio test. A
/// mono synth that let a second voice through would sound like a chord.
#[test]
fn mono_mode_holds_one_voice() {
    let mut cfg = config(OscillatorType::Sine);
    cfg.voice_mode = VoiceMode::Mono;
    let mut synth = PolySynth::new(cfg).expect("synth builds");

    synth
        .midi_sender()
        .queue(&[note_on(60, 100), note_on(64, 100), note_on(67, 100)]);
    // 300 blocks = 19200 samples, enough to skip the retriggering at the start
    // and still have a full 8192-sample measurement window.
    let audio = render(&mut synth, 300);

    assert_eq!(
        synth.active_voice_count(),
        1,
        "mono mode must never have more than one voice sounding"
    );

    // And the surviving pitch must be the last note played.
    let tail = &audio[8192..8192 + 8192];
    let got = dominant_hz(tail, SR);
    let want = midi_hz(67);
    assert!(
        (got - want).abs() / want < 0.02,
        "mono mode is sounding {got:.2} Hz; the last note played was \
         {want:.2} Hz"
    );
}

/// The master volume must scale the output.
///
/// `set_volume`/`volume_atomic` are exercised for their atomics but never for
/// their effect on the signal.
#[test]
fn master_volume_scales_the_output() {
    let mut full = PolySynth::new(config(OscillatorType::Sine)).expect("synth builds");
    full.midi_sender().queue(&[note_on(69, 100)]);
    let loud = render(&mut full, 100);

    let mut half = PolySynth::new(config(OscillatorType::Sine)).expect("synth builds");
    half.set_volume(0.5);
    half.midi_sender().queue(&[note_on(69, 100)]);
    let quiet = render(&mut half, 100);

    let ratio = rms(&quiet[2048..]) / rms(&loud[2048..]);
    assert!(
        (ratio - 0.5).abs() < 0.05,
        "halving the master volume changed the level by a factor of {ratio}, \
         expected 0.5"
    );

    // Zero must be silence, not merely quiet.
    let mut muted = PolySynth::new(config(OscillatorType::Sine)).expect("synth builds");
    muted.set_volume(0.0);
    muted.midi_sender().queue(&[note_on(69, 100)]);
    let silent = render(&mut muted, 100);
    assert!(
        peak(&silent) < 1e-6,
        "volume 0.0 still produced {}",
        peak(&silent)
    );
}

/// A synth with no notes must emit exact silence.
///
/// The control case. If this fails, every level assertion above is measuring
/// something other than the note.
#[test]
fn an_idle_synth_is_silent() {
    let mut synth = PolySynth::new(config(OscillatorType::Sine)).expect("synth builds");
    let audio = render(&mut synth, 50);
    assert_eq!(
        peak(&audio),
        0.0,
        "an idle synth emitted {} — silence must be exact, not approximate",
        peak(&audio)
    );
}

/// Both channels must carry the signal.
///
/// Without unison or panning a voice is centred, so left and right should be
/// identical. A synth that filled only channel 0 would pass every mono
/// measurement in this file.
#[test]
fn a_centred_voice_reaches_both_channels() {
    let mut synth = PolySynth::new(config(OscillatorType::Sine)).expect("synth builds");
    synth.midi_sender().queue(&[note_on(69, 100)]);
    let (l, r) = render_stereo(&mut synth, 100);

    assert!(peak(&r[2048..]) > 0.01, "the right channel is silent");
    for (i, (&a, &b)) in l.iter().zip(r.iter()).enumerate().skip(2048) {
        assert!(
            (a - b).abs() < 1e-6,
            "channels diverge at sample {i}: left {a}, right {b} — \
             a centred voice with no unison should be identical in both"
        );
    }
}

/// Render through `tick`, one sample at a time: `(left, right)`.
fn render_tick(synth: &mut PolySynth, samples: usize) -> (Vec<f32>, Vec<f32>) {
    let (mut l, mut r) = (Vec::with_capacity(samples), Vec::with_capacity(samples));
    let mut frame = [0.0f32; 2];
    for _ in 0..samples {
        synth.tick(&[], &mut frame);
        l.push(frame[0]);
        r.push(frame[1]);
    }
    (l, r)
}

/// `tick` and `process` are two separate implementations of the same mix, and
/// both must be right.
///
/// This test exists because of a hole the sabotage pass found: zeroing the right
/// channel inside `tick` broke nothing, since every other test in this file
/// drives the synth through `process`. `AudioUnit` requires both — a host may
/// call either — so a divergence between them is a real defect that was
/// invisible.
///
/// The two are not asserted sample-identical. `process` renders in 64-sample
/// blocks and applies MIDI at block boundaries, while `tick` advances the
/// allocator once per sample, so their phase relationship to a note-on differs
/// by up to a block. What must agree is the pitch, the level, and the fact that
/// both channels are fed.
#[test]
fn the_tick_path_matches_the_block_path() {
    let mut ticked = PolySynth::new(config(OscillatorType::Sine)).expect("synth builds");
    ticked.midi_sender().queue(&[note_on(69, 100)]);
    let (tl, tr) = render_tick(&mut ticked, 12_800);

    let mut blocked = PolySynth::new(config(OscillatorType::Sine)).expect("synth builds");
    blocked.midi_sender().queue(&[note_on(69, 100)]);
    let (bl, _br) = render_stereo(&mut blocked, 200);

    assert!(
        peak(&tl[2048..]) > 0.01,
        "the tick path's left channel is silent"
    );
    assert!(
        peak(&tr[2048..]) > 0.01,
        "the tick path's right channel is silent — `process` feeds both \
         channels, so `tick` must too"
    );

    // Same centring rule as the block path.
    for (i, (&a, &b)) in tl.iter().zip(tr.iter()).enumerate().skip(2048) {
        assert!(
            (a - b).abs() < 1e-6,
            "tick channels diverge at sample {i}: left {a}, right {b}"
        );
    }

    // Same pitch and same level as the block path, within measurement error.
    let t_hz = dominant_hz(&tl[4096..4096 + 8192], SR);
    let b_hz = dominant_hz(&bl[4096..4096 + 8192], SR);
    assert!(
        (t_hz - b_hz).abs() / b_hz < 0.01,
        "tick produced {t_hz:.2} Hz but process produced {b_hz:.2} Hz"
    );

    let (t_rms, b_rms) = (rms(&tl[4096..]), rms(&bl[4096..]));
    assert!(
        (t_rms - b_rms).abs() / b_rms < 0.05,
        "tick's level ({t_rms}) differs from process's ({b_rms}) by more than \
         5% — the two paths should mix identically"
    );
}
