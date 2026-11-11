use super::*;
use tutti_nodes::{LadderFilterNode, LadderType, SvfFilterNode};
use tutti_types::{Amplitude, Seconds};

const SR: f64 = 48_000.0;

fn sr() -> SampleRate {
    SampleRate::new(SR)
}

/// Instant attack, full sustain: the envelope is 1.0 from the first
/// sample, so the output is the oscillator/filter chain alone.
fn flat() -> Adsr {
    Adsr::new(Seconds(0.0), Seconds(0.0), Amplitude(1.0), Seconds(0.1))
}

fn bank(osc: OscillatorType, filter: FilterType, env: Adsr) -> VoiceBank {
    VoiceBank::new(osc, &filter, &env, sr(), 16, 1)
}

/// Start `lane` sounding at `freq` with unity gains, as a note-on would.
fn start(bank: &mut VoiceBank, lane: usize, freq: Hz, cutoff: Hz, res: Resonance) {
    bank.set_pitch(lane, freq);
    bank.set_filter(lane, 1, cutoff, res);
    bank.set_gains(lane, Amplitude::UNITY, Amplitude::UNITY);
    bank.snap_tone(lane);
    bank.snap_gain(lane);
    bank.start(lane, Phase::START);
    bank.gate_on(lane);
}

/// Render `frames` frames of the given lanes, one control step of `step`
/// frames at a time, returning the left channel.
fn render(bank: &mut VoiceBank, lanes: &[usize], frames: usize, step: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(frames);
    while out.len() < frames {
        let n = step.min(frames - out.len());
        for &lane in lanes {
            bank.mark_live(lane);
        }
        out.extend_from_slice(bank.render(n).0);
    }
    out
}

/// The envelope level after each of `frames` single-frame steps.
fn envelope_trace(bank: &mut VoiceBank, lane: usize, frames: usize) -> Vec<f32> {
    (0..frames)
        .map(|_| {
            bank.mark_live(lane);
            bank.render(1);
            bank.envelope(lane).get()
        })
        .collect()
}

/// Attack, decay and release take their authored times, and sustain holds.
///
/// At 48 kHz: a 10 ms attack is 480 samples to 1.0, a 20 ms decay 960 more
/// to the 0.5 sustain, and a 30 ms release 1440 samples to silence.
#[test]
fn adsr_stages_take_their_authored_times() {
    let env = Adsr::new(Seconds(0.01), Seconds(0.02), Amplitude(0.5), Seconds(0.03));
    let mut b = bank(OscillatorType::Sine, FilterType::None, env);
    start(&mut b, 0, Hz(440.0), Hz(20_000.0), Resonance::NONE);

    let trace = envelope_trace(&mut b, 0, 3000);
    let peak = trace.iter().position(|&e| e >= 1.0).unwrap();
    assert!(
        (478..=481).contains(&peak),
        "attack peaked at sample {peak}, want ~479"
    );
    // Linear rise: half way at half time.
    assert!(
        (trace[239] - 0.5).abs() < 0.01,
        "attack midpoint {}",
        trace[239]
    );

    let sustained = peak + trace[peak..].iter().position(|&e| e <= 0.5).unwrap();
    assert!(
        (peak + 958..=peak + 962).contains(&sustained),
        "decay reached sustain at {sustained}, want ~{}",
        peak + 960
    );
    assert!(
        trace[2500..].iter().all(|&e| e == 0.5),
        "sustain did not hold at 0.5"
    );

    b.gate_off(0);
    let release = envelope_trace(&mut b, 0, 2000);
    let silent = release.iter().position(|&e| e == 0.0).unwrap();
    assert!(
        (1438..=1441).contains(&silent),
        "release reached zero at {silent}, want ~1439"
    );
    assert!(b.is_idle(0), "a finished release must leave the lane idle");
    assert!(
        (release[719] - 0.25).abs() < 0.01,
        "release midpoint {}",
        release[719]
    );
}

