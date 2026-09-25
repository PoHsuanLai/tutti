//! End-to-end surround export, in pure tutti — no ECS, no Bevy.
//!
//! Builds a surround producer graph entirely from engine primitives
//! (`VbapPannerNode` places each source, `ChannelSumNode` folds them into an
//! N-wide master), renders it through the offline export pipeline, and asserts
//! the resulting file is a genuine multi-channel WAV whose channels carry the
//! placed energy — i.e. the surround producer and the multi-channel export path
//! work together without any DAW/ECS layer.
//!
//! The graphs are native (`tutti_graph::GraphBuilder`, rendered through
//! `RenderGraph::Graph`; doc 013 Phase 3 PR 8), and the mix is
//! `tutti_spatial::vbap_mix_parts` wired on the builder — the same units and
//! edges `build_vbap_mix` puts in a `Net` (`tutti-spatial`'s
//! `tests/vbap_mix_parts.rs` pins the two bit-identical).

#![cfg(feature = "wav")]

use tutti_export::{ChannelLayout, EncodeConfig, ExportConfig, RenderConfig, RenderGraph};
use tutti_graph::GraphBuilder;
use tutti_types::NodeKey;

/// The rate [`export`] renders at, and so the rate every graph is built for.
const RATE: tutti_core::SampleRate = tutti_core::SampleRate(48_000.0);

/// Render `g` to `path` as float WAV at `layout`, for `secs`.
///
/// The tests care about channel routing, not about export configuration, so the
/// config is built once here rather than restated at every call site.
fn export(g: GraphBuilder, layout: ChannelLayout, secs: f64, path: &std::path::Path) {
    let (editor, executor) = g.build(RenderGraph::prepare(RATE)).expect("builds");
    tutti_export::render_to_file(
        RenderGraph::Graph { editor, executor },
        &ExportConfig {
            render: RenderConfig {
                sample_rate: RATE,
                duration_seconds: secs,
                ..Default::default()
            },
            encode: EncodeConfig {
                bit_depth: tutti_export::BitDepth::Float32,
                channels: layout,
                ..Default::default()
            },
            ..Default::default()
        },
        &tutti_export::FrozenClock,
        path,
    )
    .expect("export");
}
use tutti_nodes::testing::Const;
use tutti_spatial::{vbap_mix_parts, VbapMixNode, VbapSource};

/// Assemble a VBAP mix of `sources` into `g` and return the summed mix node —
/// `build_vbap_mix` for the builder: `vbap_mix_parts`' units added, and every
/// one of its edges wired, the sources resolved by position in `sources`.
fn vbap_mix(
    g: &mut GraphBuilder,
    layout: ChannelLayout,
    sources: &[VbapSource<NodeKey>],
) -> Result<NodeKey, tutti_spatial::VbapError> {
    let parts = vbap_mix_parts(layout, sources)?;
    let edges = parts.edges().to_vec();
    let panners: Vec<NodeKey> = parts
        .panners
        .into_iter()
        .map(|p| g.add_unit(Box::new(p)))
        .collect();
    let lfe = parts.lfe.map(|send| {
        (
            g.add_unit(Box::new(send.sum)),
            g.add_unit(Box::new(send.lowpass)),
        )
    });
    let sum = g.add_unit(Box::new(parts.sum));
    let key = |n: VbapMixNode| match n {
        VbapMixNode::Source(i) => sources[i].node,
        VbapMixNode::Panner(i) => panners[i],
        VbapMixNode::LfeSum => lfe.expect("an LFE edge implies the send").0,
        VbapMixNode::LfeLowpass => lfe.expect("an LFE edge implies the send").1,
        VbapMixNode::Sum => sum,
    };
    for e in edges {
        g.connect(key(e.from), e.from_port, key(e.to), e.to_port);
    }
    Ok(sum)
}

/// Build a quad surround graph via the engine's VBAP mix: one source at the
/// front-left speaker (45°) and one at the rear-left speaker (135°), each
/// placed by a panner and summed into a 4-wide output.
fn quad_surround_graph() -> GraphBuilder {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::QUAD);

    let src_front = g.add_unit(Box::new(Const::frame(&[1.0, 1.0])));
    let src_rear = g.add_unit(Box::new(Const::frame(&[1.0, 1.0])));

    let mix = vbap_mix(
        &mut g,
        ChannelLayout::QUAD,
        &[
            VbapSource::at(src_front, 45.0), // FL (ch0)
            VbapSource::at(src_rear, 135.0), // RL (ch2)
        ],
    )
    .expect("build quad surround mix");
    g.pipe_output(mix);
    g
}

