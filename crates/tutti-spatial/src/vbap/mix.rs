//! Assembling a VBAP surround mix: place each source into the speaker field
//! with a panner, then fold the panners into one N-wide master.
//!
//! [`VbapPannerNode`] does the placing. The folding
//! is [`ChannelSumNode`]'s — which lives in
//! `tutti-nodes` rather than here, because summing `K` sources of `N` channels
//! is arity arithmetic with no geometry in it, and mixers that never touch VBAP
//! need it too.

use tutti_graph::GraphBuilder;
use tutti_graph::NodeKey;
use tutti_nodes::{ChannelSumNode, SvfFilterNode, SvfType};
use tutti_types::{Azimuth, ChannelLayout, Elevation, Hz, Q};

use super::error::Result;
use super::node::VbapPannerNode;

/// LFE bass-management low-pass cutoff. 120 Hz is the standard consumer LFE
/// crossover (Dolby/DTS bass management typically low-pass the LFE feed at
/// 80–120 Hz); 120 Hz is the conservative upper bound.
const LFE_CUTOFF_HZ: Hz = Hz(120.0);
/// Butterworth Q for the LFE low-pass (maximally flat, no resonant bump).
const LFE_Q: Q = Q(0.707);

/// One source to place in a VBAP mix: the node whose (stereo) output feeds a
/// panner, and the position to place it at.
///
/// The two coordinates are different types because they behave differently: a
/// bearing wraps onto the circle, a height saturates at the poles. That is also
/// what keeps them from being passed in the wrong order.
///
/// Generic over the node handle because the mix is built in more than one
/// graph: `N` is a graph [`NodeKey`] for [`build_vbap_mix`] (the default),
/// and whatever handle another graph uses for [`VbapMixParts::new`]. The mix
/// never reads it; it is carried so a caller keeps each source's handle
/// beside its position, in the order [`VbapMixNode::Source`] indexes.
#[derive(Debug, Clone, Copy)]
pub struct VbapSource<N = NodeKey> {
    /// The node whose output ports 0 and 1 feed this source's panner. A mono
    /// source should present the same sample on both.
    pub node: N,
    /// Bearing to place the source at: 0 is front, 90 left, -90 right. Wraps.
    pub azimuth: Azimuth,
    /// Height to place the source at: 0 is ear level, positive up. Saturates
    /// at the poles.
    pub elevation: Elevation,
}

impl<N> VbapSource<N> {
    /// Creates a source at ear level ([`Elevation::LEVEL`]) at the given
    /// bearing in degrees.
    pub fn at(node: N, azimuth: impl Into<Azimuth>) -> Self {
        Self {
            node,
            azimuth: azimuth.into(),
            elevation: Elevation::LEVEL,
        }
    }
}

/// One end of a [`VbapMixEdge`]: a node of a [`VbapMixParts`] by its role, or
/// one of the caller's sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VbapMixNode {
    /// The caller's `i`th source, in the order passed to [`VbapMixParts::new`].
    /// Only ever an edge's `from`: the mix reads its sources, never feeds them.
    Source(usize),
    /// The `i`th [`VbapPannerNode`] ([`VbapMixParts::panners`]), one per
    /// source, in source order.
    Panner(usize),
    /// The LFE send's mono sum ([`VbapLfeSend::sum`]).
    LfeSum,
    /// The LFE send's low-pass ([`VbapLfeSend::lowpass`]).
    LfeLowpass,
    /// The `layout`-wide sum ([`VbapMixParts::sum`]): the mix's output.
    Sum,
}

/// One audio edge of a VBAP mix: output `from_port` of `from` feeds input
/// `to_port` of `to`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VbapMixEdge {
    /// The node whose output is the signal.
    pub from: VbapMixNode,
    /// `from`'s output port.
    pub from_port: usize,
    /// The node that reads it. Never a [`VbapMixNode::Source`].
    pub to: VbapMixNode,
    /// `to`'s input port.
    pub to_port: usize,
}

/// The bass-management send of a layout with an LFE channel: every source's
/// first channel summed to mono, then low-passed at 120 Hz.
#[derive(Debug)]
pub struct VbapLfeSend {
    /// Mono sum of every source's channel 0 ([`VbapMixNode::LfeSum`]).
    pub sum: ChannelSumNode,
    /// The 120 Hz low-pass after it ([`VbapMixNode::LfeLowpass`]).
    pub lowpass: SvfFilterNode<f32>,
    /// The `layout` channel the send lands in (3 for 5.1 and 7.1.4).
    pub channel: usize,
}

