//! [`DriveUnit`]: a host's own node, for the capture and value-path suites.
//!
//! The capture and value-path suites pin what happens to a node bound
//! through `spawn_audio_node` / `AudioGraphRes::insert`: its controls are
//! captured before it goes in, and a route reaches its own cell. Their
//! fixture was `DistortionNode`, then a test-local `AudioUnit` resolved
//! through the `ModTargetRegistry`'s type registration; both paths became
//! one when `Legacy` went (doc 013, "Legacy deleted"). It is now what a host
//! writes for a node of its own: a `ParamNode` registered with
//! [`param_graph_node!`](bevy_tutti::param_graph_node), which is the path
//! these suites pin.

use std::sync::Arc;

use bevy_tutti::param_graph_node;
use tutti_core::{AtomicF32, ChannelLayout, Drive, Param};
use tutti_graph::{Cx, IntoNode, Io, Node, NodeParts, ParamNode, ParamSet, Prepare, Shape, Status};
use tutti_types::UnitParam;

/// One input, one output: the input times its drive. `Drive` is its one
/// param (`UnitParam::Drive`, in its `ParamSet`), so its one control-rate
/// target mirrors into the cell it reads; `Clone` shares the cell.
#[derive(Clone)]
pub struct DriveUnit {
    drive: Param<Drive>,
}

impl DriveUnit {
    /// A node at `drive`.
    pub fn new(drive: f32) -> Self {
        Self {
            drive: Param::new(Drive(drive)),
        }
    }

    /// The drive cell the node reads, shared with every clone.
    pub fn drive(&self) -> Arc<AtomicF32> {
        self.drive.as_atomic()
    }
}

impl Node for DriveUnit {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::MONO, ChannelLayout::MONO)
    }

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let d = self.drive.load().get();
        let (ins, mut outs) = io.split();
        for (y, x) in outs.get(0).iter_mut().zip(ins.get(0)) {
            *y = x * d;
        }
        Status::Modified
    }

    fn reset(&mut self) {}
}

impl ParamNode for DriveUnit {
    fn param_set(&self) -> ParamSet {
        ParamSet::builder()
            .param(UnitParam::Drive, self.drive.as_atomic())
            .build()
    }

    fn fork_fresh(&self) -> Self {
        let mut fork = self.clone();
        fork.drive.detach();
        fork
    }
}

impl IntoNode for DriveUnit {
    type Controls = ParamSet;

    fn into_parts(self) -> NodeParts<ParamSet> {
        tutti_graph::param_parts(self)
    }
}

param_graph_node!(DriveUnit);