#[test]
fn quad_surround_graph_exports_a_four_channel_wav_with_rear_energy() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("surround.wav");

    // Render long enough for the panner's ~0.05s position smoother to settle
    // (0.3s @ 48k ≈ 14k samples, well past the ~2400-sample time constant).
    export(quad_surround_graph(), ChannelLayout::QUAD, 0.3, &path);

    let reader = hound::WavReader::open(&path).unwrap();
    assert_eq!(reader.spec().channels, 4, "file must carry four channels");

    // Deinterleave and sum energy per channel over the settled tail (the last
    // quarter of the file), skipping the smoother's ramp-in.
    let samples: Vec<f32> = reader.into_samples::<f32>().map(|s| s.unwrap()).collect();
    let frames = samples.len() / 4;
    let tail_start = (frames * 3) / 4;
    let mut energy = [0.0f32; 4];
    for f in tail_start..frames {
        for (c, e) in energy.iter_mut().enumerate() {
            let s = samples[f * 4 + c];
            *e += s * s;
        }
    }
    let total: f32 = energy.iter().sum();
    assert!(total > 0.0, "surround export produced silence");

    // Front source → front-left (ch0); rear source → rear-left (ch2). Both must
    // carry real energy, proving per-source placement survived to the file and
    // the export did NOT collapse surround to the front stereo pair.
    assert!(
        energy[0] > total * 0.2,
        "front-left channel should carry the front source (energy {energy:?})"
    );
    assert!(
        energy[2] > total * 0.2,
        "rear-left channel should carry the rear source — surround reached the \
         file intact (energy {energy:?})"
    );
}

/// The Stage-4 export path: a graph that starts at the **device (stereo)
/// output** width — exactly what the live engine produces — is widened offline
/// and re-piped to a surround master before export. This mirrors what a host
/// does to the graph it exports (a `Net`'s `set_output_arity`; on the native
/// graph, the topology's global outputs grown), and proves widening a stereo
/// graph does NOT lose the surround channels.
///
/// Mutation (run): widen *after* `pipe_output` → only the first two global
/// outputs are wired, the file is 4-wide with silent rears, and the rear-left
/// assertion fails.
#[test]
fn stereo_net_widened_then_exports_four_channels() {
    // Build the surround producer inside a STEREO-output graph (like the live one).
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    assert_eq!(g.outputs(), 2, "starts at device stereo width");
    let src_front = g.add_unit(Box::new(Const::frame(&[1.0, 1.0])));
    let src_rear = g.add_unit(Box::new(Const::frame(&[1.0, 1.0])));
    let mix = vbap_mix(
        &mut g,
        ChannelLayout::QUAD,
        &[
            VbapSource::at(src_front, 45.0),
            VbapSource::at(src_rear, 135.0),
        ],
    )
    .expect("build quad mix");

    // Widen the graph to quad (the new outputs silent until wired), then re-pipe
    // the master. Without the widen, `pipe_output` would only wire 2 global
    // outputs and the rears drop.
    g.spec_mut()
        .topology
        .outputs
        .resize(4, tutti_types::graph::Source::Zero);
    assert_eq!(g.outputs(), 4, "graph widened to quad");
    g.pipe_output(mix);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("widened.wav");
    export(g, ChannelLayout::QUAD, 0.3, &path);

    let reader = hound::WavReader::open(&path).unwrap();
    assert_eq!(
        reader.spec().channels,
        4,
        "widened net exports four channels"
    );

    let samples: Vec<f32> = reader.into_samples::<f32>().map(|s| s.unwrap()).collect();
    let frames = samples.len() / 4;
    let tail_start = (frames * 3) / 4;
    let mut energy = [0.0f32; 4];
    for f in tail_start..frames {
        for (c, e) in energy.iter_mut().enumerate() {
            *e += samples[f * 4 + c].powi(2);
        }
    }
    let total: f32 = energy.iter().sum();
    assert!(total > 0.0, "widened export produced silence");
    // The rear channel (2) carries real energy — proof the widen preserved the
    // surround channels through to the file.
    assert!(
        energy[2] > total * 0.2,
        "rear-left survived the widen (energy {energy:?})"
    );
}

