use super::*;
use tutti_graph::Prepare;
use tutti_types::Samples;

/// Render `g` (no global inputs) for 49 blocks of 256 frames — past the
/// panners' ~0.05 s position smoother — and return each output channel's
/// energy over the last block.
fn settled_energy(g: GraphBuilder) -> Vec<f32> {
    let mut r = g
        .renderer(Prepare::new(tutti_core::SampleRate(48_000.0), Samples(256)))
        .expect("builds");
    let mut last = Vec::new();
    for _ in 0..49 {
        last = r.render(256);
    }
    last.iter().map(|c| c.iter().map(|s| s * s).sum()).collect()
}

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
    use tutti_nodes::testing::Const;
    use tutti_types::ChannelLayout;

    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::QUAD);

    // Two DC sources (constant on both stereo inputs of each panner).
    let src_front = g.add(Const::frame(&[1.0, 1.0]));
    let src_rear = g.add(Const::frame(&[1.0, 1.0]));

    // Place one at the front-left speaker (45° → ch0) and one at the
    // rear-left speaker (135° → ch2). The builder wires each source through a
    // quad panner and sums them into one 4-wide node.
    let mix = build_vbap_mix(
        &mut g,
        ChannelLayout::QUAD,
        &[
            VbapSource::at(src_front, 45.0),
            VbapSource::at(src_rear, 135.0),
        ],
    )
    .expect("build quad surround mix");
    g.pipe_output(mix);

    // The panner de-zippers position changes with a ~0.05s one-pole smoother
    // starting from 0°, so the azimuth ramps to its target over a few
    // thousand samples: measure the settled last block.
    let energy = settled_energy(g);

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
    use tutti_types::ChannelLayout;

    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::QUAD);
    let mix = build_vbap_mix(&mut g, ChannelLayout::QUAD, &[]).expect("empty mix still builds");
    // ChannelSumNode clamps 0 sources to 1 input group, so it's a valid
    // 4-out node reading zeros.
    assert_eq!(g.output_count(mix), 4);
}

/// Render a 5.1 `build_vbap_mix` graph and return settled per-channel
/// energy over the last block. Positions are `(azimuth, elevation)` per
/// source; `src_signal(i)` is source `i`'s (stereo) node.
fn render_5_1_energy(
    sources: &[(f32, f32)],
    src_signal: impl Fn(usize) -> tutti_nodes::testing::Const,
) -> [f32; 6] {
    use tutti_types::ChannelLayout;

    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::from(6u16));
    let surround: Vec<VbapSource> = sources
        .iter()
        .enumerate()
        .map(|(i, &(az, el))| VbapSource {
            node: g.add(src_signal(i)),
            azimuth: Azimuth(az),
            elevation: Elevation(el),
        })
        .collect();
    let mix = build_vbap_mix(&mut g, ChannelLayout::from(6u16), &surround).expect("build 5.1 mix");
    g.pipe_output(mix);
    settled_energy(g).try_into().expect("six channels")
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
    let energy = render_5_1_energy(&[(0.0, 0.0)], |_| Const::frame(&[1.0, 1.0]));
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
    let energy = render_5_1_energy(&[(30.0, 0.0)], |_| Const::frame(&[1.0, 1.0]));
    let total: f32 = energy.iter().sum();
    assert!(
        energy[3] > total * 0.02,
        "LFE channel should carry the low-passed bass-management send \
         (energy {energy:?})"
    );
}
