//! The feedback delay network as a [`ReverbNode`], and [`ReverbKind`], which
//! spawns the node of either reverb (design 022 step 1c).

use std::sync::Arc;

use tutti_dsp::{FdnControls, FdnDesign, FreeverbControls, FreeverbDesign};
use tutti_graph::{Io, ParamSetBuilder};
use tutti_types::{
    AtomicF32, Db, Depth, Hz, Mix, Param, Seconds, StereoWidth, Unit, UnitAddr, UnitParam,
};

use crate::ramp::{LastGood, block_driven};
use crate::reverb::{FreeverbNode, ReverbDesign, ReverbNode, ReverbParams};

/// One cell, read through [`LastGood`]: a block's value is the port's last frame
/// when the graph modulates it and the cell otherwise, and a non-finite write
/// reads as unchanged.
#[derive(Clone, Debug)]
struct Knob<U: Unit<Raw = f32>> {
    param: Param<U>,
    good: LastGood,
}

impl<U: Unit<Raw = f32>> Knob<U> {
    fn new(value: U, fallback: f32) -> Self {
        Self {
            good: LastGood::new(value.to_raw(), fallback),
            param: Param::new(value),
        }
    }

    fn cell(&self) -> Arc<AtomicF32> {
        self.param.as_atomic()
    }

    fn base(&self) -> f32 {
        self.param.load().to_raw()
    }

    fn read(&mut self, io: &Io<'_>, port: usize) -> U {
        let v = block_driven(io, port).unwrap_or_else(|| self.base());
        U::from_raw(self.good.read(v))
    }

    /// The largest value the port takes in the block, or the cell's when nothing
    /// drives it.
    fn highest(&self, io: &Io<'_>, port: usize) -> Option<U> {
        let frames = io.param(port).frames()?;
        let top = frames
            .iter()
            .copied()
            .filter(|v| v.is_finite())
            .fold(f32::NEG_INFINITY, f32::max);
        Some(U::from_raw(if top.is_finite() { top } else { self.base() }))
    }

    fn detach(&mut self) {
        self.param.detach();
    }
}

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

/// The FDN's cells: the three decays, the size, the diffusion, the pre-delay,
/// the modulation's depth and rate, the width, the wet gain and (if the design
/// can freeze) the freeze.
#[derive(Clone, Debug)]
pub struct FdnParams {
    decay: [Knob<Seconds>; 3],
    size: Knob<Mix>,
    diffusion: Knob<Mix>,
    pre_delay: Knob<Seconds>,
    depth: Knob<Depth>,
    rate: Knob<Hz>,
    width: Knob<StereoWidth>,
    wet: Knob<Db>,
    freeze: Knob<Mix>,
    freezable: bool,
}

impl ReverbParams for FdnParams {
    type Controls = FdnControls;

