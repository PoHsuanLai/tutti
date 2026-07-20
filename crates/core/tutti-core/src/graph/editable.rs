//! `AudioGraph` — the editable DSP graph.
//!
//! Owns the fundsp-backed [`GraphNet`], the [`PdcManager`] that tracks plugin
//! delay compensation, and (with feature `midi`) the [`MidiRoutingTable`] that
//! publishes hardware-MIDI → node routing snapshots. Edits take `&mut self`
//! directly. No `Mutex`, no closure, no `Arc<TuttiEngine>`.
//!
//! # Edit and commit
//!
//! Graph edits stage changes on the frontend `GraphNet`. Call [`commit`] once
//! after a batch of edits to publish them to the audio thread:
//!
//! ```ignore
//! let id = graph.add(sine_hz(440.0));
//! graph.pipe_output(id);
//! graph.commit();
//! ```
//!
//! Panic safety: if anything between the edit and `commit()` panics, the
//! audio thread keeps playing the last committed graph. The next successful
//! `commit()` flushes whatever is staged.
//!
//! # PDC
//!
//! [`PdcManager`] is fully private to the graph. Readers subscribe via
//! [`pdc_snapshot`](AudioGraph::pdc_snapshot), which hands back an
//! [`Arc`]`<`[`ArcSwap`]`<`[`PdcState`](crate::PdcState)`>>` — the only
//! channel through which PDC state escapes. Typical consumer is the sampler's
//! butler thread.

use arc_swap::ArcSwap;
use std::sync::Arc;

use crate::dsp::AudioUnit;
use crate::{
    dsp::{Fade, Net, NodeId, Source},
    PdcManager, PdcState, GraphNet,
};

use tutti_midi_types::MidiRoutingTable;

/// The editable DSP graph.
///
/// Owned by a single `&mut` thread; no locks. Wraps a [`GraphNet`](crate::GraphNet)
/// and its associated [`PdcManager`](crate::PdcManager) (plus, under the
/// `midi` feature, a [`MidiRoutingTable`]). Edits are staged until
/// [`commit`](Self::commit) publishes them to the audio thread.
pub struct AudioGraph {
    net: GraphNet,
    pdc: PdcManager,
    midi_route: MidiRoutingTable,
    sample_rate: f64,
    channels: usize,
}