/// A VBAP mix's units, not yet in any graph, with every edge between them
/// and from the caller's sources, as data.
///
/// The graph-agnostic half of [`build_vbap_mix`], for a caller that builds
/// its graph its own way: it inserts the units and wires
/// [`edges`](Self::edges) there, resolving [`VbapMixNode::Source`]`(i)` to its
/// `i`th source. The mix's output is the [`sum`](Self::sum), `layout`-wide.
/// [`insert_into`](Self::insert_into) is that step for a [`GraphBuilder`], and
/// `build_vbap_mix` is exactly `VbapMixParts::new(..)?.insert_into(..)`.
#[derive(Debug)]
pub struct VbapMixParts {
    /// One panner per source, placed at the source's position, in source
    /// order ([`VbapMixNode::Panner`]).
    pub panners: Vec<VbapPannerNode>,
    /// The LFE send, when `layout` has an LFE channel.
    pub lfe: Option<VbapLfeSend>,
    /// Folds every panner, and the LFE send, into one `layout`-wide node
    /// ([`VbapMixNode::Sum`]).
    pub sum: ChannelSumNode,
    edges: Vec<VbapMixEdge>,
}

impl VbapMixParts {
    /// Builds a VBAP surround mix's nodes and edges without inserting them into
    /// any graph.
    ///
    /// The graph-agnostic form of [`build_vbap_mix`] (see [`VbapMixParts`]). The
    /// sources' `node` handles are not read — only their positions, and their
    /// order, which [`VbapMixNode::Source`] indexes. Allocates; call it on the
    /// control thread.
    ///
    /// # Errors
    ///
    /// Returns [`VbapError::UnsupportedSpeakerLayout`](super::VbapError::UnsupportedSpeakerLayout)
    /// if `layout` has no VBAP preset (only 2, 4, 6, 8 and 12 channels do), and
    /// [`VbapError::Geometry`](super::VbapError::Geometry) if the preset geometry is
    /// rejected.
    pub fn new<N>(
        layout: tutti_types::ChannelLayout,
        sources: &[VbapSource<N>],
    ) -> Result<VbapMixParts> {
        let channels = layout.count() as usize;
        let mut edges = Vec::new();

        let mut panners = Vec::with_capacity(sources.len());
        for (i, src) in sources.iter().enumerate() {
            let panner = VbapPannerNode::for_layout(layout)?;
            panner.controls().set_position(src.azimuth, src.elevation);
            panners.push(panner);
            // Stereo-in: feed the source's first two outputs into the panner.
            for port in 0..2 {
                edges.push(VbapMixEdge {
                    from: VbapMixNode::Source(i),
                    from_port: port,
                    to: VbapMixNode::Panner(i),
                    to_port: port,
                });
            }
        }

        // Bass management: layouts with an LFE (.1) channel get a dedicated
        // low-passed send, because LFE is NOT a spatialized speaker — the panners
        // leave that channel silent (see `speaker_channel_map`). Every source is
        // summed to mono, low-passed (~120 Hz), and routed into the LFE channel as
        // one extra input group on the main sum. Without this, a 5.1/7.1 export's
        // LFE channel would be empty.
        let lfe = crate::layout::lfe_channel(layout).map(|channel| {
            // Mono-sum the sources' first channel, then low-pass.
            for s in 0..sources.len() {
                edges.push(VbapMixEdge {
                    from: VbapMixNode::Source(s),
                    from_port: 0,
                    to: VbapMixNode::LfeSum,
                    to_port: s,
                });
            }
            edges.push(VbapMixEdge {
                from: VbapMixNode::LfeSum,
                from_port: 0,
                to: VbapMixNode::LfeLowpass,
                to_port: 0,
            });
            VbapLfeSend {
                sum: ChannelSumNode::new(sources.len().max(1), ChannelLayout::MONO),
                lowpass: SvfFilterNode::<f32>::new(SvfType::LowPass, LFE_CUTOFF_HZ, LFE_Q),
                channel,
            }
        });

        // The main sum folds every panner (each an N-wide group) plus, when present,
        // one extra group carrying only the LFE send. `ChannelSumNode::new` clamps a
        // zero source count to 1, so an empty mix is a valid silent N-wide node.
        let groups = panners.len() + usize::from(lfe.is_some());
        for s in 0..panners.len() {
            for c in 0..channels {
                edges.push(VbapMixEdge {
                    from: VbapMixNode::Panner(s),
                    from_port: c,
                    to: VbapMixNode::Sum,
                    to_port: s * channels + c,
                });
            }
        }
        // The LFE send occupies the last input group: only its LFE-channel slot is
        // wired; the rest of that group reads zeros.
        if let Some(send) = &lfe {
            edges.push(VbapMixEdge {
                from: VbapMixNode::LfeLowpass,
                from_port: 0,
                to: VbapMixNode::Sum,
                to_port: panners.len() * channels + send.channel,
            });
        }

        Ok(VbapMixParts {
            panners,
            lfe,
            // `layout`, not the degraded `channels` count: the width is already in
            // hand here, so hand the bus the declaration rather than a number it
            // has to re-interpret.
            sum: ChannelSumNode::new(groups, layout),
            edges,
        })
    }

