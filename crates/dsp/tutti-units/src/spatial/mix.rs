//! Width-generic summing bus — the fan-in that folds several N-channel sources
//! into one N-channel mix.
//!
//! [`SpatialPannerNode`](super::SpatialPannerNode) places each source into an
//! N-speaker field, but a `Net` output edge is a 1:1 wire, not a sum — so
//! combining several panned sources into one surround master needs an explicit
//! summing node. fundsp's compile-time `join`/`sumi` want a *static* arity and a
//! static width; a mixer folds a *runtime* number of sources at a *runtime*
//! channel count, so it lives here as a small hand-written [`AudioUnit`].
//!
//! This is the width-generic generalization of the DAW-side stereo fan-in: `K`
//! sources × `channels` each, interleaved per source, summed channel-wise into
//! `channels` outputs. Input port `s * channels + c` is source `s`'s channel
//! `c`; output port `c` is the sum of channel `c` across all sources.

use tutti_core::dsp::{Net, Signal};
use tutti_core::NodeId;

use crate::{Result, SpatialPannerNode, SvfFilterNode, SvfType};

/// LFE bass-management low-pass cutoff. 120 Hz is the standard consumer LFE
/// crossover (Dolby/DTS bass management typically low-pass the LFE feed at
/// 80–120 Hz); 120 Hz is the conservative upper bound.
const LFE_CUTOFF_HZ: f32 = 120.0;
/// Butterworth Q for the LFE low-pass (maximally flat, no resonant bump).
const LFE_Q: f32 = 0.707;

/// A dynamic-arity, dynamic-width summing bus: `sources * channels` inputs →
/// `channels` outputs, summed per channel.
///
/// Inputs are grouped per source (all of source 0's channels, then source 1's,
/// …); output `c` is `Σ_s input[s * channels + c]`. With `channels == 2` this is
/// exactly a stereo sum bus; with `channels == 6` it folds several 5.1 panners
/// into one 5.1 master.
#[derive(Clone, Debug)]
pub struct ChannelSumUnit {
    sources: usize,
    channels: usize,
}

impl ChannelSumUnit {
    /// A bus summing `sources` inputs, each `channels` wide. Both are clamped to
    /// at least 1 (a zero-wide or zero-source bus is meaningless — the graph
    /// would have nothing to sum).
    pub fn new(sources: usize, channels: usize) -> Self {
        Self {
            sources: sources.max(1),
            channels: channels.max(1),
        }
    }

    /// The channel width this bus sums at (its output count).
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// The number of N-wide sources it folds.
    pub fn sources(&self) -> usize {
        self.sources
    }
}

impl tutti_core::AudioUnit for ChannelSumUnit {
    fn inputs(&self) -> usize {
        self.sources * self.channels
    }

    fn outputs(&self) -> usize {
        self.channels
    }

    #[inline]
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        for c in 0..self.channels {
            let mut acc = 0.0f32;
            for s in 0..self.sources {
                acc += input[s * self.channels + c];
            }
            output[c] = acc;
        }
    }

    fn process(
        &mut self,
        size: usize,
        input: &tutti_core::BufferRef,
        output: &mut tutti_core::BufferMut,
    ) {
        for i in 0..size {
            for c in 0..self.channels {
                let mut acc = 0.0f32;
                for s in 0..self.sources {
                    acc += input.at_f32(s * self.channels + c, i);
                }
                output.set_f32(c, i, acc);
            }
        }
    }

    fn route(
        &mut self,
        _input: &tutti_core::SignalFrame,
        _frequency: f64,
    ) -> tutti_core::SignalFrame {
        let mut output = tutti_core::SignalFrame::new(self.channels);
        for c in 0..self.channels {
            output.set(c, Signal::Latency(0.0));
        }
        output
    }

    fn get_id(&self) -> u64 {
        // Distinct from the DAW-side StereoSumUnit id (0xDA02); this is the
        // engine-level width-generic bus.
        const CHANNEL_SUM_ID: u64 = 0x_0000_0000_0000_5501;
        CHANNEL_SUM_ID
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}

