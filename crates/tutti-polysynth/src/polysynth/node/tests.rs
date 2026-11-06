use crate::{PolySynth, SynthConfig};

/// **The ports and their bases.** A synth declares its volume, and with
/// a unison engine its detune and spread too ([`SYNTH_PARAMS`](super::SYNTH_PARAMS)),
/// each port's base its cell.
///
/// Mutation (run): `param_base(2)` answering the detune cell →
/// fails. Mutation (run): the shape declaring all three without
/// unison → fails.
#[test]
fn the_shape_declares_volume_and_the_unison_params() {
    use tutti_graph::Node;
    use tutti_types::UnitParam;

    let plain = PolySynth::new(SynthConfig::default()).expect("builds");
    plain.set_volume(0.5);
    assert_eq!(plain.shape().params.as_slice(), &[UnitParam::Volume][..]);
    assert_eq!(plain.param_base(0), Some(0.5));
    assert_eq!(plain.param_base(1), None);

    let unison = PolySynth::new(SynthConfig {
        unison: Some(crate::UnisonConfig {
            voice_count: 3,
            detune_cents: tutti_types::Cents(12.0),
            stereo_spread: tutti_types::Spread::new_clamped(0.25),
            phase_randomize: false,
        }),
        ..SynthConfig::default()
    })
    .expect("builds");
    assert_eq!(unison.shape().params.as_slice(), &super::SYNTH_PARAMS[..]);
    assert_eq!(unison.param_base(1), Some(12.0));
    assert_eq!(unison.param_base(2), Some(0.25));
}
