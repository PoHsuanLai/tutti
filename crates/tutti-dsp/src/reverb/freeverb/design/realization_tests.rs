use super::*;
use crate::contract::RealizationCase;
use crate::contract::cases::realization as cases;
use crate::testing::flaws::{Defect, Defective};
use crate::{Realization, Setup};

fn controls(room: f32, damp: f32) -> FreeverbControls {
    FreeverbControls {
        room: Mix(room),
        damp: Mix(damp),
    }
}

impl RealizationCase for FreeverbDesign {
    fn probes(&self) -> Vec<FreeverbControls> {
        vec![
            controls(0.5, 0.5),
            controls(0.0, 0.5),
            controls(0.9, 0.2),
            controls(0.5, 1.0),
        ]
    }

    fn extremes(&self) -> Vec<FreeverbControls> {
        vec![controls(1.0, 0.0), controls(1.0, 1.0)]
    }

    fn steps(&self) -> Vec<(FreeverbControls, FreeverbControls)> {
        vec![
            (controls(0.5, 0.5), controls(0.6, 0.5)),
            (controls(0.9, 0.0), controls(0.2, 1.0)),
        ]
    }
}

crate::realization_contract!(FreeverbDesign, Lti, reverb, BlockIndependent);

fn defective(defect: Defect) -> Defective<FreeverbDesign> {
    Defective {
        inner: FreeverbDesign,
        defect,
        tweak: |_| {},
    }
}

#[track_caller]
fn fails(result: Result<(), String>, why: &str) {
    let e = result.expect_err("the case should fail on the defective realization");
    assert!(e.contains(why), "{e}");
}

/// Each case of the contract fails on a Freeverb with the defect its
/// mutation names (the cases' mutation records, run every time).
#[test]
fn a_defective_realization_fails_its_case() {
    // rt60: a loop gain 5% too high rings 39% longer.
    let m = defective(Defect::LoopGain(1.05));
    fails(cases::rt60(&m), "measured RT60");
    // tail: 3% too high rings 2.4 times as long at room 0.9.
    let m = defective(Defect::LoopGain(1.03));
    fails(cases::tail_covers(&m), "dB");
    // stability: a gain that never stops growing.
    fails(cases::stability(&defective(Defect::Runaway)), "");
    // band order: no damping, every band like the lowest.
    let m = defective(Defect::NoDamping);
    fails(cases::band_order(&m), "bands");
    // arrival: a frame late, and a latency that claims a frame.
    fails(
        cases::arrival(&defective(Defect::LateByOne)),
        "first output",
    );
    fails(
        cases::arrival(&defective(Defect::FalseLatency)),
        "first output",
    );
    // latency: one frame more than the network has.
    fails(cases::latency(&defective(Defect::FalseLatency)), "latency");
    // wet only: the input leaks to the output.
    fails(
        cases::wet_only(&defective(Defect::DryLeak)),
        "shortest path",
    );
    // decorrelated: both channels alike.
    fails(
        cases::decorrelated(&defective(Defect::MonoOutput)),
        "correlate",
    );
    // click free: a gain of 4 applied whole at the retarget.
    fails(
        cases::click_free(&defective(Defect::UnslewedGain(4.0))),
        "jump",
    );
    // A modest step too: 1.5 times fails the 3 times allowance (measured
    // margins of the clean realizations: Freeverb 1.98, the FDN 1.51 times the
    // steady jump).
    fails(
        cases::click_free(&defective(Defect::UnslewedGain(1.5))),
        "jump",
    );
    // line lengths: the longest comb 5% long (inside the RT60 tolerance).
    let m = Defective {
        inner: FreeverbDesign,
        defect: Defect::None,
        tweak: |k| k.parts_mut().2.lengthen_last_comb(1722),
    };
    fails(cases::line_lengths(&m), "first echo");
    // reset that does nothing; fresh that keeps the state; a gain that
    // depends on the block.
    fails(cases::reset(&defective(Defect::NoReset)), "reset");
    fails(cases::fresh(&defective(Defect::FreshKeepsState)), "fresh()");
    fails(
        cases::block_independent(&defective(Defect::BlockDependent)),
        "blocks of",
    );
}