/// A release begun mid-attack falls from where the attack had got to, and
/// still takes the authored release time; a retrigger mid-release attacks
/// from where the release had got to, never dropping to zero first.
///
/// Both are what make a fast re-strike click-free. (`adsr_live` did the
/// first with a multiplier on the still-rising attack curve and restarted
/// the second from zero.)
#[test]
fn a_release_starts_from_the_current_level() {
    let env = Adsr::new(Seconds(0.01), Seconds(0.02), Amplitude(0.5), Seconds(0.03));
    let mut b = bank(OscillatorType::Sine, FilterType::None, env);
    start(&mut b, 0, Hz(440.0), Hz(20_000.0), Resonance::NONE);

    // 150 samples into a 480-sample attack.
    let _ = envelope_trace(&mut b, 0, 150);
    let at_release = b.envelope(0).get();
    assert!((at_release - 150.0 / 480.0).abs() < 0.01);
    b.gate_off(0);
    let release = envelope_trace(&mut b, 0, 1500);
    assert!(release[0] < at_release && release[0] > at_release - 0.01);
    assert!(
        release.windows(2).all(|w| w[1] <= w[0]),
        "release must fall monotonically"
    );
    let silent = release.iter().position(|&e| e == 0.0).unwrap();
    assert!(
        (1438..=1441).contains(&silent),
        "release took {silent} samples, want ~1439"
    );

    // Retrigger half way through a release from sustain.
    b.gate_on(0);
    let _ = envelope_trace(&mut b, 0, 2000);
    b.gate_off(0);
    let _ = envelope_trace(&mut b, 0, 720);
    let before = b.envelope(0).get();
    b.gate_on(0);
    // From 0.25 the attack needs 360 samples to peak; stop short of it.
    let attack = envelope_trace(&mut b, 0, 300);
    assert!(
        attack[0] >= before,
        "retrigger dropped the level from {before} to {}",
        attack[0]
    );
    assert!(
        attack.windows(2).all(|w| w[1] >= w[0]),
        "retriggered attack must rise"
    );
}

/// A lane renders the same whichever lane it is in, and lanes sum without
/// touching each other.
///
/// Run on the nonlinear ladder with a saw, the chain with the most per-lane
/// state, so a coefficient, state or gain written to the wrong lane shows.
#[test]
fn a_voice_renders_the_same_in_any_lane_and_lanes_sum() {
    let filter = FilterType::Moog {
        cutoff: Hz(1_500.0),
        resonance: Resonance(0.6),
    };
    let solo = |lane: usize, freq: Hz| {
        let mut b = bank(OscillatorType::Saw, filter, flat());
        start(&mut b, lane, freq, Hz(1_500.0), Resonance(0.6));
        render(&mut b, &[lane], 2048, CONTROL_BLOCK)
    };

    let in_lane_0 = solo(0, Hz(220.0));
    let in_lane_5 = solo(5, Hz(220.0));
    assert!(
        in_lane_0.iter().any(|s| s.abs() > 0.1),
        "the solo lane is silent"
    );
    assert_eq!(
        in_lane_0, in_lane_5,
        "lane 5 renders differently from lane 0"
    );

    // Two lanes of one group at once, and a lane in the next group.
    let other = solo(3, Hz(331.0));
    let far = solo(9, Hz(97.0));
    let mut b = bank(OscillatorType::Saw, filter, flat());
    start(&mut b, 0, Hz(220.0), Hz(1_500.0), Resonance(0.6));
    start(&mut b, 3, Hz(331.0), Hz(1_500.0), Resonance(0.6));
    start(&mut b, 9, Hz(97.0), Hz(1_500.0), Resonance(0.6));
    let together = render(&mut b, &[0, 3, 9], 2048, CONTROL_BLOCK);
    for (i, &t) in together.iter().enumerate() {
        let sum = in_lane_0[i] + other[i] + far[i];
        assert!(
            (t - sum).abs() < 1e-5,
            "frame {i}: lanes together {t} but separately {sum}"
        );
    }
}

