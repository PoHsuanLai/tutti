//! The feedback delay network as a [`FamilyNode`].

use tutti_dsp::{FdnControls, FdnFamily, NetworkCapacity};
use tutti_types::{Db, Depth, Hz, Mix, SampleRate, Seconds, StereoWidth};

use crate::reverb::{FamilyNode, NodeFamily};

/// The FDN's controls: the three decays, the size (a move lands over seconds by
/// design: half a frame per 32-frame chunk, about 6 s for the longest line going
/// from 0 to 1, to avoid pitch artefacts), the diffusion, the pre-delay,
/// the modulation's depth and rate, the width, the wet gain and (if the design
/// can freeze) the freeze, a cell at or above 0.5.
#[derive(Clone, Copy, Debug)]
pub struct FdnSet {
    freezable: bool,
}

// In port order after the mix; a design that cannot freeze leaves off the
// last.
control_set! {
    FdnSet(s): FdnControls = FdnControls { freeze: false, ..FdnControls::DEFAULT };
    live: (if s.freezable { 11 } else { 10 });
    Decay[0] => Seconds = decay[0],
    Decay[1] => Seconds = decay[1],
    Decay[2] => Seconds = decay[2],
    RoomSize => Mix = size,
    Diffusion => Mix = diffusion,
    DelayTime => Seconds = pre_delay,
    Depth => Depth = modulation,
    Rate => Hz = rate,
    StereoSpread => StereoWidth = width,
    GainDb => Db = wet,
    Freeze => bool = freeze;

    fn defaults(&self) -> FdnControls {
        FdnControls::DEFAULT
    }
}

impl NodeFamily for FdnFamily {
    type Set = FdnSet;

    fn set(&self, _: &FdnControls) -> FdnSet {
        FdnSet {
            freezable: self.freezable(),
        }
    }

    fn capacity(&self, rate: SampleRate) -> NetworkCapacity {
        self.grown(rate)
    }
}

/// The feedback delay network as a graph node: mono in, stereo out.
pub type FdnNode = FamilyNode<FdnFamily>;
