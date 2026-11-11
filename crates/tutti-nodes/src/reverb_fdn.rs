//! The feedback delay network as a [`DesignNode`], and [`ReverbKind`], which
//! spawns the node of either reverb (design 022 step 1c).

use tutti_dsp::{FdnControls, FdnDesign, FreeverbControls, FreeverbDesign};
use tutti_types::{Db, Depth, Hz, Mix, Seconds, StereoWidth, UnitAddr, UnitParam};

use crate::blend::DryWet;
use crate::params::{ControlSet, Params};
use crate::reverb::{DesignNode, FreeverbNode, NodeDesign};

/// The addresses of a freezable FDN's params, in port order after the mix; a
/// design that cannot freeze leaves off the last.
const ADDRS: [UnitAddr; 11] = [
    UnitAddr::new(UnitParam::Decay, 0),
    UnitAddr::new(UnitParam::Decay, 1),
    UnitAddr::new(UnitParam::Decay, 2),
    UnitAddr::first(UnitParam::RoomSize),
    UnitAddr::first(UnitParam::Diffusion),
    UnitAddr::first(UnitParam::DelayTime),
    UnitAddr::first(UnitParam::Depth),
    UnitAddr::first(UnitParam::Rate),
    UnitAddr::first(UnitParam::StereoSpread),
    UnitAddr::new(UnitParam::GainDb, 0),
    UnitAddr::first(UnitParam::Freeze),
];

/// The FDN's controls: the three decays, the size (a move lands over seconds by
/// design: half a frame per 32-frame chunk, about 6 s for the longest line going
/// from 0 to 1, to avoid pitch artefacts), the diffusion, the pre-delay,
/// the modulation's depth and rate, the width, the wet gain and (if the design
/// can freeze) the freeze, a cell at or above 0.5.
#[derive(Clone, Copy, Debug)]
pub struct FdnSet {
    freezable: bool,
}

impl ControlSet for FdnSet {
    type Controls = FdnControls;

    fn addrs(&self) -> &'static [UnitAddr] {
        if self.freezable { &ADDRS } else { &ADDRS[..10] }
    }

    fn defaults(&self) -> FdnControls {
        FdnControls::DEFAULT
    }

    fn flatten(&self, c: &FdnControls, out: &mut [f32]) {
        let all = [
            c.decay[0].to_f32(),
            c.decay[1].to_f32(),
            c.decay[2].to_f32(),
            c.size.get(),
            c.diffusion.get(),
            c.pre_delay.to_f32(),
            c.modulation.get(),
            c.rate.get(),
            c.width.get(),
            c.wet.get(),
            f32::from(u8::from(c.freeze)),
        ];
        out.copy_from_slice(&all[..out.len()]);
    }

    fn assemble(&self, v: &[f32]) -> FdnControls {
        FdnControls {
            decay: [0, 1, 2].map(|k| Seconds::from_f32(v[k])),
            size: Mix(v[3]),
            diffusion: Mix(v[4]),
            pre_delay: Seconds::from_f32(v[5]),
            modulation: Depth(v[6]),
            rate: Hz(v[7]),
            width: StereoWidth(v[8]),
            wet: Db(v[9]),
            freeze: self.freezable && v[10] >= 0.5,
        }
    }
}

impl NodeDesign for FdnDesign {
    type Params = Params<FdnSet>;
    type Blend = DryWet;

    fn params(&self, c: FdnControls) -> Self::Params {
        Params::new(
            FdnSet {
                freezable: self.freezable(),
            },
            &c,
        )
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