/// The `Lti` realization cases fail on a Freeverb with the defect their
/// mutation names (and the measured figures of the clean one, with the
/// margins, are in the cases' own docs).
#[test]
fn a_defective_freeverb_fails_the_lti_realization_cases() {
    use crate::testing::flaws::Lying;
    // linear: a saturation breaks additivity; a gain that moves with the
    // frame is linear and breaks shift invariance.
    fails(cases::linear(&defective(Defect::Tanh)), "additivity");
    fails(
        cases::linear(&defective(Defect::PeriodicGain)),
        "shift invariance",
    );
    // response: the output 10% high from the second retarget on, and every
    // comb gain 5% high.
    fails(
        cases::response(&defective(Defect::UnslewedGain(1.1))),
        "impulse response's spectrum",
    );
    fails(
        cases::response(&defective(Defect::LoopGain(1.05))),
        "impulse response's spectrum",
    );
    // decay: every comb gain 5% high decays at 72% of the radius's rate.
    fails(
        cases::decay(&defective(Defect::LoopGain(1.05))),
        "spectral radius",
    );
    // tail tightness: a model whose tail is twice the truth.
    let lying = Lying {
        inner: FreeverbDesign,
        tail: 2.0,
        hinf: 1.0,
        l1: 1.0,
    };
    fails(cases::tail_tight(&lying), "the tail to");
}

/// At the General MIDI controls `retarget` leaves the combs on exactly the
/// coefficients a freshly built `Freeverb` starts with, bit for bit, so the
/// realization of the General MIDI send renders what the reverb always did: the
/// processing loops are untouched and the figures they read are the same
/// bits. (An invariant of the coefficients, not a render compared with
/// another render.)
#[test]
fn retargeting_to_the_general_midi_network_changes_no_coefficient() {
    let rate = SampleRate(44_100.0);
    let fresh = Freeverb::new(rate);
    // Start from other figures so the retarget is seen to set them.
    let mut realization = FreeverbDesign.realization();
    realization.retarget(&FreeverbDesign.design(&controls(0.9, 0.9), rate));
    assert_ne!(realization.figures().comb, fresh.figures().comb);
    realization.retarget(&FreeverbDesign.design(&FreeverbControls::GENERAL_MIDI, rate));
    let (a, b) = (realization.figures().comb, fresh.figures().comb);
    assert_eq!(
        (a.0.to_bits(), a.1.to_bits(), a.2.to_bits()),
        (b.0.to_bits(), b.1.to_bits(), b.2.to_bits())
    );
}

/// A network the realization does not understand leaves it as it was: no
/// recirculating stage, a stage that is not a one-pole, and figures that are
/// not a stable loop.
#[test]
fn a_network_it_does_not_understand_is_ignored() {
    let rate = SampleRate(44_100.0);
    let mut realization = FreeverbDesign.realization();
    let before = realization.figures().comb;
    realization.retarget(&Network::new(rate));
    let mut flat = FreeverbDesign.design(&controls(0.9, 0.9), rate);
    crate::testing::flaws::map_absorption(&mut flat, |_| Absorption::Gain(Amplitude(0.5)));
    realization.retarget(&flat);
    let mut unstable = FreeverbDesign.design(&controls(0.9, 0.9), rate);
    crate::testing::flaws::map_absorption(&mut unstable, |_| Absorption::OnePole {
        gain: Amplitude(1.5),
        damp: 0.2,
    });
    realization.retarget(&unstable);
    assert_eq!(realization.figures().comb, before);
}

/// A realization prepared at another rate keeps the coefficients it was
/// retargeted to, and its lines are those of a `Freeverb` built at that rate.
#[test]
fn prepare_retunes_the_lines_and_keeps_the_coefficients() {
    let mut realization = FreeverbDesign.realization();
    realization.retarget(&FreeverbDesign.design(&controls(0.9, 0.2), SampleRate::DEFAULT));
    let want = realization.figures().comb;
    let rate = SampleRate(96_000.0);
    let ceiling = Ceiling::at(rate).unwrap();
    realization.prepare(
        &FreeverbDesign.bounds(&ceiling),
        &Setup {
            rate,
            max_block: tutti_types::MaxBlock::of(tutti_types::Samples(64)),
        },
    );
    assert_eq!(realization.figures().comb, want);
    assert_eq!(
        realization.figures().combs_left,
        Freeverb::new(rate).figures().combs_left
    );
}
