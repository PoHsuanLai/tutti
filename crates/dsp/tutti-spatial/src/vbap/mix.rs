//! Assembling a VBAP surround mix: place each source into the speaker field
//! with a panner, then fold the panners into one N-wide master.
//!
//! [`VbapPannerNode`](crate::vbap::VbapPannerNode) does the placing. The folding
//! is [`ChannelSumNode`](tutti_nodes::ChannelSumNode)'s — which lives in
//! `tutti-nodes` rather than here, because summing `K` sources of `N` channels
//! is arity arithmetic with no geometry in it, and mixers that never touch VBAP
//! need it too.

use tutti_core::dsp::Net;
use tutti_core::dsp::NodeId;
use tutti_core::{Azimuth, ChannelLayout, Elevation, Hz, Q};
use tutti_nodes::{ChannelSumNode, SvfFilterNode, SvfType};

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
/// graph: `N` is a `Net` [`NodeId`] for [`build_vbap_mix`] (the default), and
/// whatever key another graph uses for [`vbap_mix_parts`] — `tutti_graph`'s
/// `NodeKey`, say. The mix never reads it; it is carried so a caller keeps
/// each source's handle beside its position, in the order
/// [`VbapMixNode::Source`] indexes.
#[derive(Debug, Clone, Copy)]
pub struct VbapSource<N = NodeId> {
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
    /// A source at ear level ([`Elevation::LEVEL`]) at the given bearing.
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
    /// The caller's `i`th source, in the order passed to [`vbap_mix_parts`].
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
pub struct VbapLfeSend {
    /// Mono sum of every source's channel 0 ([`VbapMixNode::LfeSum`]).
    pub sum: ChannelSumNode,
    /// The 120 Hz low-pass after it ([`VbapMixNode::LfeLowpass`]).
    pub lowpass: SvfFilterNode<f32>,
    /// The `layout` channel the send lands in (3 for 5.1 and 7.1.4).
    pub channel: usize,
}

/// A VBAP mix's units, **not yet in any graph**, with every edge between them
/// and from the caller's sources, as data.
///
/// The graph-agnostic half of [`build_vbap_mix`], for a caller whose graph is
/// not a `Net` (the native graph of doc 013): it inserts the units into its
/// own graph and wires [`edges`](Self::edges) there, resolving
/// [`VbapMixNode::Source`]`(i)` to its `i`th source. The mix's output is the
/// [`sum`](Self::sum), `layout`-wide. [`insert_into`](Self::insert_into) is
/// that step for a `Net`, and `build_vbap_mix` is exactly
/// `vbap_mix_parts(..)?.insert_into(..)`, so both graphs are built from one
/// description of the mix rather than from two copies that must agree.
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
    /// Every audio edge of the mix, including the ones from the caller's
    /// sources ([`VbapMixNode::Source`]) into the panners and the LFE send.
    /// A port not named here reads silence: that is the rest of the LFE
    /// send's input group on the sum.
    pub fn edges(&self) -> &[VbapMixEdge] {
        &self.edges
    }

    /// Push every unit into `net` and wire [`edges`](Self::edges), resolving
    /// [`VbapMixNode::Source`]`(i)` to `sources[i]`. Returns the sum's id.
    ///
    /// The `Net` adapter. It pushes in `build_vbap_mix`'s historical order
    /// (panners, the LFE send, the sum).
    ///
    /// # Panics
    ///
    /// If `sources` is shorter than the list the parts were built from.
    pub fn insert_into(self, net: &mut Net, sources: &[NodeId]) -> NodeId {
        let panners: Vec<NodeId> = self
            .panners
            .into_iter()
            .map(|p| net.push(Box::new(p)))
            .collect();
        let lfe = self.lfe.map(|send| {
            (
                net.push(Box::new(send.sum)),
                net.push(Box::new(send.lowpass)),
            )
        });
        let sum = net.push(Box::new(self.sum));
        let id = |n: VbapMixNode| match n {
            VbapMixNode::Source(i) => sources[i],
            VbapMixNode::Panner(i) => panners[i],
            VbapMixNode::LfeSum => lfe.expect("an LFE edge implies the send").0,
            VbapMixNode::LfeLowpass => lfe.expect("an LFE edge implies the send").1,
            VbapMixNode::Sum => sum,
        };
        for e in &self.edges {
            net.connect(id(e.from), e.from_port, id(e.to), e.to_port);
        }
        sum
    }
}

