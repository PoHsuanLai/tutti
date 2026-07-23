//! Delay compensation for [`Net`].
//!
//! The algorithm is [`tutti_types::latency`] — pure graph math, no audio. This
//! module supplies the audio half: the [`PdcDelay`] node that carries a
//! compensation delay, and the [`LatencyGraph`] / [`DelayInsertion`] impls that
//! let the planner drive a [`Net`]. Latency is already a `Net` concept
//! ([`AudioUnit::latency`]), so aligning paths by it belongs here too.
//!
//! Compensation is explicit. Nothing here runs unless a caller asks for it:
//!
//! ```ignore
//! use tutti_types::latency;
//!
//! let plan = latency::compensate(&mut net);   // splices delays
//! net.commit();                               // publishes to the audio thread
//! ```
//!
//! The returned [`Compensation`](tutti_types::latency::Compensation) tells
//! sources *outside* the graph — a sampler streaming from disk — how far to
//! pre-roll. Publishing that to them is the caller's business.

mod delay;

pub use delay::PdcDelay;

use crate::audiounit::AudioUnit;
use crate::net::{Net, NodeId, Source};
use tutti_types::latency::{DelayInsertion, LatencyGraph};
use tutti_types::units::Samples;

/// Marks every [`PdcDelay<CH>`], whatever its channel count.
///
/// [`DelayInsertion::clear_delays`] scans for this rather than tracking
/// inserted nodes between runs, so a graph edited by any route still analyses
/// as authored.
pub const PDC_DELAY_ID: u64 = 0x_0000_0050_4443_4445; // "PDCDE"

impl LatencyGraph for Net {
    type Node = NodeId;

    fn nodes(&self) -> impl Iterator<Item = NodeId> {
        self.ids().copied()
    }

    /// fundsp's `AudioUnit::latency` takes `&mut self`, so this clones the node
    /// to query it. Nodes are cheap to clone (they share their state), and this
    /// runs once per node per compensation — off the audio thread.
    fn latency(&self, node: NodeId) -> Samples {
        let mut probe = dyn_clone::clone_box(self.node(node));
        Samples(probe.latency().unwrap_or(0.0).round().max(0.0) as usize)
    }

    fn inputs(&self, node: NodeId) -> impl Iterator<Item = Option<NodeId>> {
        (0..self.inputs_in(node)).map(move |port| local(self.source(node, port)))
    }

    fn outputs(&self) -> impl Iterator<Item = Option<NodeId>> {
        (0..AudioUnit::outputs(self)).map(move |channel| local(self.output_source(channel)))
    }
}

impl DelayInsertion for Net {
    /// Delays are found by their [`get_id`](crate::AudioUnit::get_id) marker
    /// rather than tracked between runs, so a graph edited by any route still
    /// analyses as authored.
    ///
    /// Uses `remove_link`, not `remove`: a delay sits *between* two endpoints,
    /// so removing it must reconnect them. Plain `remove` would zero the edge
    /// instead, silently disconnecting whatever the delay was compensating.
    /// `PdcDelay<CH>` always has `CH` inputs and `CH` outputs, which is
    /// `remove_link`'s requirement.
    fn clear_delays(&mut self) {
        let doomed: Vec<NodeId> = self
            .ids()
            .filter(|&&id| self.node(id).get_id() == PDC_DELAY_ID)
            .copied()
            .collect();
        for id in doomed {
            self.remove_link(id);
        }
    }

    fn delay_input(&mut self, node: NodeId, port: usize, by: Samples) {
        let Source::Local(src, src_port) = self.source(node, port) else {
            return;
        };

        let width = Width::of_edge(self.outputs_in(src), self.inputs_in(node));
        let delay = push_delay(self, width, by);

        self.connect(src, src_port, delay, 0);
        self.set_source(node, port, Source::Local(delay, 0));
    }

    fn delay_output(&mut self, channel: usize, by: Samples) {
        let Source::Local(src, src_port) = self.output_source(channel) else {
            return;
        };

        // An output channel is a single channel by definition.
        let delay = push_delay(self, Width::Mono, by);

        self.connect(src, src_port, delay, 0);
        self.set_output_source(channel, Source::Local(delay, 0));
    }
}

/// How wide a compensation delay must be to sit on a given edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Width {
    Mono,
    Stereo,
}

impl Width {
    /// A delay spliced between two nodes can only carry as many channels as the
    /// narrower side, or the splice would be a channel-count mismatch.
    fn of_edge(source_outputs: usize, target_inputs: usize) -> Self {
        match source_outputs.min(target_inputs) {
            0 | 1 => Self::Mono,
            _ => Self::Stereo,
        }
    }
}

fn push_delay(net: &mut Net, width: Width, by: Samples) -> NodeId {
    match width {
        Width::Mono => net.add(PdcDelay::<1>::new(by)),
        Width::Stereo => net.add(PdcDelay::<2>::new(by)),
    }
}

