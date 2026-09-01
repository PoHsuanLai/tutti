//! [`ParamPorts`] — a node declares its own audio-rate param-input ports.
//!
//! A DSP node that exposes an audio-rate input port for one of its scalar
//! parameters (a filter's cutoff, a compressor's threshold, …) answers for
//! itself where that port is. This replaces the old approach where a closed
//! `NodeKind` enum was matched against a hardcoded table
//! (`node_kind_supports`) living in the DAW layer — a denormalized copy of the
//! `*_port()` accessors these nodes already have.
//!
//! With this trait the knowledge lives on the node, where it belongs: the graph
//! just produces samples, and *what ports a node has* is the node's business.
//! A host that wants to modulate a param at audio rate looks up the port with
//! [`ParamPorts::param_port`] and connects a source into it — no central table,
//! no node rebuild, no enum to edit when a new effect gains a port.
//!
//! Each impl is a thin dispatch over the node's existing typed `*_port()`
//! accessors (`cutoff_port`, `drive_port`, `threshold_port`, …).

use tutti_core::dsp::Real;
use tutti_core::UnitParam;

use crate::{
    CompressorNode, DistortionNode, GateNode, LimiterNode, StereoDelayLineNode, StereoLadderFilterNode,
    StereoSvfFilterNode,
};

/// A node that may expose audio-rate input ports for its scalar parameters.
///
/// Returns the input-port index for `param`, or `None` if this node does not
/// expose an audio-rate port for it. The port indices follow the node's audio
/// (and sidechain) inputs — see each node's `with_param_inputs` constructor.
///
/// # Why the node answers, rather than a table
///
/// The knowledge of *what ports a node has* belongs to the node. The
/// alternative is a central map from node kind to port index maintained by the
/// host, which is a denormalized copy of the `*_port()` accessors these nodes
/// already carry: it goes stale the moment a node gains a port and nothing
/// forces the two to agree, so an audio-rate route binds to the wrong input or
/// to none. Asking the node removes the second copy — a host modulating a param
/// at audio rate looks up the port here and connects a source into it, with no
/// enum to edit when a new effect gains one.
///
/// `None` is a real answer, not a failure: most nodes expose no audio-rate port
/// for most params, and a caller that gets it must fall back to a control-rate
/// write rather than treating it as an error.
pub trait ParamPorts {
    /// Input-port index of the audio-rate override for `param`, if present.
    fn param_port(&self, param: UnitParam) -> Option<usize>;
}

impl<F: Real> ParamPorts for StereoSvfFilterNode<F> {
    fn param_port(&self, param: UnitParam) -> Option<usize> {
        match param {
            UnitParam::Cutoff => self.cutoff_port(),
            UnitParam::Q => self.q_port(),
            _ => None,
        }
    }
}

impl<F: Real> ParamPorts for StereoLadderFilterNode<F> {
    fn param_port(&self, param: UnitParam) -> Option<usize> {
        match param {
            UnitParam::Cutoff => self.cutoff_port(),
            UnitParam::Q => self.q_port(),
            UnitParam::Drive => self.drive_port(),
            _ => None,
        }
    }
}

impl ParamPorts for StereoDelayLineNode {
    fn param_port(&self, param: UnitParam) -> Option<usize> {
        match param {
            UnitParam::Feedback => self.feedback_port(),
            UnitParam::DelayTime => self.delay_time_port(),
            _ => None,
        }
    }
}

impl ParamPorts for DistortionNode {
    fn param_port(&self, param: UnitParam) -> Option<usize> {
        match param {
            UnitParam::Drive => self.drive_port(),
            _ => None,
        }
    }
}

impl ParamPorts for CompressorNode {
    fn param_port(&self, param: UnitParam) -> Option<usize> {
        match param {
            UnitParam::Threshold => self.threshold_port(),
            _ => None,
        }
    }
}

impl ParamPorts for GateNode {
    fn param_port(&self, param: UnitParam) -> Option<usize> {
        match param {
            UnitParam::Threshold => self.threshold_port(),
            _ => None,
        }
    }
}

impl ParamPorts for LimiterNode {
    fn param_port(&self, param: UnitParam) -> Option<usize> {
        match param {
            UnitParam::Ceiling => self.ceiling_port(),
            UnitParam::Threshold => self.threshold_port(),
            _ => None,
        }
    }
}
