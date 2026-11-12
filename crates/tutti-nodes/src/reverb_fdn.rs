//! The feedback delay network as a [`DesignNode`], and [`ReverbKind`], which
//! spawns the node of either reverb (design 022 step 1c).

use tutti_dsp::{FdnControls, FdnDesign, FreeverbControls, FreeverbDesign};
use tutti_types::{Db, Depth, Hz, Mix, Seconds, StereoWidth};

use crate::reverb::{DesignNode, FreeverbNode, NodeDesign};

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

impl NodeDesign for FdnDesign {
    type Set = FdnSet;

    fn set(&self, _: &FdnControls) -> FdnSet {
        FdnSet {
            freezable: self.freezable(),
        }
    }
}

/// The feedback delay network as a graph node: mono in, stereo out.
pub type FdnNode = DesignNode<FdnDesign>;

/// Which reverb to spawn: the choice is explicit and built on the control side.
/// It only spawns the matching node ([`spawn`](Self::spawn)); there is no
/// "auto", and the enum holds no policy.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ReverbKind {
    /// The Freeverb: a fixed room with a size and a damping.
    Freeverb,
    /// The feedback delay network, built to this design: three band decays, a
    /// size, a diffusion, a pre-delay, a modulation, a width, a wet gain and
    /// (if the design is freezable) a freeze.
    Fdn(FdnDesign),
}

/// The node [`ReverbKind::spawn`] made.
// A control-side value a caller matches and inserts at once; boxing the larger
// node would only make the caller unbox it.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum SpawnedReverb {
    /// A live Freeverb.
    Freeverb(FreeverbNode),
    /// A live feedback delay network.
    Fdn(FdnNode),
}

impl ReverbKind {
    /// A live node of this kind, its cells at the kind's default controls (the
    /// General MIDI room for Freeverb, [`FdnControls::DEFAULT`] for the FDN).
    /// Allocates: build it before it goes live.
    #[must_use]
    pub fn spawn(self) -> SpawnedReverb {
        match self {
            ReverbKind::Freeverb => SpawnedReverb::Freeverb(DesignNode::live(
                FreeverbDesign,
                FreeverbControls::GENERAL_MIDI,
            )),
            ReverbKind::Fdn(design) => {
                SpawnedReverb::Fdn(DesignNode::live(design, FdnControls::DEFAULT))
            }
        }
    }
}
