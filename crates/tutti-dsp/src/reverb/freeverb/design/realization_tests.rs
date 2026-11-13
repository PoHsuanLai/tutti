use super::*;
use crate::Setup;
use crate::contract::RecurrenceCase;

fn controls(room: f32, damp: f32) -> FreeverbControls {
    FreeverbControls {
        room: Mix(room),
        damp: Mix(damp),
    }
}

impl RecurrenceCase for FreeverbFamily {
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

crate::recurrence_contract!(FreeverbFamily, Lti, reverb, BlockIndependent);

/// At the General MIDI controls `retarget` leaves the combs on exactly the
/// coefficients a freshly built `Freeverb` starts with, bit for bit, so the
/// realization of the General MIDI send renders what the reverb always did: the
/// processing loops are untouched and the figures they read are the same
/// bits. (An invariant of the coefficients, not a render compared with
/// another render.)
#[test]
fn retargeting_to_the_general_midi_network_changes_no_coefficient() {
    let rate = SampleRate::SR_44K1;
    let fresh = Freeverb::new(rate);
    // Start from other figures so the retarget is seen to set them.
    let mut realization = FreeverbFamily.realized();
    realization.retarget(&FreeverbFamily.member(&controls(0.9, 0.9), rate));
    assert_ne!(realization.figures().comb, fresh.figures().comb);
    realization.retarget(&FreeverbFamily.member(&FreeverbControls::GENERAL_MIDI, rate));
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
    let rate = SampleRate::SR_44K1;
    let mut realization = FreeverbFamily.realized();
    let before = realization.figures().comb;
    realization.retarget(&Network::new(rate));
    let mut flat = FreeverbFamily.member(&controls(0.9, 0.9), rate);
    crate::testing::map_absorption(&mut flat, |_| Absorption::Gain(Amplitude(0.5)));
    realization.retarget(&flat);
    let mut unstable = FreeverbFamily.member(&controls(0.9, 0.9), rate);
    crate::testing::map_absorption(&mut unstable, |_| Absorption::OnePole {
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
    let mut realization = FreeverbFamily.realized();
    realization.retarget(&FreeverbFamily.member(&controls(0.9, 0.2), SampleRate::DEFAULT));
    let want = realization.figures().comb;
    let rate = SampleRate::SR_96K;
    let reach = Reach::at(rate);
    realization.prepare(
        &FreeverbFamily.reach(&reach),
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