/// Render the bank's unfiltered oscillator, then the same oscillator
/// through the bank's filter, and the first through `reference`.
/// A saw through the bank's lane `filter` (wet), and the same saw
/// unfiltered (dry): what a reference filter runs on.
fn filter_vs_dry(filter: FilterType, cutoff: Hz, res: Resonance) -> (Vec<f32>, Vec<f32>) {
    let mut dry = bank(OscillatorType::Saw, FilterType::None, flat());
    start(&mut dry, 0, Hz(110.0), cutoff, res);
    let dry = render(&mut dry, &[0], 4096, CONTROL_BLOCK);

    let mut wet = bank(OscillatorType::Saw, filter, flat());
    start(&mut wet, 0, Hz(110.0), cutoff, res);
    let wet = render(&mut wet, &[0], 4096, CONTROL_BLOCK);
    (wet, dry)
}

/// The lane SVF is `SvfFilterNode`'s filter: same coefficients (from the
/// shared `SvfCoeffs::new`), same recurrence, same taps.
#[test]
fn the_lane_svf_matches_svf_filter_node() {
    for (mode, ty) in [
        (SvfMode::Lowpass, SvfType::LowPass),
        (SvfMode::Highpass, SvfType::HighPass),
        (SvfMode::Bandpass, SvfType::BandPass),
        (SvfMode::Notch, SvfType::Notch),
    ] {
        let (cutoff, q) = (Hz(900.0), Q(2.0));
        let filter = FilterType::Svf { cutoff, q, mode };
        let (wet, dry) = filter_vs_dry(filter, cutoff, Resonance::NONE);
        // The node as a graph runs it, prepared at the bank's rate.
        let mut node = tutti_graph::Solo::new(
            SvfFilterNode::<f32>::new(ty, cutoff, q),
            tutti_graph::Prepare::new(sr(), tutti_types::Samples(64)),
        );
        let expected = node.render_input(&[&dry]).remove(0);
        for (i, (w, e)) in wet.iter().zip(&expected).enumerate() {
            assert!(
                (w - e).abs() < 1e-4,
                "{mode:?} frame {i}: lane {w}, node {e}"
            );
        }
    }
}

/// The lane ladder is `LadderFilterNode`'s LP24 ladder, up to the `tanh`
/// approximation (worst case 2e-4, at the saturator's clamp).
#[test]
fn the_lane_ladder_matches_ladder_filter_node() {
    let (cutoff, res) = (Hz(1_200.0), Resonance(0.7));
    let filter = FilterType::Moog {
        cutoff,
        resonance: res,
    };
    let (wet, dry) = filter_vs_dry(filter, cutoff, res);
    // The node as a graph runs it, prepared at the bank's rate.
    let mut node = tutti_graph::Solo::new(
        LadderFilterNode::<f32>::new(LadderType::LP24, cutoff, res),
        tutti_graph::Prepare::new(sr(), tutti_types::Samples(64)),
    );
    let expected = node.render_input(&[&dry]).remove(0);
    for (i, (w, e)) in wet.iter().zip(&expected).enumerate() {
        assert!((w - e).abs() < 2e-3, "frame {i}: lane {w}, node {e}");
    }
}