/// A 5.1 (6-channel) export must (a) place a center source in the CENTER channel
/// (2) using the correct SMPTE/WAV channel order — NOT slide the surrounds into
/// the LFE slot — and (b) carry a bass-managed low-passed send in the LFE
/// channel (3). This proves the channel-remap + LFE-routing fixes reach a real
/// file, not just the graph.
///
/// 5.1 file order: FL(0), FR(1), C(2), LFE(3), SL(4), SR(5).
///
/// Mutation (run): [`vbap_mix`] skipping the low-pass→sum edge → the LFE
/// assertion fails.
#[test]
fn surround_5_1_export_places_center_and_feeds_lfe() {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::from(6u16));
    let src = g.add_unit(Box::new(Const::frame(&[1.0, 1.0])));
    // A single dead-center source.
    let mix = vbap_mix(
        &mut g,
        ChannelLayout::from(6u16),
        &[VbapSource::at(src, 0.0)],
    )
    .expect("build 5.1 mix");
    g.pipe_output(mix);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("surround51.wav");
    export(g, ChannelLayout::from(6u16), 0.3, &path);

    // The 6-channel file must declare WAVEFORMATEXTENSIBLE (0xfffe) with the
    // standard 5.1 dwChannelMask 0x3F (FL|FR|FC|LFE|BL|BR), so other tools read
    // the speaker assignment correctly. hound emits this automatically for
    // channels > 2 — this guards that our surround files stay interoperable.
    {
        let bytes = std::fs::read(&path).unwrap();
        let fmt = bytes.windows(4).position(|w| w == b"fmt ").unwrap();
        let fmt_tag = u16::from_le_bytes([bytes[fmt + 8], bytes[fmt + 9]]);
        assert_eq!(fmt_tag, 0xfffe, "6ch file must be WAVEFORMATEXTENSIBLE");
        let mask = u32::from_le_bytes([
            bytes[fmt + 28],
            bytes[fmt + 29],
            bytes[fmt + 30],
            bytes[fmt + 31],
        ]);
        assert_eq!(mask, 0x3f, "5.1 channel mask must be FL|FR|FC|LFE|BL|BR");
    }

    let reader = hound::WavReader::open(&path).unwrap();
    assert_eq!(reader.spec().channels, 6, "file must carry six channels");

    let samples: Vec<f32> = reader.into_samples::<f32>().map(|s| s.unwrap()).collect();
    let frames = samples.len() / 6;
    let tail_start = (frames * 3) / 4;
    let mut energy = [0.0f32; 6];
    for f in tail_start..frames {
        for (c, e) in energy.iter_mut().enumerate() {
            *e += samples[f * 6 + c].powi(2);
        }
    }
    let total: f32 = energy.iter().sum();
    assert!(total > 0.0, "5.1 export produced silence");

    // Center source dominates the CENTER channel (2), not leaking to surrounds.
    assert!(
        energy[2] > total * 0.4,
        "center source should dominate the center channel (energy {energy:?})"
    );
    assert!(
        energy[4] < total * 0.05 && energy[5] < total * 0.05,
        "center source must not light the surround channels (energy {energy:?})"
    );
    // LFE channel (3) carries the bass-managed send (a DC source is below the
    // 120 Hz cutoff, so it passes).
    assert!(
        energy[3] > total * 0.02,
        "LFE channel should carry the low-passed send (energy {energy:?})"
    );
}