/// Graph-internal sources carry latency; net inputs and unconnected ports don't.
#[inline]
fn local(source: Source) -> Option<NodeId> {
    match source {
        Source::Local(id, _) => Some(id),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prelude::{dc, limiter};
    use tutti_types::latency;

    fn delay_count(graph: &Net) -> usize {
        graph
            .ids()
            .filter(|&&id| graph.node(id).get_id() == PDC_DELAY_ID)
            .count()
    }

    #[test]
    fn compensation_is_opt_in() {
        // A graph with unequal paths that is merely committed stays
        // uncompensated — nothing happens without an explicit call.
        let mut graph = Net::with_backend(2);
        let a = graph.add(dc(1.0));
        let eff = graph.add(limiter(0.01, 0.01));
        let b = graph.add(dc(1.0));
        graph.connect(a, 0, eff, 0);
        graph.set_output_source(0, Source::Local(eff, 0));
        graph.set_output_source(1, Source::Local(b, 0));

        graph.commit();
        assert_eq!(delay_count(&graph), 0);

        latency::compensate(&mut graph);
        assert!(delay_count(&graph) > 0);
    }

    #[test]
    fn unequal_output_channels_get_per_channel_compensation() {
        // ch0 runs through a limiter, ch1 is a direct dc. ch1 must be delayed
        // to match ch0, and an external source feeding ch1 must pre-roll.
        let mut graph = Net::with_backend(2);
        let a = graph.add(dc(1.0));
        let eff = graph.add(limiter(0.01, 0.01));
        let b = graph.add(dc(1.0));

        graph.connect(a, 0, eff, 0);
        // pipe_output only does contiguous-from-0 wiring, so set each directly.
        graph.set_output_source(0, Source::Local(eff, 0));
        graph.set_output_source(1, Source::Local(b, 0));

        let eff_lat = graph.latency(eff);
        assert!(!eff_lat.is_zero(), "limiter must report latency");

        let compensation = latency::compensate(&mut graph);

        assert_eq!(compensation.total(), eff_lat);
        assert_eq!(compensation.channels(), &[Samples(0), eff_lat]);
        assert_eq!(compensation.for_channel(1), eff_lat);
    }

    #[test]
    fn merge_point_delay_is_spliced_onto_the_early_edge() {
        // dry ──────────────┐
        //                   ├──▶ mixer   (dry side needs delaying)
        // src ─▶ limiter ───┘
        let mut graph = Net::with_backend(2);
        let src = graph.add(dc(1.0));
        let eff = graph.add(limiter(0.01, 0.01));
        let dry = graph.add(dc(1.0));
        let mixer = graph.add(crate::prelude::pass() + crate::prelude::pass());

        graph.connect(src, 0, eff, 0);
        graph.connect(eff, 0, mixer, 0);
        graph.connect(dry, 0, mixer, 1);
        graph.pipe_output(mixer);

        let eff_lat = graph.latency(eff);
        let compensation = latency::compensate(&mut graph);

        // The whole graph is as slow as the limiter path.
        assert_eq!(compensation.total(), eff_lat);
        // The dry source now reaches the mixer through a delay, not directly.
        assert!(matches!(
            graph.source(mixer, 1),
            Source::Local(id, _) if graph.node(id).get_id() == PDC_DELAY_ID
        ));
    }

    #[test]
    fn recompensating_is_idempotent() {
        // Delays report no latency and are cleared each run, so compensating
        // an already-compensated graph must not stack delays on delays.
        let mut graph = Net::with_backend(2);
        let a = graph.add(dc(1.0));
        let eff = graph.add(limiter(0.01, 0.01));
        let b = graph.add(dc(1.0));
        graph.connect(a, 0, eff, 0);
        graph.set_output_source(0, Source::Local(eff, 0));
        graph.set_output_source(1, Source::Local(b, 0));

        let first = latency::compensate(&mut graph);
        let delays_after_first = delay_count(&graph);

        let second = latency::compensate(&mut graph);

        assert_eq!(second, first);
        assert_eq!(delay_count(&graph), delays_after_first);
    }

    #[test]
    fn zero_latency_graph_gets_no_delays() {
        let mut graph = Net::with_backend(2);
        let a = graph.add(dc(1.0));
        graph.pipe_output(a);

        let compensation = latency::compensate(&mut graph);

        assert!(compensation.is_empty());
        assert_eq!(delay_count(&graph), 0);
    }

    #[test]
    fn edge_width_follows_the_narrower_side() {
        assert_eq!(Width::of_edge(1, 2), Width::Mono);
        assert_eq!(Width::of_edge(2, 1), Width::Mono);
        assert_eq!(Width::of_edge(2, 2), Width::Stereo);
        assert_eq!(Width::of_edge(0, 2), Width::Mono);
    }
}