impl AudioGraph {
    /// Construct from pre-built parts. Called by `TuttiEngineBuilder`.
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        net: GraphNet,
        pdc: PdcManager,
        midi_route: MidiRoutingTable,
        sample_rate: f64,
        channels: usize,
    ) -> Self {
        Self {
            net,
            pdc,
            midi_route,
            sample_rate,
            channels,
        }
    }

    /// Build an empty graph with the given output `channels` (0 inputs,
    /// 48 kHz). Constructs the net / PDC / routing table internally.
    pub fn empty(channels: usize) -> Self {
        let mut net = GraphNet::new(0, channels);
        // Allocate the fundsp realtime backend (as the real builder does) so
        // `commit()` has a backend to publish into. Discarded here — nothing
        // drives audio through a test/bootstrap graph.
        let _backend = net.backend();
        let pdc = PdcManager::new(channels, 0);
        Self {
            net,
            pdc,
            midi_route: MidiRoutingTable::new(),
            sample_rate: 48_000.0,
            channels,
        }
    }

    /// Sample rate the graph was built with.
    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    /// Output channel count the graph was built with.
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Whether `id` refers to a node currently in the graph.
    pub fn contains(&self, id: NodeId) -> bool {
        self.net.inner().contains(id)
    }

    /// Number of nodes in the graph.
    pub fn len(&self) -> usize {
        self.net.inner().size()
    }

    /// Whether the graph has no nodes.
    pub fn is_empty(&self) -> bool {
        self.net.inner().size() == 0
    }

    /// Iterate over every node id in the graph.
    ///
    /// Order is unspecified. Combine with [`inputs`](Self::inputs),
    /// [`outputs`](Self::outputs), and [`source`](Self::source) to walk the
    /// graph for debug UIs, test assertions, or custom export pipelines.
    pub fn ids(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.net.inner().ids().copied()
    }

    /// Number of input ports on `id`.
    ///
    /// Panics if `id` is not in the graph.
    pub fn inputs(&self, id: NodeId) -> usize {
        self.net.inner().inputs_in(id)
    }

    /// Number of output ports on `id`.
    ///
    /// Panics if `id` is not in the graph.
    pub fn outputs(&self, id: NodeId) -> usize {
        self.net.inner().outputs_in(id)
    }

    /// What feeds `channel` of the global output bus.
    pub fn output_source(&self, channel: usize) -> Source {
        self.net.inner().output_source(channel)
    }

    /// Typed read-access to a node.
    ///
    /// Returns `None` if `id` is not in the graph or does not refer to a `T`.
    pub fn node<T: AudioUnit + 'static>(&self, id: NodeId) -> Option<&T> {
        self.net.downcast::<T>(id)
    }

    /// Typed mutable access to a node.
    ///
    /// Returns `None` if `id` is not in the graph or does not refer to a `T`.
    pub fn node_mut<T: AudioUnit + 'static>(&mut self, id: NodeId) -> Option<&mut T> {
        self.net.downcast_mut::<T>(id)
    }

    /// Lock-free PDC snapshot subscription.
    ///
    /// The returned [`Arc`]`<`[`ArcSwap`]`<`[`PdcState`](crate::PdcState)`>>`
    /// is cheap to clone and safe to share with RT readers (e.g. the sampler
    /// butler thread). Each reader calls `.load()` whenever it needs a current
    /// snapshot. Snapshots are republished by [`commit`](Self::commit).
    pub fn pdc_snapshot(&self) -> Arc<ArcSwap<PdcState>> {
        self.pdc.snapshot_arc()
    }

    /// Snapshot the fundsp [`Net`] for offline processing (e.g. export).
    pub fn clone_net(&self) -> Net {
        self.net.inner().clone()
    }

    /// Monotonic revision of the staged graph — bumped on every edit + commit.
    /// A stable revision means identical graph topology + params, so anything
    /// derived from the rendered audio (e.g. a spectral analysis cache) can key
    /// on it for invalidation.
    pub fn revision(&self) -> u64 {
        self.net.inner().revision()
    }

    /// Snapshot the net with its global output bus repointed at `target`, so an
    /// offline render captures `target`'s output post-everything-upstream.
    ///
    /// See [`isolate_output`] for the isolation semantics. Returns `None` if
    /// `target` produces no output channels.
    pub fn clone_net_isolated(&self, target: NodeId) -> Option<Net> {
        let mut net = self.clone_net();
        isolate_output(&mut net, target).then_some(net)
    }

    /// Add a unit and return its id.
    ///
    /// Accepts any `U: AudioUnit + 'static`. For units that arrive already
    /// boxed (e.g. from plugin loaders) use [`add_boxed`](Self::add_boxed).
    pub fn add<U: AudioUnit + 'static>(&mut self, unit: U) -> NodeId {
        self.net.inner_mut().push(Box::new(unit))
    }

    /// Add an already-boxed unit and return its id.
    ///
    /// Companion to [`add`](Self::add) for callers holding a
    /// `Box<dyn AudioUnit>` (typically from plugin loaders).
    pub fn add_boxed(&mut self, unit: Box<dyn AudioUnit>) -> NodeId {
        self.net.inner_mut().push(unit)
    }

    /// Remove a node and return its unit.
    ///
    /// Panics if `id` is not in the graph.
    pub fn remove(&mut self, id: NodeId) -> Box<dyn AudioUnit> {
        self.net.inner_mut().remove(id)
    }

    /// Replace a node's unit in-place, preserving connections, and return the
    /// old unit.
    ///
    /// The replacement must have the same input and output port arities as
    /// the unit it replaces. For click-free swaps during playback use
    /// [`crossfade`](Self::crossfade); for already-boxed replacements use
    /// [`replace_boxed`](Self::replace_boxed).
    pub fn replace<U: AudioUnit + 'static>(&mut self, id: NodeId, unit: U) -> Box<dyn AudioUnit> {
        self.net.inner_mut().replace(id, Box::new(unit))
    }

    /// Boxed-input variant of [`replace`](Self::replace).
    pub fn replace_boxed(&mut self, id: NodeId, unit: Box<dyn AudioUnit>) -> Box<dyn AudioUnit> {
        self.net.inner_mut().replace(id, unit)
    }

    /// Click-free equivalent of [`replace`](Self::replace).
    ///
    /// Smoothly swaps the unit at `id` over `fade_time` seconds using the
    /// chosen [`Fade`] shape. Same port-arity requirement as `replace`. For
    /// already-boxed replacements use
    /// [`crossfade_boxed`](Self::crossfade_boxed).
    pub fn crossfade<U: AudioUnit + 'static>(
        &mut self,
        id: NodeId,
        fade: Fade,
        fade_time: f32,
        unit: U,
    ) {
        self.net
            .inner_mut()
            .crossfade(id, fade, fade_time, Box::new(unit))
    }

    /// Boxed-input variant of [`crossfade`](Self::crossfade).
    pub fn crossfade_boxed(
        &mut self,
        id: NodeId,
        fade: Fade,
        fade_time: f32,
        unit: Box<dyn AudioUnit>,
    ) {
        self.net.inner_mut().crossfade(id, fade, fade_time, unit)
    }

    /// Connect one output port of `src` to one input port of `dst`.
    pub fn connect(&mut self, src: NodeId, src_port: usize, dst: NodeId, dst_port: usize) {
        self.net.inner_mut().connect(src, src_port, dst, dst_port)
    }

    /// Connect all of `src`'s outputs to `dst`'s inputs, pairwise.
    pub fn pipe_all(&mut self, src: NodeId, dst: NodeId) {
        self.net.inner_mut().pipe_all(src, dst)
    }

    /// Pipe `src`'s outputs directly to the network's global outputs.
    pub fn pipe_output(&mut self, src: NodeId) {
        self.net.inner_mut().pipe_output(src)
    }

    /// Add `unit` and wire all of its outputs to the global output bus in a
    /// single call, returning its id.
    pub fn master<U: AudioUnit + 'static>(&mut self, unit: U) -> NodeId {
        let id = self.add(unit);
        self.pipe_output(id);
        id
    }

    /// Boxed-input variant of [`master`](Self::master).
    pub fn master_boxed(&mut self, unit: Box<dyn AudioUnit>) -> NodeId {
        let id = self.add_boxed(unit);
        self.pipe_output(id);
        id
    }

    /// Disconnect one input port of `node`, replacing it with zero input.
    pub fn disconnect(&mut self, node: NodeId, port: usize) {
        self.net.inner_mut().disconnect(node, port)
    }

    /// Read the [`Source`] feeding `node`'s input `port`.
    ///
    /// "Source" is the fundsp description of what drives that input — another
    /// node's output, a global input, or zero.
    pub fn source(&self, node: NodeId, port: usize) -> Source {
        self.net.inner().source(node, port)
    }

    /// Rewrite the [`Source`] feeding `node`'s input `port`.
    ///
    /// Counterpart to [`source`](Self::source); use this for arbitrary input
    /// rewiring that doesn't fit [`connect`](Self::connect) /
    /// [`disconnect`](Self::disconnect).
    pub fn set_source(&mut self, node: NodeId, port: usize, src: Source) {
        self.net.inner_mut().set_source(node, port, src)
    }

    /// Mutable access to the MIDI routing table.
    ///
    /// Edits are staged alongside graph edits and only reach the audio thread
    /// after the next [`commit`](Self::commit).
    pub fn midi_route_mut(&mut self) -> &mut MidiRoutingTable {
        &mut self.midi_route
    }

    /// Publish pending edits to the audio thread.
    ///
    /// Runs PDC analysis (inserting compensation delays automatically),
    /// commits fundsp's [`Net`] backend, and publishes a fresh PDC snapshot
    /// plus (under the `midi` feature) a fresh MIDI routing snapshot. Returns
    /// total graph latency in samples (informational).
    ///
    /// If a panic occurs between edits and `commit`, the audio thread keeps
    /// running the last successfully committed graph; the next `commit` call
    /// flushes whatever is currently staged.
    pub fn commit(&mut self) -> usize {
        let outcome = self.net.commit();
        // Publish per-output-channel latency into PDC. Sampler streams
        // (and any other external PDC consumer) index into
        // `channel_latencies` by their own channel identity — see
        // `apply_pdc_updates` in the sampler butler.
        for (ch, &lat) in outcome.channel_latencies.iter().enumerate() {
            self.pdc.set_channel_latency(ch, lat);
        }
        // Return-bus PDC: `PdcManager::set_return_latency` exists but is
        // unused here — `AudioGraph` has no bus topology yet, so we can't
        // split "track" latency from "return" latency. Returns continue
        // to get zero compensation (same as prior behaviour). When bus
        // identity lands on the graph, add a parallel `set_return_latency`
        // loop here.
        self.midi_route.commit();
        outcome.total_latency
    }

    /// [`Display`](core::fmt::Display)-able Graphviz `digraph { … }` summary
    /// of the current graph.
    ///
    /// Walks [`ids`](Self::ids) + [`source`](Self::source) +
    /// [`output_source`](Self::output_source) to produce a DOT-format string
    /// suitable for piping to `dot -Tsvg` or pasting into a bug report.
    /// Zero cost unless printed; the returned wrapper borrows `self`.
    ///
    /// ```ignore
    /// println!("{}", engine.graph.dot());
    /// ```
    pub fn dot(&self) -> GraphDot<'_> {
        GraphDot { graph: self }
    }

    /// Raw read access to the fundsp [`Net`].
    ///
    /// Last-resort escape hatch; prefer the typed methods above.
    pub fn net(&self) -> &Net {
        self.net.inner()
    }

    /// Raw mutable access to the fundsp [`Net`].
    ///
    /// Last-resort escape hatch; prefer the typed methods above. Edits made
    /// here are still staged and only reach the audio thread after
    /// [`commit`](Self::commit).
    pub fn net_mut(&mut self) -> &mut Net {
        self.net.inner_mut()
    }
}

