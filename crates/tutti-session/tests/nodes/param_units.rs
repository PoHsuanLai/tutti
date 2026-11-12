//! The unit of every engine node's params, as a [`Key`](tutti_session::Key)'s
//! methods hand it out (doc 030, N1): `key.cutoff()` is a `TypedParamRef<U>`
//! with `U` the node's [`HasParam`] unit, which the node's own declaration
//! writes from its cell's type. The table is the spec's: each param is in
//! the unit a reader of the node's docs expects (the ladder's resonance is
//! addressed as `Q` and is in `Resonance`; the chorus's depth is a swing in
//! `Seconds`, the phaser's a fraction).

use std::any::TypeId;

use tutti_session::HasParam;
use tutti_types::{
    Amplitude, Azimuth, CompressionRatio, Db, Depth, Drive, Elevation, Feedback, Hz, Mix, Pan, Q,
    Resonance, Seconds, Spread, StereoWidth, UnitParam,
};

/// Whether `N`'s param of kind `P` is in `U`: the unit a key's method for it
/// returns.
fn is_in<N: HasParam<P>, const P: u16, U: 'static>(node: &str, kind: UnitParam)
where
    N::Unit: 'static,
{
    assert_eq!(
        TypeId::of::<N::Unit>(),
        TypeId::of::<U>(),
        "{node}: {kind:?} is not in {}",
        std::any::type_name::<U>()
    );
}

macro_rules! units {
    ($($test:ident $(#[$cfg:meta])* { $($node:ty { $($kind:ident => $unit:ty),* $(,)? })* })*) => {$(
        $(#[$cfg])*
        #[test]
        fn $test() {
            $($(
                is_in::<$node, { UnitParam::$kind as u16 }, $unit>(
                    stringify!($node),
                    UnitParam::$kind,
                );
            )*)*
        }
    )*};
}

units! {
    every_engine_nodes_params_are_in_their_declared_unit {
        tutti_nodes::SvfFilterNode<f32> { Cutoff => Hz, Q => Q, GainDb => Db }
        tutti_nodes::SvfFilterNode<f64> { Cutoff => Hz, Q => Q, GainDb => Db }
        tutti_nodes::EqBandNode<f32> { Cutoff => Hz, Q => Q, GainDb => Db }
        tutti_nodes::EqBandNode<f64> { Cutoff => Hz, Q => Q, GainDb => Db }
        tutti_nodes::LadderFilterNode<f32> { Cutoff => Hz, Q => Resonance, Drive => Drive }
        tutti_nodes::LadderFilterNode<f64> { Cutoff => Hz, Q => Resonance, Drive => Drive }
        tutti_nodes::CompressorNode {
            Threshold => Db, Ratio => CompressionRatio, Attack => Seconds, Release => Seconds,
            GainDb => Db
        }
        tutti_nodes::GateNode { Threshold => Db, Attack => Seconds, Release => Seconds }
        tutti_nodes::LimiterNode { Threshold => Db, Ceiling => Db, Release => Seconds }
        tutti_nodes::BrickwallLimiterNode { Ceiling => Db }
        tutti_nodes::DistortionNode { Drive => Drive }
        tutti_nodes::BusStripNode { Volume => Amplitude, Pan => Pan }
        tutti_nodes::CrossoverNode { Cutoff => Hz }
        tutti_nodes::MultibandCompressorNode {
            Cutoff => Hz, Threshold => Db, Knee => Db, Ratio => CompressionRatio,
            Attack => Seconds, Release => Seconds, GainDb => Db, Bypass => bool, Solo => bool
        }
        tutti_nodes::DelayLineNode { Feedback => Feedback, DelayTime => Seconds }
        tutti_nodes::ModDelayNode { Rate => Hz, Depth => Seconds, Feedback => Feedback }
        tutti_nodes::PhaserNode { Rate => Hz, Depth => Depth, Feedback => Feedback }
        tutti_nodes::LfoNode { Depth => Depth, Rate => Hz }
        tutti_nodes::FreeverbNode { RoomSize => Mix, Damping => Mix }
        tutti_nodes::FdnNode {
            Decay => Seconds, RoomSize => Mix, Diffusion => Mix, DelayTime => Seconds,
            Depth => Depth, Rate => Hz, StereoSpread => StereoWidth, GainDb => Db, Freeze => bool
        }
        tutti_nodes::VbapPannerNode {
            Azimuth => Azimuth, Elevation => Elevation, Spread => Spread,
            StereoWidth => StereoWidth
        }
        tutti_nodes::HrtfBinauralNode { Azimuth => Azimuth, Elevation => Elevation, Wet => Mix }
    }
    the_synths_params_are_in_their_declared_unit #[cfg(feature = "synth")] {
        tutti_polysynth::PolySynth { Volume => Amplitude, Detune => tutti_types::Cents, StereoSpread => Spread }
    }
    the_samplers_params_are_in_their_declared_unit #[cfg(feature = "sampler")] {
        tutti_sampler::VoiceNode { Volume => Amplitude }
        tutti_sampler::MemorySource { Volume => Amplitude }
        tutti_sampler::Clip { Volume => Amplitude }
    }
}