/// A pitch change ramps across the control step, one increment per
/// sample — never a stair.
///
/// Read off a saw well away from its wrap, where the output is the naive
/// ramp `2p - 1` exactly, so each sample's phase step is recoverable.
#[test]
fn a_pitch_change_ramps_every_sample_across_the_step() {
    let mut b = bank(OscillatorType::Saw, FilterType::None, flat());
    start(&mut b, 0, Hz(100.0), Hz(20_000.0), Resonance::NONE);
    b.start(0, Phase(0.1));
    let _ = render(&mut b, &[0], CONTROL_BLOCK, CONTROL_BLOCK);

    b.set_pitch(0, Hz(200.0));
    let out = render(&mut b, &[0], CONTROL_BLOCK + 1, CONTROL_BLOCK);
    let phase: Vec<f64> = out.iter().map(|&y| (f64::from(y) + 1.0) / 2.0).collect();
    let steps: Vec<f64> = phase.windows(2).map(|w| w[1] - w[0]).collect();
    let (from, to) = (100.0 / SR, 200.0 / SR);

    assert!(
        steps
            .windows(2)
            .take(CONTROL_BLOCK - 1)
            .all(|w| w[1] > w[0] + 1e-6),
        "the phase step did not rise every sample: {steps:?}"
    );
    assert!(
        steps[0] > from + 1e-6,
        "the ramp did not start moving on the first sample"
    );
    assert!(
        (steps[CONTROL_BLOCK - 1] - to).abs() < 1e-6,
        "the ramp ended at {} rather than the target {to}",
        steps[CONTROL_BLOCK - 1]
    );
}

/// No two lanes share a noise stream after the unison width changes.
///
/// The case that broke: noise, two voices, stride 1 → 2 with voice 1 never
/// sounding. Voice 1's sub-voice moves from lane 1 to lane 2 carrying its
/// untouched seed, and voice 0's new sub-voice lands on lane 1 — which,
/// seeded by lane index, got that same seed. Two lanes, one noise.
#[test]
fn unison_resizes_never_duplicate_a_noise_stream() {
    let mut b = VoiceBank::new(
        OscillatorType::Noise,
        &FilterType::None,
        &flat(),
        sr(),
        2,
        1,
    );
    for stride in [2, 1, 3, 2, 5] {
        b.resize_stride(stride);
        let states = &b.noise_states()[..2 * stride];
        for (i, a) in states.iter().enumerate() {
            for (j, c) in states.iter().enumerate().skip(i + 1) {
                assert_ne!(
                    a, c,
                    "stride {stride}: lanes {i} and {j} share a noise stream"
                );
            }
        }
    }
}

/// A sustain above unity is clamped, so a retrigger at sustain never drops
/// the level: the attack peaks at 1.0 and would otherwise hand decay a
/// level below where the note was held.
#[test]
fn a_retrigger_at_a_boosted_sustain_does_not_drop() {
    let env = Adsr::new(Seconds(0.001), Seconds(0.001), Amplitude(1.5), Seconds(0.1));
    let mut b = bank(OscillatorType::Sine, FilterType::None, env);
    start(&mut b, 0, Hz(440.0), Hz(20_000.0), Resonance::NONE);
    let held = envelope_trace(&mut b, 0, 500);
    let before = *held.last().unwrap();
    b.gate_on(0);
    let after = envelope_trace(&mut b, 0, 100);
    assert!(
        after.iter().all(|&e| e >= before),
        "retrigger dropped the level from {before} to {:?}",
        after.iter().cloned().fold(f32::INFINITY, f32::min)
    );
    assert!(
        before <= 1.0,
        "the held level {before} exceeds the clamped sustain"
    );
}

/// A pulse width of 0 still renders a pulse, not DC.
#[test]
fn a_zero_pulse_width_still_oscillates() {
    let mut b = bank(
        OscillatorType::Square { pulse_width: 0.0 },
        FilterType::None,
        flat(),
    );
    start(&mut b, 0, Hz(220.0), Hz(20_000.0), Resonance::NONE);
    let out = render(&mut b, &[0], 4800, CONTROL_BLOCK);
    let mean = out.iter().sum::<f32>() / out.len() as f32;
    let ac = (out.iter().map(|s| (s - mean).powi(2)).sum::<f32>() / out.len() as f32).sqrt();
    assert!(ac > 0.05, "a zero-width pulse rendered DC (AC rms {ac})");
}