    fn addrs(&self) -> &'static [UnitAddr] {
        if self.freezable { &ADDRS } else { &ADDRS[..10] }
    }

    fn bind(&self, set: ParamSetBuilder) -> ParamSetBuilder {
        let cells = [
            self.decay[0].cell(),
            self.decay[1].cell(),
            self.decay[2].cell(),
            self.size.cell(),
            self.diffusion.cell(),
            self.pre_delay.cell(),
            self.depth.cell(),
            self.rate.cell(),
            self.width.cell(),
            self.wet.cell(),
            self.freeze.cell(),
        ];
        self.addrs()
            .iter()
            .zip(cells)
            .fold(set, |set, (&addr, cell)| set.param(addr, cell))
    }

    fn base(&self, k: usize) -> Option<f32> {
        if k >= self.addrs().len() {
            return None;
        }
        Some(match k {
            0..=2 => self.decay[k].base(),
            3 => self.size.base(),
            4 => self.diffusion.base(),
            5 => self.pre_delay.base(),
            6 => self.depth.base(),
            7 => self.rate.base(),
            8 => self.width.base(),
            9 => self.wet.base(),
            _ => self.freeze.base(),
        })
    }

    fn read(&mut self, io: &Io<'_>, first: usize) -> FdnControls {
        let at = |k: usize| first + k;
        FdnControls {
            decay: [0, 1, 2].map(|k| self.decay[k].read(io, at(k))),
            size: self.size.read(io, at(3)),
            diffusion: self.diffusion.read(io, at(4)),
            pre_delay: self.pre_delay.read(io, at(5)),
            modulation: self.depth.read(io, at(6)),
            rate: self.rate.read(io, at(7)),
            width: self.width.read(io, at(8)),
            wet: self.wet.read(io, at(9)),
            freeze: self.freezable && self.freeze.read(io, at(10)).get() >= 0.5,
        }
    }

    /// The controls that ring longest in the block: the longest decay, the
    /// biggest room, the longest pre-delay and a freeze if any frame has one.
    /// What a port does not drive is the cell's.
    fn longest(&self, io: &Io<'_>, first: usize) -> Option<FdnControls> {
        let at = |k: usize| first + k;
        let driven = (0..self.addrs().len()).any(|k| io.param(at(k)).frames().is_some());
        if !driven {
            return None;
        }
        let p = self;
        let base = FdnControls {
            decay: [0, 1, 2].map(|k| Seconds::from_raw(p.decay[k].base())),
            size: Mix::from_raw(p.size.base()),
            diffusion: Mix::from_raw(p.diffusion.base()),
            pre_delay: Seconds::from_raw(p.pre_delay.base()),
            modulation: Depth::from_raw(p.depth.base()),
            rate: Hz::from_raw(p.rate.base()),
            width: StereoWidth::from_raw(p.width.base()),
            wet: Db::from_raw(p.wet.base()),
            freeze: false,
        };
        let frozen = self.freezable
            && io
                .param(at(10))
                .frames()
                .is_some_and(|f| f.iter().any(|&v| v >= 0.5));
        Some(FdnControls {
            decay: [0, 1, 2].map(|k| self.decay[k].highest(io, at(k)).unwrap_or(base.decay[k])),
            size: self.size.highest(io, at(3)).unwrap_or(base.size),
            pre_delay: self.pre_delay.highest(io, at(5)).unwrap_or(base.pre_delay),
            freeze: frozen || (self.freezable && self.freeze.base() >= 0.5),
            ..base
        })
    }

    fn detach(&mut self) {
        self.decay.iter_mut().for_each(Knob::detach);
        self.size.detach();
        self.diffusion.detach();
        self.pre_delay.detach();
        self.depth.detach();
        self.rate.detach();
        self.width.detach();
        self.wet.detach();
        self.freeze.detach();
    }
}

impl ReverbDesign for FdnDesign {
    type Params = FdnParams;

    fn params(&self, c: FdnControls) -> FdnParams {
        let d = FdnControls::DEFAULT;
        FdnParams {
            decay: [0, 1, 2].map(|k| Knob::new(c.decay[k], d.decay[k].get())),
            size: Knob::new(c.size, d.size.get()),
            diffusion: Knob::new(c.diffusion, d.diffusion.get()),
            pre_delay: Knob::new(c.pre_delay, 0.0),
            depth: Knob::new(c.modulation, d.modulation.get()),
            rate: Knob::new(c.rate, d.rate.get()),
            width: Knob::new(c.width, d.width.get()),
            wet: Knob::new(c.wet, 0.0),
            freeze: Knob::new(Mix(if c.freeze { 1.0 } else { 0.0 }), 0.0),
            freezable: self.freezable(),
        }
    }
}

/// The feedback delay network as a graph node: mono in, stereo out.
pub type FdnNode = ReverbNode<FdnDesign>;

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
            ReverbKind::Freeverb => SpawnedReverb::Freeverb(ReverbNode::new(
                FreeverbDesign,
                FreeverbControls::GENERAL_MIDI,
            )),
            ReverbKind::Fdn(design) => {
                SpawnedReverb::Fdn(ReverbNode::new(design, FdnControls::DEFAULT))
            }
        }
    }
}