/// Exporting a 5.1 surround graph to a STEREO file must fold the center and
/// surrounds in with the ITU/Dolby matrix, not drop them. A source panned dead
/// center (energy only in the C channel) must appear in BOTH stereo channels at
/// −3 dB — if the downmix just took channels 0/1, a center source would vanish.
#[test]
fn surround_5_1_downmixes_center_to_both_stereo_channels() {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::from(6u16));
    let src = g.add_unit(Box::new(Const::frame(&[1.0, 1.0])));
    let mix = vbap_mix(
        &mut g,
        ChannelLayout::from(6u16),
        &[VbapSource::at(src, 0.0)], // dead center → C channel
    )
    .expect("build 5.1 mix");
    g.pipe_output(mix);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("downmix.wav");
    // Render the 5.1 graph but request a STEREO file → triggers the downmix.
    export(g, ChannelLayout::STEREO, 0.3, &path);

    let reader = hound::WavReader::open(&path).unwrap();
    assert_eq!(reader.spec().channels, 2, "downmixed file is stereo");

    let samples: Vec<f32> = reader.into_samples::<f32>().map(|s| s.unwrap()).collect();
    let frames = samples.len() / 2;
    let tail_start = (frames * 3) / 4;
    let (mut el, mut er) = (0.0f32, 0.0f32);
    // Per-sample peak |Lo| in the settled tail — used to prove the C channel is
    // folded in at −3 dB, not merely the front L/R VBAP spill.
    let mut peak_l = 0.0f32;
    for f in tail_start..frames {
        let l = samples[f * 2];
        el += l.powi(2);
        er += samples[f * 2 + 1].powi(2);
        peak_l = peak_l.max(l.abs());
    }
    // Center-panned source must reach BOTH channels, roughly symmetric.
    assert!(el > 0.0 && er > 0.0, "center source vanished in downmix");
    let ratio = el / er;
    assert!(
        (0.5..2.0).contains(&ratio),
        "center downmix should be ~symmetric L/R, got el={el} er={er}"
    );
    // The dominant C channel (not just the front L/R spill) must be folded in.
    // With the fold `Lo = FL + 0.707·C` peaks at ≈1.71 (front spill ≈1.0 plus
    // 0.707·C, C≈1.0). If C were dropped — the old truncating channel-pick — `Lo`
    // would peak at only the front spill (≈0.71). The `> 1.2` threshold sits
    // firmly between the two, so this fails the instant C stops being folded.
    // This is the assertion the old test lacked: it passed on the spill alone.
    assert!(
        peak_l > 1.2,
        "C channel was dropped, not folded — |Lo| peaked at {peak_l} (expect ≈1.71 with the C fold)"
    );
}

/// A STEREO graph exported to a MONO file must AVERAGE L+R, not keep only the
/// left channel. Regression guard: the render→frame fold previously picked a
/// single source channel per destination, so `n_out=2 → CH=1` silently dropped
/// the right channel. Distinct constant L/R make the drop visible.
#[test]
fn stereo_graph_exports_folded_mono_not_left_only() {
    // Const::frame(&[0.8, 0.2]): left=0.8, right=0.2 → mono average = 0.5, NOT 0.8.
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let src = g.add_unit(Box::new(Const::frame(&[0.8, 0.2])));
    g.connect_output(src, 0, 0).connect_output(src, 1, 1);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mono.wav");
    export(g, ChannelLayout::MONO, 0.1, &path);

    let reader = hound::WavReader::open(&path).unwrap();
    assert_eq!(reader.spec().channels, 1, "file is mono");
    let samples: Vec<f32> = reader.into_samples::<f32>().map(|s| s.unwrap()).collect();
    let mid = samples[samples.len() / 2];
    // (0.8 + 0.2) / 2 = 0.5. Left-only would read 0.8; right-only 0.2.
    assert!(
        (mid - 0.5).abs() < 1e-3,
        "mono must be the L/R average (0.5), got {mid} — a drop would give 0.8 or 0.2"
    );
}

/// A 5.1 graph exported to a MONO file must fold C + surrounds in (drop LFE),
/// not keep only channel 0. A dead-center source (dominant C channel) must
/// survive to the mono file — proof the matrix mono fold runs, not a channel
/// pick that would drop C.
#[test]
fn surround_5_1_exports_folded_mono_keeps_center() {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::from(6u16));
    let src = g.add_unit(Box::new(Const::frame(&[1.0, 1.0])));
    let mix = vbap_mix(
        &mut g,
        ChannelLayout::from(6u16),
        &[VbapSource::at(src, 0.0)], // dead center → C channel (2)
    )
    .expect("build 5.1 mix");
    g.pipe_output(mix);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("surround_mono.wav");
    export(g, ChannelLayout::MONO, 0.3, &path);

    let reader = hound::WavReader::open(&path).unwrap();
    assert_eq!(reader.spec().channels, 1, "file is mono");
    let samples: Vec<f32> = reader.into_samples::<f32>().map(|s| s.unwrap()).collect();
    let frames = samples.len();
    let tail_start = (frames * 3) / 4;
    let peak = samples[tail_start..]
        .iter()
        .fold(0.0f32, |m, s| m.max(s.abs()));
    // The C channel folds into mono at (Lo+Ro)·0.707, with Lo,Ro ≈ FL + 0.707·C,
    // so a dead-center unit source peaks at ≈2.41. A channel-0-only pick (the old
    // truncating driver) would peak at only the front spill (≈0.71). The `> 1.5`
    // threshold sits between the two, failing the instant C stops being folded.
    assert!(
        peak > 1.5,
        "center source dropped in the mono fold — |mono| peaked at {peak}"
    );
}

