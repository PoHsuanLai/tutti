use super::*;
use crate::Ring;
use crate::{Lti as _, System as _};

/// The pre-delay is whole frames, rounded: 1 ms at 48 kHz is 48, more than half a
/// frame rounds up, and a negative or NaN span is none.
#[test]
fn the_pre_delay_is_rounded_to_whole_frames() {
    let rate = SampleRate(48_000.0);
    assert_eq!(pre_delay_frames(Seconds(0.001), rate), Samples(48));
    assert_eq!(pre_delay_frames(Seconds(0.6 / 48_000.0), rate), Samples(1));
    assert_eq!(
        pre_delay_frames(Seconds(0.4 / 48_000.0), rate),
        Samples::ZERO
    );
    assert_eq!(pre_delay_frames(Seconds(-1.0), rate), Samples::ZERO);
    assert_eq!(pre_delay_frames(Seconds(f64::NAN), rate), Samples::ZERO);
}

/// A `Network` lives on the stack of the audio thread: the whole value stays
/// under 64 KiB (design 022 guessed "a few kilobytes"; it is about 55 KB: `MAX_STAGES` of the
/// largest stage).
#[test]
fn a_network_fits_the_audio_thread_stack() {
    let bytes = std::mem::size_of::<Network>();
    println!("size_of::<Network>() = {bytes}");
    assert!(bytes < 64 * 1024, "{bytes}");
}

/// An empty network recirculates nothing: no decay, no tail, stable.
#[test]
fn an_empty_network_is_silent_and_stable() {
    let n = Network::new(SampleRate(44_100.0));
    assert_eq!(n.rt60(Band::Mid), Seconds(0.0));
    assert_eq!(n.tail(Db(-60.0)), Ring::Finite(Seconds(0.0)));
    assert!(n.is_stable() && !n.is_lossless());
}

/// The two forms differ in their direct term: a lattice has `-g` (none at
/// `g = 0`, a plain delay), Freeverb's always `-1`; only the lattice has a
/// finite low-frequency group delay for the loop analysis.
#[test]
fn the_forms_differ_in_their_direct_term_and_group_delay() {
    let ap = |form, gain| AllPass {
        delay: 100.0,
        gain,
        modulation: Modulation::NONE,
        form,
    };
    assert!(ap(AllPassForm::Lattice, 0.5).has_direct_term());
    assert!(ap(AllPassForm::Lattice, -0.5).has_direct_term());
    assert!(!ap(AllPassForm::Lattice, 0.0).has_direct_term());
    assert!(ap(AllPassForm::Freeverb, 0.0).has_direct_term());
    assert!(ap(AllPassForm::Freeverb, 0.5).has_direct_term());
    assert_eq!(ap(AllPassForm::Lattice, 0.5).low_frequency_delay(), 300.0);
    assert_eq!(
        ap(AllPassForm::Freeverb, 0.5).low_frequency_delay(),
        f32::INFINITY
    );
}

/// The lattice form is unit magnitude at every frequency, from its
/// recurrence `y[n] = -g x[n] + x[n-D] + g y[n-D]`; with the same `g` the
/// Freeverb form (`((1+g) z^-D - 1) / (1 - g z^-D)`) is not. Both
/// impulse responses are run from their difference equations.
#[test]
fn the_lattice_is_unit_magnitude_and_the_freeverb_form_is_not() {
    let (d, g) = (8usize, 0.5f64);
    let len = 40 * d;
    let mut lattice = vec![0.0f64; len];
    let mut freeverb = vec![0.0f64; len];
    // Freeverb: buf[n] = x[n] + g buf[n-D]; y[n] = buf[n-D] - x[n].
    let mut buf = vec![0.0f64; len];
    for n in 0..len {
        let x = if n == 0 { 1.0 } else { 0.0 };
        let back = |v: &[f64]| if n >= d { v[n - d] } else { 0.0 };
        lattice[n] = -g * x + (if n == d { 1.0 } else { 0.0 }) + g * back(&lattice);
        buf[n] = x + g * back(&buf);
        freeverb[n] = back(&buf) - x;
    }
    let mag = |h: &[f64], w: f64| {
        let (re, im) = h.iter().enumerate().fold((0.0, 0.0), |(re, im), (n, &x)| {
            (
                re + x * libm::cos(w * n as f64),
                im - x * libm::sin(w * n as f64),
            )
        });
        f64::hypot(re, im)
    };
    let omegas = [0.0, 0.1, 0.4, std::f64::consts::PI / d as f64, 2.0, 3.0];
    assert!(
        omegas
            .iter()
            .all(|&w| (mag(&lattice, w) - 1.0).abs() < 1e-6)
    );
    assert!(
        omegas
            .iter()
            .any(|&w| (mag(&freeverb, w) - 1.0).abs() > 0.5)
    );
}