/// Repoint `net`'s stereo global output bus at `target`'s output, so a render
/// of `net` captures `target`'s signal post-everything-upstream.
///
/// A mono target fans to both output channels; a stereo (or wider) target maps
/// ports 0→L, 1→R. The upstream cone of `target` keeps ticking — fundsp orders
/// *all* vertices, not just those reachable from the output bus (running nodes
/// may have side effects), so nothing is pruned by the repoint. Sibling
/// branches that no longer feed an output are simply never read.
///
/// Returns `false` (leaving `net` untouched) if `target` has no outputs.
/// Operate on a [`clone_net`](AudioGraph::clone_net) snapshot, never the live
/// net — this rewrites the output edges.
pub fn isolate_output(net: &mut Net, target: NodeId) -> bool {
    let outs = net.outputs_in(target);
    if outs == 0 {
        return false;
    }
    let bus = net.outputs();
    for ch in 0..bus {
        // Mono target fans to every bus channel; multi-out target maps port→ch
        // and clamps any extra bus channels to the target's last port.
        let port = ch.min(outs - 1);
        net.set_output_source(ch, Source::Local(target, port));
    }
    true
}

/// Graphviz `digraph` printer for a [`AudioGraph`].
///
/// Returned by [`AudioGraph::dot`]. Implements [`Display`](core::fmt::Display);
/// call `.to_string()` or format directly with `println!("{}", graph.dot())`.
pub struct GraphDot<'a> {
    graph: &'a AudioGraph,
}

