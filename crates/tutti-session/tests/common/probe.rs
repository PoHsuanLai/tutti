//! Nodes for the modulation suite, copied from `bevy-tutti`'s
//! `tests/common/probe.rs` (design 015 §5.1: the session's tests have
//! their own probe nodes).
//!
//! - [`ProbeUnit`]: a host's `ParamNode` with one param (its knob a `Param`
//!   cell, in its `ParamSet`) that writes the value its param port reads to
//!   its one output, so a render shows what a lane delivers.
//! - [`Pulse`]: a stereo level on a span of executor frames, a follower's
//!   input.
//!
//! `ProbeUnit` is a [`SessionNode`] whose controls are its `ParamSet`, so
//! `Graph::add` addresses its param.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use tutti_core::{AtomicF32, ChannelLayout};
use tutti_graph::{
    Cx, IntoNode, Io, Node, NodeParts, ParamNode, ParamSet, Prepare, Shape, Status,
};
use tutti_session::SessionNode;
use tutti_types::UnitParam;

/// One param (`Cutoff` unless told) whose knob is a cell, and an output that
/// carries the value the param port reads: the port's frames under
/// modulation, the knob without it. `extra` declares a second param after
/// it, which the probe does not read. `Clone` shares the cells.
#[derive(Clone)]
pub struct ProbeUnit {
    param: UnitParam,
    extra: Option<UnitParam>,
    knob: Arc<AtomicF32>,
    other: Arc<AtomicF32>,
}

impl ProbeUnit {
    /// A cutoff probe at `knob`.
    pub fn new(knob: f32) -> Self {
        Self::of(UnitParam::Cutoff, knob)
    }

    /// A probe of `param` at `knob`.
    pub fn of(param: UnitParam, knob: f32) -> Self {
        Self {
            param,
            extra: None,
            knob: Arc::new(AtomicF32::new(knob)),
            other: Arc::new(AtomicF32::new(0.0)),
        }
    }

    /// With a second param, `extra`, after the first: another param list.
    pub fn with_extra(mut self, extra: UnitParam) -> Self {
        self.extra = Some(extra);
        self
    }

    /// The knob cell, shared with every clone.
    pub fn knob(&self) -> Arc<AtomicF32> {
        Arc::clone(&self.knob)
    }
}

impl Node for ProbeUnit {
    fn shape(&self) -> Shape {
        let shape = Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO);
        match self.extra {
            None => shape.with_params(&[self.param]),
            Some(extra) => shape.with_params(&[self.param, extra]),
        }
    }

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        match io.param(0).frames() {
            Some(v) => {
                let v: Vec<f32> = v.to_vec();
                io.output(0).copy_from_slice(&v);
            }
            None => {
                let k = self.knob.load(Ordering::Acquire);
                io.output(0).fill(k);
            }
        }
        Status::Modified
    }

    fn reset(&mut self) {}

    fn param_base(&self, port: usize) -> Option<f32> {
        match port {
            0 => Some(self.knob.load(Ordering::Acquire)),
            1 if self.extra.is_some() => Some(self.other.load(Ordering::Acquire)),
            _ => None,
        }
    }
}

impl ParamNode for ProbeUnit {
    fn param_set(&self) -> ParamSet {
        let set = ParamSet::builder().param(self.param, Arc::clone(&self.knob));
        match self.extra {
            None => set.build(),
            Some(extra) => set.param(extra, Arc::clone(&self.other)).build(),
        }
    }

    fn fork_fresh(&self) -> Self {
        Self {
            param: self.param,
            extra: self.extra,
            knob: Arc::new(AtomicF32::new(self.knob.load(Ordering::Acquire))),
            other: Arc::new(AtomicF32::new(self.other.load(Ordering::Acquire))),
        }
    }
}

impl IntoNode for ProbeUnit {
    type Controls = ParamSet;

    fn into_parts(self) -> NodeParts<ParamSet> {
        tutti_graph::param_parts(self)
    }
}

impl SessionNode for ProbeUnit {
    fn params(controls: &ParamSet) -> Option<ParamSet> {
        Some(controls.clone())
    }
}

/// `level` on executor frames `[from, to)`, else 0, on both channels.
#[derive(Clone)]
pub struct Pulse {
    pub from: u64,
    pub to: u64,
    pub level: f32,
}

impl Node for Pulse {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::STEREO)
    }

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let base = cx.env.frame.get();
        for c in 0..2 {
            for (i, x) in io.output(c).iter_mut().enumerate() {
                let f = base + i as u64;
                *x = if (self.from..self.to).contains(&f) {
                    self.level
                } else {
                    0.0
                };
            }
        }
        Status::Modified
    }

    fn reset(&mut self) {}
}
