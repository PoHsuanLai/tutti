//! A node with two params of one kind, for the indexed-address suite.
//!
//! [`Bands`] addresses a `Threshold` at index 0 and another at index 1, each a
//! cell of its own, and writes what port 0 reads to output 0 and what port
//! 1 reads to output 1 (the port's frames under modulation, the knob
//! without it). It is a [`SessionNode`] whose controls are its `ParamSet`.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use tutti_core::{AtomicF32, ChannelLayout};
use tutti_graph::{
    Cx, IntoNode, Io, Node, NodeParts, ParamInput, ParamNode, ParamSet, Prepare, Shape, Status,
};
use tutti_session::SessionNode;
use tutti_types::{UnitAddr, UnitParam};

/// `Threshold` of band 0 and of band 1.
pub const BAND0: UnitAddr = UnitAddr::new(UnitParam::Threshold, 0);
pub const BAND1: UnitAddr = UnitAddr::new(UnitParam::Threshold, 1);

/// Two thresholds, as above. `Clone` shares the cells.
#[derive(Clone)]
pub struct Bands {
    knobs: [Arc<AtomicF32>; 2],
}

impl Bands {
    /// Band 0 at `t0`, band 1 at `t1`.
    pub fn new(t0: f32, t1: f32) -> Self {
        Self {
            knobs: [Arc::new(AtomicF32::new(t0)), Arc::new(AtomicF32::new(t1))],
        }
    }
}

impl Node for Bands {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::STEREO).with_param_addrs(&[BAND0, BAND1])
    }

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        for k in 0..2 {
            match io.param(k) {
                ParamInput::Frames(v) => io.output(k).copy_from_slice(v),
                ParamInput::Base => {
                    let base = self.knobs[k].load(Ordering::Acquire);
                    io.output(k).fill(base);
                }
            }
        }
        Status::Modified
    }

    fn reset(&mut self) {}

    fn param_base(&self, port: usize) -> Option<f32> {
        self.knobs.get(port).map(|k| k.load(Ordering::Acquire))
    }
}

impl ParamNode for Bands {
    fn param_set(&self) -> ParamSet {
        ParamSet::builder()
            .param(BAND0, Arc::clone(&self.knobs[0]))
            .param(BAND1, Arc::clone(&self.knobs[1]))
            .build()
    }

    fn fork_fresh(&self) -> Self {
        Self::new(
            self.knobs[0].load(Ordering::Acquire),
            self.knobs[1].load(Ordering::Acquire),
        )
    }
}

impl IntoNode for Bands {
    type Controls = ParamSet;

    fn into_parts(self) -> NodeParts<ParamSet> {
        tutti_graph::param_parts(self)
    }
}

impl SessionNode for Bands {
    fn params(controls: &ParamSet) -> Option<ParamSet> {
        Some(controls.clone())
    }
}
