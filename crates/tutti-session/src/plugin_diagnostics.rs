//! A hosted plugin's diagnostics and its buses (design 029 sections 2.7 and
//! 3.5): the counts and figures a host shows beside a plugin, and the graph's
//! bus handles for wiring it.
//!
//! Neither depends on where the plugin runs: a figure that exists only out of
//! process (a server's pid) is `None`, a count that cannot happen in process
//! (a pipeline's missed chunks) is zero, and everything else reads the same.

use tutti_graph::{BusRef, BusSource, Losses, MAX_BUSES};
use tutti_plugin_types::{BlockCounts, BusRole, PluginBus, TerminationReason};
use tutti_types::{Samples, Tail};

use crate::plugin::HostedPlugin;

/// What the graph holds of a plugin's node: the figures a view is made with.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Figures {
    pub(crate) latency: Samples,
    pub(crate) tail: Tail,
}

/// A hosted plugin's counts and figures, lent by
/// [`Plugin::diagnostics`](crate::Plugin::diagnostics).
///
/// What a host shows in a plugin's corner (its server, its dropouts, its
/// latency), kept off the main surface. The losses are
/// [`Losses`], counted in events as the MIDI inlets count theirs.
#[derive(Debug, Clone, Copy)]
pub struct Diagnostics<'a> {
    hosted: &'a HostedPlugin,
    figures: Figures,
}

impl<'a> Diagnostics<'a> {
    pub(crate) fn new(hosted: &'a HostedPlugin, figures: Figures) -> Self {
        Self { hosted, figures }
    }

    /// The OS process id of the `plugin-server` hosting the plugin, or `None`
    /// for a plugin that runs in this process.
    pub fn server_pid(&self) -> Option<u32> {
        self.hosted.handle.server_pid()
    }

    /// The blocks the audio path gave up waiting for, and the replies it took,
    /// over the plugin's life. A plugin that runs in this process has no
    /// replies to wait for: both are zero.
    pub fn block_counts(&self) -> BlockCounts {
        self.hosted.handle.block_counts()
    }

    /// The chunks the plugin's pipeline had not answered by the time the node
    /// played them (each played as silence: a dropout), since the plugin was
    /// bound. Zero for a plugin that runs in this process, which has no
    /// pipeline to be late.
    pub fn missed_chunks(&self) -> u64 {
        self.hosted
            .controls
            .as_ref()
            .map_or(0, tutti_plugin_host::PluginControls::missed_chunks)
    }

    /// The MIDI events on the node's event input that never reached the plugin,
    /// since it was bound: past the capacity of a block, in a block the plugin
    /// did not run, or the rest of a SysEx message that lost a packet. All of
    /// them are [`lost`](Losses::lost); `refused` stays zero (the plugin node
    /// has one event input, so no event is addressed to a port it lacks).
    pub fn midi_in(&self) -> Losses {
        Losses::lost(
            self.hosted
                .node
                .as_ref()
                .map_or(0, tutti_plugin_host::NodeControls::dropped_midi_in),
        )
    }

    /// The parameter points the host sent that the node did not deliver as
    /// they were: [`lost`](Losses::lost) are grid points a full queue dropped,
    /// [`evicted`](Losses::evicted) are points that replaced the last one of a
    /// full queue, and [`refused`](Losses::refused) are points for a parameter
    /// the node has no slot for.
    pub fn parameter_points(&self) -> Losses {
        let stats = self
            .hosted
            .node
            .as_ref()
            .map(tutti_plugin_host::NodeControls::param_delivery)
            .unwrap_or_default();
        Losses::lost(stats.grid_points_dropped)
            + Losses::evicted(stats.points_merged)
            + Losses::refused(stats.points_dropped)
    }

    /// The parameter edits the plugin made that the host did not hear, over the
    /// plugin's life, all [`lost`](Losses::lost). A lost edit costs a stale
    /// readout, not audio, and the session re-reads the parameters after one.
    pub fn parameter_edits(&self) -> Losses {
        Losses::lost(self.hosted.handle.dropped_parameter_events())
    }