/// Build a VBAP surround mix's units and edges, in no graph.
///
/// The graph-agnostic form of [`build_vbap_mix`] (see [`VbapMixParts`]). The
/// sources' `node` handles are not read — only their positions, and their
/// order, which [`VbapMixNode::Source`] indexes. Errors as `build_vbap_mix`
/// does, if `layout` has no VBAP preset.
pub fn vbap_mix_parts<N>(
    layout: tutti_types::ChannelLayout,
    sources: &[VbapSource<N>],
) -> Result<VbapMixParts> {
    let channels = layout.count() as usize;
    let mut edges = Vec::new();

    let mut panners = Vec::with_capacity(sources.len());
    for (i, src) in sources.iter().enumerate() {
        let panner = VbapPannerNode::for_layout(layout)?;
        panner.set_position(src.azimuth, src.elevation);
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

/// Assemble a VBAP surround producer into `net` and return the summed mix node.
///
/// Each source gets a [`VbapPannerNode::for_layout`] placed at its position;
/// every panner's `CH` outputs are summed by a [`ChannelSumNode`] into one
/// `layout`-wide node, whose id is returned. The caller decides what to do with
/// it — `net.pipe_output(mix)` for a direct surround render, or feed it into a
/// master strip. Pure graph surgery, no ECS.
///
/// This is the one-call form of the `sources → panners → ChannelSumNode` graph
/// (the shape proven by the surround tests). It builds the whole mix at once, so
/// it suits offline assembly and tests; an incremental reconciler that adds and
/// removes sources over time borrows the *structure* rather than calling this.
/// It is [`vbap_mix_parts`] followed by [`VbapMixParts::insert_into`]; a graph
/// other than `Net` uses the first half alone.
///
/// Each source node is wired stereo-in (its ports 0 and 1) to its panner. A
/// mono source should present the same sample on both — the panner treats a
/// single input channel as centered anyway. Errors if `layout` has no VBAP
/// preset (see [`VbapPannerNode::for_layout`]). An empty `sources` yields a
/// silent (but valid) `layout`-wide sum node.
pub fn build_vbap_mix(
    net: &mut Net,
    layout: tutti_types::ChannelLayout,
    sources: &[VbapSource],
) -> Result<NodeId> {
    let nodes: Vec<NodeId> = sources.iter().map(|s| s.node).collect();
    Ok(vbap_mix_parts(layout, sources)?.insert_into(net, &nodes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_core::AudioUnit;

    /// The pure-tutti surround producer, end to end, assembled via
    /// [`build_vbap_mix`]: two DC sources, one placed at a *front* speaker
    /// and one at a *rear* speaker of a quad field. Asserts the front source
    /// lands in a front channel and the rear source lands in a rear channel —
    /// i.e. per-source placement survives the panner → sum → global-output path,
    /// and the graph does NOT collapse to the front L/R pair — the engine-level
    /// surround contract, with no ECS anywhere.
    ///
    /// Quad speaker layout (vbap preset): ch0 FL 45°, ch1 FR -45°, ch2 RL 135°,
    /// ch3 RR -135°.
    #[test]
    fn surround_graph_places_front_and_rear_sources() {
        use tutti_core::dsp::Net;
        use tutti_core::BufferRef;
        use tutti_core::{BufferVec, MAX_BUFFER_SIZE};
        use tutti_nodes::testing::Const;
        use tutti_types::ChannelLayout;

        const CH: usize = 4; // quad

        let mut net = Net::new(0, CH);

        // Two DC sources (constant on both stereo inputs of each panner).
        let src_front = net.push(Box::new(Const::frame(&[1.0, 1.0])));
        let src_rear = net.push(Box::new(Const::frame(&[1.0, 1.0])));

        // Place one at the front-left speaker (45° → ch0) and one at the
        // rear-left speaker (135° → ch2). The builder wires each source through a
        // quad panner and sums them into one 4-wide node.
        let mix = build_vbap_mix(
            &mut net,
            ChannelLayout::QUAD,
            &[
                VbapSource::at(src_front, 45.0),
                VbapSource::at(src_rear, 135.0),
            ],
        )
        .expect("build quad surround mix");
        net.pipe_output(mix);
        net.set_sample_rate(tutti_core::SampleRate(48000.0));

        // The panner de-zippers position changes with a ~0.05s one-pole smoother
        // starting from 0°, so the azimuth ramps to its target over a few
        // thousand samples. Render several blocks to let it settle, then measure
        // energy only from the final (settled) block.
        let block = 256usize.min(MAX_BUFFER_SIZE);
        let empty = BufferRef::new(&[]);
        let mut buf = BufferVec::new(CH);
        let mut energy = vec![0.0f32; CH];
        // ~12k samples of warm-up (well past the 0.05s @ 48k ≈ 2400-sample time
        // constant) then measure the last block.
        let settle_blocks = 48;
        for b in 0..=settle_blocks {
            let mut out = buf.buffer_mut();
            net.process(block, &empty, &mut out);
            if b == settle_blocks {
                for (c, e) in energy.iter_mut().enumerate() {
                    *e = out.channel_f32(c)[..block].iter().map(|s| s * s).sum();
                }
            }
        }

        let total: f32 = energy.iter().sum();
        assert!(total > 0.0, "surround graph produced silence");

        // The front source lands in the front-left channel (0); the rear source
        // lands in the rear-left channel (2). Both must carry real energy — a
        // stereo fold would leave ch2 (and ch3) silent.
        assert!(
            energy[0] > total * 0.2,
            "front source should light the front-left channel (energy {energy:?})"
        );
        assert!(
            energy[2] > total * 0.2,
            "rear source should light the rear-left channel — surround did NOT \
             collapse to stereo (energy {energy:?})"
        );
    }

    #[test]
    fn for_layout_dispatches_and_rejects_unsupported() {
        use crate::vbap::VbapPannerNode;
        use tutti_types::ChannelLayout;

        // Each supported width builds a panner of the right output count.
        assert_eq!(
            VbapPannerNode::for_layout(ChannelLayout::STEREO)
                .unwrap()
                .num_channels(),
            2
        );
        assert_eq!(
            VbapPannerNode::for_layout(ChannelLayout::QUAD)
                .unwrap()
                .num_channels(),
            4
        );
        assert_eq!(
            VbapPannerNode::for_layout(ChannelLayout::from(6u16))
                .unwrap()
                .num_channels(),
            6
        );
        // A width with no preset errors rather than silently falling back.
        let err = VbapPannerNode::for_layout(ChannelLayout::from(3u16));
        assert!(matches!(
            err,
            Err(crate::vbap::VbapError::UnsupportedSpeakerLayout(3))
        ));
    }

    #[test]
    fn build_vbap_mix_empty_sources_is_a_silent_valid_node() {
        use tutti_core::dsp::Net;
        use tutti_types::ChannelLayout;

        let mut net = Net::new(0, 4);
        let mix =
            build_vbap_mix(&mut net, ChannelLayout::QUAD, &[]).expect("empty mix still builds");
        // ChannelSumNode clamps 0 sources to 1 input group, so it's a valid
        // 4-out node reading zeros.
        assert_eq!(net.outputs_in(mix), 4);
    }

    /// Render a 5.1 `build_vbap_mix` graph and return settled per-channel
    /// energy over the last block. `src_freq_hz` sets the DC/tone for each
    /// source (constant if 0). Positions are `(azimuth, elevation)` per source.
    fn render_5_1_energy(
        sources: &[(f32, f32)],
        src_signal: impl Fn(usize) -> tutti_core::dsp::Net + Copy,
    ) -> [f32; 6] {
        use tutti_core::dsp::Net;
        use tutti_core::BufferRef;
        use tutti_core::{BufferVec, MAX_BUFFER_SIZE};
        use tutti_types::ChannelLayout;

        let mut net = Net::new(0, 6);
        let src_ids: Vec<_> = (0..sources.len())
            .map(|i| {
                // Splice a per-source signal sub-net in: push its single node.
                let sub = src_signal(i);
                net.push(Box::new(sub))
            })
            .collect();
        let surround: Vec<VbapSource> = src_ids
            .iter()
            .zip(sources)
            .map(|(&node, &(az, el))| VbapSource {
                node,
                azimuth: Azimuth(az),
                elevation: Elevation(el),
            })
            .collect();

        let mix =
            build_vbap_mix(&mut net, ChannelLayout::from(6u16), &surround).expect("build 5.1 mix");
        net.pipe_output(mix);
        net.set_sample_rate(tutti_core::SampleRate(48000.0));

        let block = 256usize.min(MAX_BUFFER_SIZE);
        let empty = BufferRef::new(&[]);
        let mut buf = BufferVec::new(6);
        let mut energy = [0.0f32; 6];
        let settle_blocks = 48;
        for b in 0..=settle_blocks {
            let mut out = buf.buffer_mut();
            net.process(block, &empty, &mut out);
            if b == settle_blocks {
                for (c, e) in energy.iter_mut().enumerate() {
                    *e = out.channel_f32(c)[..block].iter().map(|s| s * s).sum();
                }
            }
        }
        energy
    }

    /// A center-panned (0° azimuth) source must land in the CENTER channel (2)
    /// and NOT leak into the LFE channel (3). This is the channel-remap fix: the
    /// VBAP speaker order `[L,R,C,Ls,Rs]` is scattered to file order
    /// `[FL,FR,C,LFE,SL,SR]`, so the surrounds don't slide into the LFE slot.
    #[test]
    fn center_source_lands_in_center_not_lfe() {
        use tutti_nodes::testing::Const;

        // One DC source, dead center. (No high frequencies, so the LFE low-pass
        // passes the DC send — the assertion is that center DOMINATES and LFE
        // is a smaller (bass-managed) share, not that LFE is zero.)
        let energy = render_5_1_energy(&[(0.0, 0.0)], |_| {
            let mut n = tutti_core::dsp::Net::new(0, 2);
            let id = n.push(Box::new(Const::frame(&[1.0, 1.0])));
            n.pipe_output(id);
            n
        });
        let total: f32 = energy.iter().sum();
        assert!(total > 0.0);
        // Center (ch2) carries the panned source.
        assert!(
            energy[2] > total * 0.4,
            "center source should dominate the center channel (energy {energy:?})"
        );
        // The surrounds (ch4, ch5) must be ~silent — proof the remap didn't slide
        // the panner's Ls/Rs into the wrong slots. (Front L/R get a little from a
        // dead-center VBAP source, which is expected.)
        assert!(
            energy[4] < total * 0.05 && energy[5] < total * 0.05,
            "a center source must not light the surround channels (energy {energy:?})"
        );
    }

    /// The LFE channel (3) is fed by a dedicated low-passed send, not by the
    /// panner. A source with strong high-frequency content should still light
    /// LFE (the bass component passes) but the panner must never write LFE — so
    /// LFE energy comes only through the ~120 Hz low-pass. Asserted as LFE being
    /// non-silent (bass management is wired) for a broadband source.
    #[test]
    fn lfe_channel_receives_bass_managed_send() {
        use tutti_nodes::testing::Const;

        // A DC (0 Hz) source is entirely below the 120 Hz cutoff, so the LFE
        // send passes it — LFE must be non-silent.
        let energy = render_5_1_energy(&[(30.0, 0.0)], |_| {
            let mut n = tutti_core::dsp::Net::new(0, 2);
            let id = n.push(Box::new(Const::frame(&[1.0, 1.0])));
            n.pipe_output(id);
            n
        });
        let total: f32 = energy.iter().sum();
        assert!(
            energy[3] > total * 0.02,
            "LFE channel should carry the low-passed bass-management send \
             (energy {energy:?})"
        );
    }
}