/// A 7.1.4 Atmos (12-channel) graph exports a genuine 12-channel WAV — the
/// producer supports the layout (VBAP atmos_7_1_4 preset), and the export
/// dispatch admits width 12. A rear-panned source lands in a rear-surround
/// channel, proving per-source placement survives to the 12-wide file.
///
/// 7.1.4 order: FL FR C LFE Lss Rss Lrs Rrs + 4 heights.
#[test]
fn atmos_7_1_4_exports_twelve_channels_with_rear_energy() {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::from(12u16));
    let src = g.add_unit(Box::new(Const::frame(&[1.0, 1.0])));
    let mix = vbap_mix(
        &mut g,
        ChannelLayout::from(12u16),
        &[VbapSource::at(src, 150.0)], // hard rear-left
    )
    .expect("build 7.1.4 mix");
    g.pipe_output(mix);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("atmos.wav");
    export(g, ChannelLayout::from(12u16), 0.3, &path);

    let reader = hound::WavReader::open(&path).unwrap();
    assert_eq!(reader.spec().channels, 12, "file carries twelve channels");
    let samples: Vec<f32> = reader.into_samples::<f32>().map(|s| s.unwrap()).collect();
    let frames = samples.len() / 12;
    let tail = (frames * 3) / 4;
    let mut energy = [0.0f32; 12];
    for f in tail..frames {
        for (c, e) in energy.iter_mut().enumerate() {
            *e += samples[f * 12 + c].powi(2);
        }
    }
    let total: f32 = energy.iter().sum();
    assert!(total > 0.0, "7.1.4 export produced silence");
    // A rear-left source's energy must reach a rear/side-surround-left channel
    // (Lss=4 or Lrs=6), not collapse to the front pair.
    assert!(
        energy[4] > total * 0.1 || energy[6] > total * 0.1,
        "rear-left source must reach a left surround channel (energy {energy:?})"
    );
}

/// A 7.1.4 (12ch) graph exported to a STEREO file folds the discrete surround /
/// height channels into the front pair — they must NOT be dropped. A hard
/// rear-left source (150° → the rear-left-surround channel, Lrs=6, a *discrete*
/// channel the front pair does not carry) must reach the LEFT downmix channel at
/// −3 dB and stay out of the right. Guards the 12→stereo fold arm: a truncating
/// front-pair pick would drop Lrs entirely and leave the left channel silent.
#[test]
fn atmos_7_1_4_downmixes_surround_into_front() {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::from(12u16));
    let src = g.add_unit(Box::new(Const::frame(&[1.0, 1.0])));
    let mix = vbap_mix(
        &mut g,
        ChannelLayout::from(12u16),
        &[VbapSource::at(src, 150.0)], // hard rear-left → Lrs (discrete ch 6)
    )
    .expect("build 7.1.4 mix");
    g.pipe_output(mix);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("atmos_stereo.wav");
    export(g, ChannelLayout::STEREO, 0.3, &path);

    let reader = hound::WavReader::open(&path).unwrap();
    assert_eq!(reader.spec().channels, 2, "downmixed file is stereo");
    let samples: Vec<f32> = reader.into_samples::<f32>().map(|s| s.unwrap()).collect();
    let frames = samples.len() / 2;
    let tail = (frames * 3) / 4;
    let (mut el, mut er) = (0.0f32, 0.0f32);
    for f in tail..frames {
        el += samples[f * 2].powi(2);
        er += samples[f * 2 + 1].powi(2);
    }
    // The rear-left surround folds into Lo (left) only. A truncating pick would
    // drop Lrs and leave the left channel silent (near-zero energy).
    assert!(
        el > 0.0,
        "rear-left surround was dropped, not folded into the left downmix (el={el})"
    );
    assert!(
        el > er * 4.0,
        "a rear-left source must fold to the LEFT downmix, not the right (el={el}, er={er})"
    );
}