    /// Returns every audio edge of the mix, including the ones from the caller's
    /// sources ([`VbapMixNode::Source`]) into the panners and the LFE send.
    /// A port not named here reads silence: that is the rest of the LFE
    /// send's input group on the sum.
    pub fn edges(&self) -> &[VbapMixEdge] {
        &self.edges
    }

    /// Adds every unit to `g` and wires [`edges`](Self::edges), resolving
    /// [`VbapMixNode::Source`]`(i)` to `sources[i]`. Returns the sum's key.
    ///
    /// Nodes are added in order: panners, the LFE send, the sum. The panners
    /// are graph nodes,
    /// added with their controls, which are dropped here with the low-pass's:
    /// the placement is the one [`VbapMixParts::new`] set, and the send's cutoff
    /// is fixed. A caller that moves sources later takes each panner's
    /// [`VbapPannerNode::controls`] from [`panners`](Self::panners) before
    /// inserting.
    ///
    /// # Panics
    ///
    /// Panics if `sources` is shorter than the list the parts were built from.
    pub fn insert_into(self, g: &mut GraphBuilder, sources: &[NodeKey]) -> NodeKey {
        let panners: Vec<NodeKey> = self
            .panners
            .into_iter()
            .map(|p| g.add_with_controls(p).0)
            .collect();
        let lfe = self
            .lfe
            .map(|send| (g.add(send.sum), g.add_with_controls(send.lowpass).0));
        let sum = g.add(self.sum);
        let id = |n: VbapMixNode| match n {
            VbapMixNode::Source(i) => sources[i],
            VbapMixNode::Panner(i) => panners[i],
            VbapMixNode::LfeSum => lfe.expect("an LFE edge implies the send").0,
            VbapMixNode::LfeLowpass => lfe.expect("an LFE edge implies the send").1,
            VbapMixNode::Sum => sum,
        };
        for e in &self.edges {
            g.connect(id(e.from), e.from_port, id(e.to), e.to_port);
        }
        sum
    }
}

/// Builds a VBAP surround mix into `g` and returns the summed mix node.
///
/// Each source gets a [`VbapPannerNode::for_layout`] placed at its position;
/// every panner's outputs are summed by a [`ChannelSumNode`] into one
/// `layout`-wide node, whose key is returned. The caller decides what to do
/// with it: `g.pipe_output(mix)` for a direct surround render, or feed it
/// into a master strip. For a layout with an LFE channel (5.1, 7.1, 7.1.4),
/// every source's channel 0 is also summed to mono, low-passed at 120 Hz and
/// routed into the LFE channel.
///
/// It builds the whole mix at once, so it suits offline assembly and tests. It
/// is [`VbapMixParts::new`] followed by [`VbapMixParts::insert_into`]; a graph
/// built another way uses the first half alone. The panners' controls are
/// dropped, so positions are fixed at the ones given here.
///
/// Each source node is wired stereo-in (its ports 0 and 1) to its panner. A
/// mono source should present the same sample on both. An empty `sources`
/// yields a silent (but valid) `layout`-wide sum node.
///
/// # Errors
///
/// Returns [`VbapError::UnsupportedSpeakerLayout`](super::VbapError::UnsupportedSpeakerLayout)
/// if `layout` has no VBAP preset (only 2, 4, 6, 8 and 12 channels do), and
/// [`VbapError::Geometry`](super::VbapError::Geometry) if the preset geometry is
/// rejected.
pub fn build_vbap_mix(
    g: &mut GraphBuilder,
    layout: tutti_types::ChannelLayout,
    sources: &[VbapSource],
) -> Result<NodeKey> {
    let nodes: Vec<NodeKey> = sources.iter().map(|s| s.node).collect();
    Ok(VbapMixParts::new(layout, sources)?.insert_into(g, &nodes))
}

#[cfg(test)]
mod tests;
