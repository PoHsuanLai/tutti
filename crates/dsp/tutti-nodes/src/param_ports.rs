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

use tutti_core::Real;
use tutti_core::UnitParam;

use crate::{
    CompressorNode, DelayLineNode, DistortionNode, GateNode, LadderFilterNode, LimiterNode,
    SvfFilterNode,
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

impl<F: Real> ParamPorts for SvfFilterNode<F> {
    fn param_port(&self, param: UnitParam) -> Option<usize> {
        match param {
            UnitParam::Cutoff => self.cutoff_port(),
            UnitParam::Q => self.q_port(),
            _ => None,
        }
    }
}

impl<F: Real> ParamPorts for LadderFilterNode<F> {
    fn param_port(&self, param: UnitParam) -> Option<usize> {
        match param {
            UnitParam::Cutoff => self.cutoff_port(),
            UnitParam::Q => self.q_port(),
            UnitParam::Drive => self.drive_port(),
            _ => None,
        }
    }
}

impl ParamPorts for DelayLineNode {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{noise, process_block};
    use crate::{LadderType, SvfType};
    use tutti_core::{AudioUnit, ChannelLayout, SampleRate};

    /// Render one block with every param port held at its value in `held`,
    /// except `param`'s, which is held at `value`. The audio inputs carry
    /// noise. The port for each param is the one the node *advertises*.
    fn render_with_port<N: AudioUnit + ParamPorts>(
        mut node: N,
        held: &[(UnitParam, f32)],
        param: UnitParam,
        value: f32,
    ) -> Vec<Vec<f32>> {
        node.set_sample_rate(SampleRate(48_000.0));
        let width = node.outputs();
        let mut ports: Vec<usize> = held
            .iter()
            .map(|&(p, _)| node.param_port(p).expect("every held param has a port"))
            .collect();
        ports.sort_unstable();
        ports.dedup();
        assert_eq!(
            ports.len(),
            held.len(),
            "one distinct port per param: {ports:?}"
        );
        let mut inputs: Vec<Vec<f32>> = (0..node.inputs())
            .map(|c| noise(c as u32 + 40, 64))
            .collect();
        for &(p, v) in held {
            let port = node.param_port(p).expect("every held param has a port");
            inputs[port] = vec![if p == param { value } else { v }; 64];
        }
        assert_eq!(
            node.inputs(),
            width + held.len(),
            "the ports follow the audio inputs, one each"
        );
        let refs: Vec<&[f32]> = inputs.iter().map(|v| &v[..]).collect();
        process_block(&mut node, &refs)
    }

    /// At every width, the port a node advertises for a param is the input its
    /// DSP reads for it: moving only that input moves the output.
    ///
    /// This is the property `ParamPortMap` relies on, and the one a merge can
    /// break silently — the indices move with the width, so an advertised index
    /// computed from one width and a read computed from another would still
    /// report the right arity.
    ///
    /// Mutation (each run, each fails): `SvfFilterNode::q_port` returning the
    /// cutoff port's index; `DelayLineNode::delay_time_port` ignoring
    /// `mod_feedback`; the SVF's `process` reading the Q port as its cutoff;
    /// the ladder reading its drive one port early.
    #[test]
    fn the_advertised_port_is_the_one_the_dsp_reads_at_every_width() {
        for w in [1usize, 2, 6] {
            let layout = ChannelLayout::from(w);
            let svf_held = [(UnitParam::Cutoff, 1_000.0), (UnitParam::Q, 0.707)];
            for (param, a, b) in [
                (UnitParam::Cutoff, 200.0, 8_000.0),
                (UnitParam::Q, 0.5, 8.0),
            ] {
                let mk = || {
                    SvfFilterNode::<f64>::with_param_inputs(
                        layout,
                        SvfType::LowPass,
                        1_000.0,
                        0.707,
                        true,
                        true,
                    )
                };
                assert_ne!(
                    render_with_port(mk(), &svf_held, param, a),
                    render_with_port(mk(), &svf_held, param, b),
                    "svf width {w}: {param:?}"
                );
            }
            let ladder_held = [
                (UnitParam::Cutoff, 1_000.0),
                (UnitParam::Q, 0.3),
                (UnitParam::Drive, 1.0),
            ];
            for (param, a, b) in [
                (UnitParam::Cutoff, 200.0, 8_000.0),
                (UnitParam::Q, 0.0, 0.9),
                (UnitParam::Drive, 1.0, 8.0),
            ] {
                let mk = || {
                    LadderFilterNode::<f64>::with_param_inputs(
                        layout,
                        LadderType::LP24,
                        1_000.0,
                        0.3,
                        true,
                        true,
                        true,
                    )
                };
                assert_ne!(
                    render_with_port(mk(), &ladder_held, param, a),
                    render_with_port(mk(), &ladder_held, param, b),
                    "ladder width {w}: {param:?}"
                );
            }
            let delay_held = [(UnitParam::Feedback, 0.5), (UnitParam::DelayTime, 0.0003)];
            for (param, a, b) in [
                (UnitParam::Feedback, 0.0, 0.9),
                (UnitParam::DelayTime, 0.0002, 0.0008),
            ] {
                let mk = || DelayLineNode::with_param_inputs(layout, 0.01, 0.0003, 0.5, true, true);
                assert_ne!(
                    render_with_port(mk(), &delay_held, param, a),
                    render_with_port(mk(), &delay_held, param, b),
                    "delay width {w}: {param:?}"
                );
            }
        }
    }

    /// The id follows the audio width, never the port count: a mono node with
    /// param ports is still the mono shape, and a stereo one the wide shape.
    ///
    /// Mutation: keying `get_id` on `self.inputs() == 1` (which counts the
    /// ports) fails every "mono, ported" case.
    #[test]
    fn get_id_follows_the_audio_width_not_the_port_count() {
        use crate::node_id::{
            DELAY_LINE_ID, LADDER_FILTER_ID, STEREO_DELAY_LINE_ID, SVF_FILTER_ID,
        };
        for (w, ported) in [(1usize, false), (1, true), (2, false), (2, true)] {
            let layout = ChannelLayout::from(w);
            let svf = SvfFilterNode::<f64>::with_param_inputs(
                layout,
                SvfType::LowPass,
                1e3,
                0.7,
                ported,
                ported,
            );
            let ladder = LadderFilterNode::<f64>::with_param_inputs(
                layout,
                LadderType::LP24,
                1e3,
                0.3,
                ported,
                ported,
                ported,
            );
            let delay = DelayLineNode::with_param_inputs(layout, 0.1, 0.01, 0.3, ported, ported);
            let mono = w == 1;
            let what = format!("width {w}, ported {ported}");
            assert_eq!(svf.get_id() == SVF_FILTER_ID, mono, "svf {what}");
            assert_eq!(ladder.get_id() == LADDER_FILTER_ID, mono, "ladder {what}");
            assert_eq!(
                delay.get_id(),
                if mono {
                    DELAY_LINE_ID
                } else {
                    STEREO_DELAY_LINE_ID
                },
                "delay {what}"
            );
        }
    }
}