    /// The latency the graph compensates for this plugin's node, as of the last
    /// update: the plugin's own, and out of process one chunk of the pipeline
    /// that carries its audio.
    pub fn latency(&self) -> Samples {
        self.figures.latency
    }

    /// The tail the graph holds for the node, as of the last update: what the
    /// plugin reports, which may be unbounded or not yet reported.
    pub fn tail(&self) -> Tail {
        self.figures.tail
    }

    /// Whether the node follows the project's meter (the time signature and bar
    /// it tells the plugin). True for every plugin the session added; false for
    /// one a host inserted itself
    /// ([`Graph::host_plugin`](crate::Graph::host_plugin)), whose node the
    /// session did not give a meter.
    pub fn follows_project_meter(&self) -> bool {
        self.hosted
            .node
            .as_ref()
            .is_some_and(tutti_plugin_host::NodeControls::has_meter)
    }

    /// Ends the plugin's server process: the death follows as it would for any
    /// crash ([`PluginHealth::Died`](crate::PluginHealth::Died), with
    /// `reason` as its cause), and recovery, if on, reloads it. Nothing for a
    /// plugin that runs in this process. Not for the audio thread.
    pub fn terminate(&self, reason: TerminationReason) {
        self.hosted.handle.terminate(reason);
    }
}

/// A plugin's buses as the graph's bus handles, lent by
/// [`Plugin::buses`](crate::Plugin::buses): what to wire with
/// `Graph::connect` ([`BusSource`] into [`BusRef`]).
///
/// `connect(kick.out(0), comp.buses().sidechain().unwrap())` is the plugin's
/// sidechain. A bus index here is the graph's bus index, the plugin's own bus
/// order, so each handle is the one `key.bus_in(i)` and `key.bus_out(i)` name.
/// The graph tracks [`MAX_BUSES`] of a side: a plugin with more has its later
/// buses reached by port.
#[derive(Debug, Clone, Copy)]
pub struct PluginBuses<'a> {
    node: tutti_graph::NodeKey,
    inputs: &'a [PluginBus],
    outputs: &'a [PluginBus],
}

impl<'a> PluginBuses<'a> {
    pub(crate) fn new(node: tutti_graph::NodeKey, hosted: &'a HostedPlugin) -> Self {
        Self {
            node,
            inputs: hosted.handle.input_buses(),
            outputs: hosted.handle.output_buses(),
        }
    }

    /// Bus `index` of this node (below [`MAX_BUSES`], so it fits the `u8`).
    fn bus(&self, index: usize) -> BusRef {
        BusRef::new(self.node, index as u8)
    }

    /// The main input bus, if the plugin has an input.
    pub fn main_input(&self) -> Option<BusRef> {
        let index = self
            .inputs
            .iter()
            .position(|bus| bus.role == BusRole::Main)?;
        self.input(index)
    }

    /// The plugin's sidechain: its first input bus past the main one.
    ///
    /// No format marks a bus a sidechain (they say "aux"), and an input that is
    /// not the main one is what a sidechain feeds, so this is the first of
    /// those; `None` for a plugin with no such bus.
    pub fn sidechain(&self) -> Option<BusRef> {
        let index = self
            .inputs
            .iter()
            .position(|bus| bus.role != BusRole::Main)?;
        self.input(index)
    }

    /// Input bus `index`, in the plugin's bus order.
    pub fn input(&self, index: usize) -> Option<BusRef> {
        (index < self.inputs.len().min(MAX_BUSES)).then(|| self.bus(index))
    }

    /// Output bus `index`, in the plugin's bus order: the main bus is the one
    /// the plugin marks main, and the rest are its extra outputs.
    pub fn output(&self, index: usize) -> Option<BusSource> {
        (index < self.outputs.len().min(MAX_BUSES)).then(|| BusSource::Node(self.bus(index)))
    }

    /// How many input buses the graph tracks.
    pub fn input_count(&self) -> usize {
        self.inputs.len().min(MAX_BUSES)
    }

    /// How many output buses the graph tracks.
    pub fn output_count(&self) -> usize {
        self.outputs.len().min(MAX_BUSES)
    }
}