impl<'a> core::fmt::Display for GraphDot<'a> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let net = self.graph.net.inner();

        writeln!(f, "digraph tutti {{")?;
        writeln!(f, "  rankdir=LR;")?;
        writeln!(f, "  node [shape=box, fontname=\"Monospace\"];")?;
        writeln!(f, "  in   [shape=point, label=\"\"];")?;
        writeln!(f, "  out  [shape=point, label=\"\"];")?;

        for id in net.ids().copied() {
            let inputs = net.inputs_in(id);
            let outputs = net.outputs_in(id);
            // We can't recover the concrete Rust type name from a
            // `&dyn AudioUnit` without cooperation from the trait. Label
            // by id + arity; callers who care about the concrete type
            // downcast via `graph.node::<T>(id)`.
            writeln!(
                f,
                "  {} [label=\"#{} ({}->{})\"];",
                id,
                id.value(),
                inputs,
                outputs,
            )?;
        }

        for id in net.ids().copied() {
            for port in 0..net.inputs_in(id) {
                match net.source(id, port) {
                    Source::Local(src, src_port) => {
                        writeln!(f, "  {} -> {} [label=\"{}->{}\"];", src, id, src_port, port,)?
                    }
                    Source::Global(src_port) => {
                        writeln!(f, "  in -> {} [label=\"{}->{}\"];", id, src_port, port,)?
                    }
                    Source::Zero => {}
                }
            }
        }

        for port in 0..self.graph.channels {
            match net.output_source(port) {
                Source::Local(src, src_port) => {
                    writeln!(f, "  {} -> out [label=\"{}->{}\"];", src, src_port, port,)?
                }
                Source::Global(src_port) => {
                    writeln!(f, "  in -> out [label=\"{}->{}\"];", src_port, port,)?
                }
                Source::Zero => {}
            }
        }

        writeln!(f, "}}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::{dc, limiter};
    use crate::PdcManager;

    fn graph_with(channels: usize) -> AudioGraph {
        let mut net = GraphNet::new(0, channels);
        // Allocate the fundsp backend so `commit()` has something to
        // publish into. We never drive audio through it in this test —
        // we only care that the commit path runs and updates PDC state.
        let _backend = net.backend();
        let pdc = PdcManager::new(channels, 0);
        AudioGraph::from_parts(
            net,
            pdc,
            MidiRoutingTable::new(),
            48_000.0,
            channels,
        )
    }

    #[test]
    fn commit_publishes_per_channel_latency() {
        // 2-output graph: channel 0 has a limiter in front, channel 1 is
        // a direct dc. After commit, PdcState.channel_latencies should
        // reflect each channel's latency independently — previously the
        // lumped total went to channel 0 and channel 1 was ignored.
        let mut graph = graph_with(2);

        let a = graph.add(dc(1.0));
        let eff = graph.add(limiter(0.01, 0.01));
        let b = graph.add(dc(1.0));

        graph.connect(a, 0, eff, 0);
        // Wire eff → output 0 and b → output 1 individually via the inner
        // Net (pipe_output only does contiguous-from-0 wiring).
        {
            let net = graph.net_mut();
            net.set_output_source(0, Source::Local(eff, 0));
            net.set_output_source(1, Source::Local(b, 0));
        }

        let eff_lat = graph
            .net_mut()
            .node_mut(eff)
            .latency()
            .unwrap_or(0.0)
            .round() as usize;
        assert!(eff_lat > 0, "limiter must report latency");

        let total = graph.commit();
        assert_eq!(total, eff_lat);

        let snap = graph.pdc_snapshot().load_full();
        assert!(snap.channel_latencies().len() >= 2);
        assert_eq!(snap.channel_latencies()[0], eff_lat);
        assert_eq!(snap.channel_latencies()[1], 0);
    }

    #[test]
    fn isolate_output_taps_target_and_keeps_upstream_alive() {
        // Two independent sources both feed the global output. Isolating `b`
        // must yield exactly b's signal — proving (1) the output bus repoints
        // to the target, and (2) the unreferenced `a` branch does not leak in.
        let mut graph = graph_with(2);
        let a = graph.add(dc(0.25));
        let b = graph.add(dc(0.75));
        graph.net_mut().set_output_source(0, Source::Local(a, 0));
        graph.net_mut().set_output_source(1, Source::Local(a, 0));

        let mut net = graph.clone_net();
        assert!(isolate_output(&mut net, b));

        net.set_sample_rate(crate::SampleRate(48_000.0));
        net.allocate();
        let mut out = [0.0f32; 2];
        net.tick(&[], &mut out);

        assert!((out[0] - 0.75).abs() < 1e-6, "ch0 = {}", out[0]);
        assert!((out[1] - 0.75).abs() < 1e-6, "ch1 = {}", out[1]);
    }

    #[test]
    fn isolate_output_mono_target_fans_to_both_channels() {
        let mut graph = graph_with(2);
        let m = graph.add(dc(0.5)); // single-output source
        let mut net = graph.clone_net();
        assert!(isolate_output(&mut net, m));
        assert_eq!(net.outputs_in(m), 1);

        net.set_sample_rate(crate::SampleRate(48_000.0));
        net.allocate();
        let mut out = [0.0f32; 2];
        net.tick(&[], &mut out);
        assert!((out[0] - 0.5).abs() < 1e-6);
        assert!((out[1] - 0.5).abs() < 1e-6);
    }
}