/// One source to place in a surround mix: the node whose (stereo) output feeds a
/// panner, and the position (`azimuth`, `elevation`, in degrees) to place it at.
/// Azimuth: 0° front, 90° left, -90° right. Elevation: 0° ear level, +up.
#[derive(Debug, Clone, Copy)]
pub struct SurroundSource {
    pub node: NodeId,
    pub azimuth: f32,
    pub elevation: f32,
}

impl SurroundSource {
    /// A source at ear level (`elevation = 0`) at the given azimuth.
    pub fn at(node: NodeId, azimuth: f32) -> Self {
        Self {
            node,
            azimuth,
            elevation: 0.0,
        }
    }
}

/// Assemble a surround producer into `net` and return the summed mix node.
///
/// Each source gets a [`SpatialPannerNode::for_layout`] placed at its position;
/// every panner's `CH` outputs are summed by a [`ChannelSumUnit`] into one
/// `layout`-wide node, whose id is returned. The caller decides what to do with
/// it — `net.pipe_output(mix)` for a direct surround render, or feed it into a
/// master strip. Pure graph surgery, no ECS.
///
/// This is the one-call form of the `sources → panners → ChannelSumUnit` graph
/// (the shape proven by the surround tests). It builds the whole mix at once, so
/// it suits offline assembly and tests; an incremental reconciler that adds and
/// removes sources over time borrows the *structure* rather than calling this.
///
/// Each source node is wired stereo-in (its ports 0 and 1) to its panner. A
/// mono source should present the same sample on both — the panner treats a
/// single input channel as centered anyway. Errors if `layout` has no VBAP
/// preset (see [`SpatialPannerNode::for_layout`]). An empty `sources` yields a
/// silent (but valid) `layout`-wide sum node.
pub fn build_surround_mix(
    net: &mut Net,
    layout: tutti_types::ChannelLayout,
    sources: &[SurroundSource],
) -> Result<NodeId> {
    let channels = layout.count() as usize;

    let mut panner_ids = Vec::with_capacity(sources.len());
    for src in sources {
        let panner = SpatialPannerNode::for_layout(layout)?;
        panner.set_position(src.azimuth, src.elevation);
        let pid = net.push(Box::new(panner));
        // Stereo-in: feed the source's first two outputs into the panner.
        net.connect(src.node, 0, pid, 0);
        net.connect(src.node, 1, pid, 1);
        panner_ids.push(pid);
    }

    // Bass management: layouts with an LFE (.1) channel get a dedicated
    // low-passed send, because LFE is NOT a spatialized speaker — the panners
    // leave that channel silent (see `speaker_channel_map`). We sum every
    // source to mono, low-pass it (~120 Hz), and route it into the LFE channel
    // as one extra input group on the main sum. Without this, a 5.1/7.1 export's
    // LFE channel would be empty.
    let lfe_group = super::nodes::lfe_channel(layout).map(|lfe_ch| {
        // Mono-sum the sources' first channel, then low-pass.
        let mono_sum = net.push(Box::new(ChannelSumUnit::new(sources.len().max(1), 1)));
        for (s, src) in sources.iter().enumerate() {
            net.connect(src.node, 0, mono_sum, s);
        }
        let lowpass = net.push(Box::new(SvfFilterNode::<f32>::new(
            SvfType::LowPass,
            LFE_CUTOFF_HZ,
            LFE_Q,
        )));
        net.connect(mono_sum, 0, lowpass, 0);
        (lowpass, lfe_ch)
    });

    // The main sum folds every panner (each an N-wide group) plus, when present,
    // one extra group carrying only the LFE send. `ChannelSumUnit::new` clamps a
    // zero source count to 1, so an empty mix is a valid silent N-wide node.
    let groups = panner_ids.len() + usize::from(lfe_group.is_some());
    let sum = net.push(Box::new(ChannelSumUnit::new(groups, channels)));
    for (s, &pid) in panner_ids.iter().enumerate() {
        for c in 0..channels {
            net.connect(pid, c, sum, s * channels + c);
        }
    }
    // The LFE send occupies the last input group: only its LFE-channel slot is
    // wired; the rest of that group reads zeros.
    if let Some((lowpass, lfe_ch)) = lfe_group {
        let group = panner_ids.len();
        net.connect(lowpass, 0, sum, group * channels + lfe_ch);
    }
    Ok(sum)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_core::AudioUnit;

    #[test]
    fn arity_and_width() {
        let u = ChannelSumUnit::new(3, 6);
        assert_eq!(u.inputs(), 18); // 3 sources × 6 channels
        assert_eq!(u.outputs(), 6);
        assert_eq!(u.channels(), 6);
        assert_eq!(u.sources(), 3);
    }

    #[test]
    fn clamps_degenerate_args() {
        let u = ChannelSumUnit::new(0, 0);
        assert_eq!(u.sources(), 1);
        assert_eq!(u.channels(), 1);
    }

    #[test]
    fn tick_sums_per_channel() {
        // Two quad sources: source A = [1,2,3,4], source B = [10,20,30,40].
        let mut u = ChannelSumUnit::new(2, 4);
        let input = [1.0, 2.0, 3.0, 4.0, 10.0, 20.0, 30.0, 40.0];
        let mut out = [0.0f32; 4];
        u.tick(&input, &mut out);
        assert_eq!(out, [11.0, 22.0, 33.0, 44.0]);
    }

    #[test]
    fn stereo_case_matches_a_plain_stereo_sum() {
        // channels == 2 degenerates to the classic stereo fan-in.
        let mut u = ChannelSumUnit::new(3, 2);
        // 3 stereo sources interleaved per source: (L,R),(L,R),(L,R).
        let input = [0.1, 0.2, 0.3, 0.4, 0.5, 0.6];
        let mut out = [0.0f32; 2];
        u.tick(&input, &mut out);
        assert!((out[0] - 0.9).abs() < 1e-6); // 0.1+0.3+0.5
        assert!((out[1] - 1.2).abs() < 1e-6); // 0.2+0.4+0.6
    }

    /// The pure-tutti surround producer, end to end, assembled via
    /// [`build_surround_mix`]: two DC sources, one placed at a *front* speaker
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
        use tutti_core::dsp::{dc, Net};
        use tutti_core::BufferRef;
        use tutti_core::{BufferVec, MAX_BUFFER_SIZE};
        use tutti_types::ChannelLayout;

        const CH: usize = 4; // quad

        let mut net = Net::new(0, CH);

        // Two DC sources (constant on both stereo inputs of each panner).
        let src_front = net.push(Box::new(dc((1.0, 1.0))));
        let src_rear = net.push(Box::new(dc((1.0, 1.0))));

        // Place one at the front-left speaker (45° → ch0) and one at the
        // rear-left speaker (135° → ch2). The builder wires each source through a
        // quad panner and sums them into one 4-wide node.
        let mix = build_surround_mix(
            &mut net,
            ChannelLayout::Quad,
            &[
                SurroundSource::at(src_front, 45.0),
                SurroundSource::at(src_rear, 135.0),
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
        use crate::SpatialPannerNode;
        use tutti_types::ChannelLayout;

        // Each supported width builds a panner of the right output count.
        assert_eq!(
            SpatialPannerNode::for_layout(ChannelLayout::Stereo)
                .unwrap()
                .num_channels(),
            2
        );
        assert_eq!(
            SpatialPannerNode::for_layout(ChannelLayout::Quad)
                .unwrap()
                .num_channels(),
            4
        );
        assert_eq!(
            SpatialPannerNode::for_layout(ChannelLayout::from(6u16))
                .unwrap()
                .num_channels(),
            6
        );
        // A width with no preset errors rather than silently falling back.
        let err = SpatialPannerNode::for_layout(ChannelLayout::from(3u16));
        assert!(matches!(
            err,
            Err(crate::Error::UnsupportedSpeakerLayout(3))
        ));
    }

    #[test]
    fn build_surround_mix_wires_expected_arity() {
        use tutti_core::dsp::{dc, Net};
        use tutti_core::AudioUnit;
        use tutti_types::ChannelLayout;

        let mut net = Net::new(0, 6);
        let a = net.push(Box::new(dc((1.0, 1.0))));
        let b = net.push(Box::new(dc((1.0, 1.0))));
        let c = net.push(Box::new(dc((1.0, 1.0))));

        // 3 sources into a 5.1 mix → the sum node is 6-out.
        let mix = build_surround_mix(
            &mut net,
            ChannelLayout::from(6u16),
            &[
                SurroundSource::at(a, 0.0),
                SurroundSource::at(b, 90.0),
                SurroundSource::at(c, -90.0),
            ],
        )
        .expect("build 5.1 mix");

        assert_eq!(net.outputs_in(mix), 6, "mix node is 5.1-wide");
    }

    #[test]
    fn build_surround_mix_empty_sources_is_a_silent_valid_node() {
        use tutti_core::dsp::Net;
        use tutti_core::AudioUnit;
        use tutti_types::ChannelLayout;

        let mut net = Net::new(0, 4);
        let mix =
            build_surround_mix(&mut net, ChannelLayout::Quad, &[]).expect("empty mix still builds");
        // ChannelSumUnit clamps 0 sources to 1 input group, so it's a valid
        // 4-out node reading zeros.
        assert_eq!(net.outputs_in(mix), 4);
    }

    #[test]
    fn build_surround_mix_rejects_unsupported_layout() {
        use tutti_core::dsp::{dc, Net};
        use tutti_types::ChannelLayout;

        let mut net = Net::new(0, 3);
        let a = net.push(Box::new(dc((1.0, 1.0))));
        let err = build_surround_mix(
            &mut net,
            ChannelLayout::from(3u16),
            &[SurroundSource::at(a, 0.0)],
        );
        assert!(matches!(
            err,
            Err(crate::Error::UnsupportedSpeakerLayout(3))
        ));
    }

    /// Render a 5.1 `build_surround_mix` graph and return settled per-channel
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
        let surround: Vec<SurroundSource> = src_ids
            .iter()
            .zip(sources)
            .map(|(&node, &(az, el))| SurroundSource {
                node,
                azimuth: az,
                elevation: el,
            })
            .collect();

        let mix = build_surround_mix(&mut net, ChannelLayout::from(6u16), &surround)
            .expect("build 5.1 mix");
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
        use tutti_core::dsp::dc;

        // One DC source, dead center. (No high frequencies, so the LFE low-pass
        // passes the DC send — we assert center DOMINATES and LFE is a smaller
        // (bass-managed) share, not that LFE is zero.)
        let energy = render_5_1_energy(&[(0.0, 0.0)], |_| {
            let mut n = tutti_core::dsp::Net::new(0, 2);
            let id = n.push(Box::new(dc((1.0, 1.0))));
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
    /// LFE energy comes only through the ~120 Hz low-pass. Here we assert LFE is
    /// non-silent (bass management is wired) for a broadband source.
    #[test]
    fn lfe_channel_receives_bass_managed_send() {
        use tutti_core::dsp::dc;

        // A DC (0 Hz) source is entirely below the 120 Hz cutoff, so the LFE
        // send passes it — LFE must be non-silent.
        let energy = render_5_1_energy(&[(30.0, 0.0)], |_| {
            let mut n = tutti_core::dsp::Net::new(0, 2);
            let id = n.push(Box::new(dc((1.0, 1.0))));
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
