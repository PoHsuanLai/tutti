//! `TuttiNet` — thin facade over [`fundsp::net::Net`] that adds typed downcast
//! helpers and a crate-internal commit that runs PDC analysis.
//!
//! Callers use [`TuttiNet::inner`] / [`TuttiNet::inner_ref`] to reach fundsp's
//! `Net` directly for all pure graph operations (push, connect, remove, etc.).
//! The rest of the API is intentionally minimal.

use crate::compat::{Box, Vec};
use crate::pdc;

use fundsp::net::{Net, NodeId, Source};
use fundsp::prelude::AudioUnit;
use fundsp::realnet::NetBackend;

/// Result of committing a [`TuttiNet`]. Carries both the scalar graph
/// latency and the per-output-channel arrival times so callers can
/// publish them into their PDC manager.
#[derive(Debug, Clone, Default)]
pub struct CommitOutcome {
    /// Worst-case arrival time across all outputs.
    pub total_latency: usize,
    /// Pre-compensation arrival time at each output channel's source tap.
    /// Length equals `net.outputs()`.
    pub channel_latencies: Vec<usize>,
}

pub struct TuttiNet {
    net: Net,
}

impl TuttiNet {
    /// Build a new net with the given input/output port counts.
    pub fn new(inputs: usize, outputs: usize) -> Self {
        Self {
            net: Net::new(inputs, outputs),
        }
    }

    /// Consumes a live backend that can be plugged into an audio callback
    /// processor. Can only be called once (panics on second call).
    pub fn backend(&mut self) -> NetBackend {
        self.net.backend()
    }

    /// Shared access to the underlying fundsp `Net` for queries.
    pub fn inner(&self) -> &Net {
        &self.net
    }

    /// Mutable access to the underlying fundsp `Net`.
    ///
    /// All pure graph operations (push, connect, remove, replace, set_sample_rate, ...)
    /// go through this escape hatch.
    pub fn inner_mut(&mut self) -> &mut Net {
        &mut self.net
    }

    /// Get a typed reference to a node. Returns `None` if the node is not of type `T`.
    pub fn downcast<T: AudioUnit + 'static>(&self, id: NodeId) -> Option<&T> {
        <dyn AudioUnit>::as_any(self.net.node(id)).downcast_ref::<T>()
    }

    /// Get a typed mutable reference to a node. Returns `None` if the node is not of type `T`.
    pub fn downcast_mut<T: AudioUnit + 'static>(&mut self, id: NodeId) -> Option<&mut T> {
        <dyn AudioUnit>::as_any_mut(self.net.node_mut(id)).downcast_mut::<T>()
    }

    /// Commit pending graph changes to the backend with automatic PDC.
    ///
    /// 1. Removes any previously-inserted PDC delay nodes (identified by
    ///    their stable `get_id()` markers — no state is tracked between commits).
    /// 2. Runs PDC analysis on the graph.
    /// 3. Inserts fresh compensation delays at merge points and output channels.
    /// 4. Commits the underlying fundsp `Net` so changes reach the audio thread.
    ///
    /// Returns a [`CommitOutcome`] carrying the total graph latency plus
    /// per-output-channel latencies. External PDC consumers (e.g. the
    /// sampler butler) index into `channel_latencies` by their own
    /// channel identity; callers that don't care can still read
    /// `total_latency`.
    pub fn commit(&mut self) -> CommitOutcome {
        self.remove_pdc_delays();

        let analysis = pdc::graph_compensator::analyze(&mut self.net);

        for comp in &analysis.compensations {
            if comp.delay_samples == 0 {
                continue;
            }
            self.insert_pdc_delay(comp.node_id, comp.input_port, comp.delay_samples);
        }

        for comp in &analysis.output_compensations {
            if comp.delay_samples == 0 {
                continue;
            }
            self.insert_output_pdc_delay(comp.output_channel, comp.delay_samples);
        }

        self.net.commit();
        CommitOutcome {
            total_latency: analysis.total_latency,
            channel_latencies: analysis.channel_latencies,
        }
    }

    /// Remove all auto-inserted PDC delay nodes from the graph.
    ///
    /// Identified by their `AudioUnit::get_id()` markers (`PDC_DELAY_ID` and
    /// `MONO_PDC_DELAY_ID`). Scanning is O(n) but avoids carrying stale state
    /// between commits — if the user manually removed a node via `inner()`,
    /// the next commit's analysis still sees the correct graph.
    fn remove_pdc_delays(&mut self) {
        use crate::node_id::{MONO_PDC_DELAY_ID, PDC_DELAY_ID};
        let doomed: Vec<NodeId> = self
            .net
            .ids()
            .filter(|&&id| {
                let gid = self.net.node(id).get_id();
                gid == PDC_DELAY_ID || gid == MONO_PDC_DELAY_ID
            })
            .copied()
            .collect();
        for id in doomed {
            self.net.remove(id);
        }
    }

    fn insert_pdc_delay(&mut self, target: NodeId, input_port: usize, delay_samples: usize) {
        let source = self.net.source(target, input_port);
        if let Source::Local(src_id, src_port) = source {
            let src_outputs = self.net.outputs_in(src_id);
            let target_inputs = self.net.inputs_in(target);

            let pdc_node = if src_outputs == 1 || target_inputs == 1 {
                self.net
                    .push(Box::new(pdc::MonoPdcDelayUnit::new(delay_samples)))
            } else {
                self.net
                    .push(Box::new(pdc::PdcDelayUnit::new(delay_samples)))
            };

            self.net.connect(src_id, src_port, pdc_node, 0);
            self.net
                .set_source(target, input_port, Source::Local(pdc_node, 0));
        }
    }

    fn insert_output_pdc_delay(&mut self, output_channel: usize, delay_samples: usize) {
        let source = self.net.output_source(output_channel);
        if let Source::Local(src_id, src_port) = source {
            let pdc_node = self
                .net
                .push(Box::new(pdc::MonoPdcDelayUnit::new(delay_samples)));

            self.net.connect(src_id, src_port, pdc_node, 0);
            self.net
                .set_output_source(output_channel, Source::Local(pdc_node, 0));
        }
    }
}
