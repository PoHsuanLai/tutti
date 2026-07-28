//! End-to-end surround export, in pure tutti — no ECS, no Bevy.
//!
//! Builds a surround producer graph entirely from engine primitives
//! (`SpatialPannerNode` places each source, `ChannelSumUnit` folds them into an
//! N-wide master), renders it through the offline export pipeline, and asserts
//! the resulting file is a genuine multi-channel WAV whose channels carry the
//! placed energy — i.e. the surround producer and the multi-channel export path
//! work together without any DAW/ECS layer.

#![cfg(feature = "wav")]

use tutti_core::dsp::{dc, Net};
use tutti_export::{ChannelLayout, EncodeSpec, ExportSpec, RenderDuration, RenderSpec};

/// Render `net` to `path` as float WAV at `layout`, for `secs`.
///
/// The tests care about channel routing, not about export configuration, so the
/// spec is built once here rather than restated at every call site.
fn export(net: tutti_core::dsp::Net, layout: ChannelLayout, secs: f64, path: &std::path::Path) {
    tutti_export::render_to_file(
        net,
        &ExportSpec {
            render: RenderSpec {
                sample_rate: tutti_core::SampleRate(48_000.0),
                duration: RenderDuration::Seconds(secs),
                ..Default::default()
            },
            encode: EncodeSpec {
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
use tutti_units::{build_surround_mix, SurroundSource};

/// Build a quad surround graph via the engine's `build_surround_mix` helper: one
/// source at the front-left speaker (45°) and one at the rear-left speaker
/// (135°), each placed by a panner and summed into a 4-wide `Net` output.
fn quad_surround_net() -> Net {
    let mut net = Net::new(0, 4);

    let src_front = net.push(Box::new(dc((1.0, 1.0))));
    let src_rear = net.push(Box::new(dc((1.0, 1.0))));

    let mix = build_surround_mix(
        &mut net,
        ChannelLayout::Quad,
        &[
            SurroundSource::at(src_front, 45.0), // FL (ch0)
            SurroundSource::at(src_rear, 135.0), // RL (ch2)
        ],
    )
    .expect("build quad surround mix");
    net.pipe_output(mix);
    net
}

#[test]
fn quad_surround_graph_exports_a_four_channel_wav_with_rear_energy() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("surround.wav");

    // Render long enough for the panner's ~0.05s position smoother to settle
    // (0.3s @ 48k ≈ 14k samples, well past the ~2400-sample time constant).
    export(quad_surround_net(), ChannelLayout::Quad, 0.3, &path);

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

/// The Stage-4 export path: a net that starts at the **device (stereo) output**
/// width — exactly what the live engine produces — is widened offline via
/// `set_output_arity` and re-piped to a surround master before export. This
/// mirrors what the app's `widen_export_net` does to the cloned net, and proves
/// widening a stereo net does NOT lose the surround channels.
#[test]
fn stereo_net_widened_then_exports_four_channels() {
    use tutti_core::AudioUnit; // for `Net::outputs`

    // Build the surround producer inside a STEREO-output net (like the live one).
    let mut net = Net::new(0, 2);
    assert_eq!(net.outputs(), 2, "starts at device stereo width");
    let src_front = net.push(Box::new(dc((1.0, 1.0))));
    let src_rear = net.push(Box::new(dc((1.0, 1.0))));
    let mix = build_surround_mix(
        &mut net,
        ChannelLayout::Quad,
        &[
            SurroundSource::at(src_front, 45.0),
            SurroundSource::at(src_rear, 135.0),
        ],
    )
    .expect("build quad mix");

    // Widen the (backend-less) net to quad, then re-pipe the master. Without the
    // widen, `pipe_output` would only wire 2 global outputs and the rears drop.
    net.set_output_arity(4);
    assert_eq!(net.outputs(), 4, "net widened to quad");
    net.pipe_output(mix);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("widened.wav");
    export(net, ChannelLayout::Quad, 0.3, &path);

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
#[test]
fn surround_5_1_export_places_center_and_feeds_lfe() {
    let mut net = Net::new(0, 6);
    let src = net.push(Box::new(dc((1.0, 1.0))));
    // A single dead-center source.
    let mix = build_surround_mix(
        &mut net,
        ChannelLayout::from(6u16),
        &[SurroundSource::at(src, 0.0)],
    )
    .expect("build 5.1 mix");
    net.pipe_output(mix);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("surround51.wav");
    export(net, ChannelLayout::from(6u16), 0.3, &path);

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
    let mut net = Net::new(0, 6);
    let src = net.push(Box::new(dc((1.0, 1.0))));
    let mix = build_surround_mix(
        &mut net,
        ChannelLayout::from(6u16),
        &[SurroundSource::at(src, 0.0)], // dead center → C channel
    )
    .expect("build 5.1 mix");
    net.pipe_output(mix);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("downmix.wav");
    // Render the 5.1 graph but request a STEREO file → triggers the downmix.
    export(net, ChannelLayout::Stereo, 0.3, &path);

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
    // dc((0.8, 0.2)): left=0.8, right=0.2 → mono average = 0.5, NOT 0.8.
    let mut net = Net::new(0, 2);
    let src = net.push(Box::new(dc((0.8, 0.2))));
    net.connect_output(src, 0, 0);
    net.connect_output(src, 1, 1);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mono.wav");
    export(net, ChannelLayout::Mono, 0.1, &path);

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
    let mut net = Net::new(0, 6);
    let src = net.push(Box::new(dc((1.0, 1.0))));
    let mix = build_surround_mix(
        &mut net,
        ChannelLayout::from(6u16),
        &[SurroundSource::at(src, 0.0)], // dead center → C channel (2)
    )
    .expect("build 5.1 mix");
    net.pipe_output(mix);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("surround_mono.wav");
    export(net, ChannelLayout::Mono, 0.3, &path);

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
    let mut net = Net::new(0, 12);
    let src = net.push(Box::new(dc((1.0, 1.0))));
    let mix = build_surround_mix(
        &mut net,
        ChannelLayout::from(12u16),
        &[SurroundSource::at(src, 150.0)], // hard rear-left
    )
    .expect("build 7.1.4 mix");
    net.pipe_output(mix);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("atmos.wav");
    export(net, ChannelLayout::from(12u16), 0.3, &path);

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
    let mut net = Net::new(0, 12);
    let src = net.push(Box::new(dc((1.0, 1.0))));
    let mix = build_surround_mix(
        &mut net,
        ChannelLayout::from(12u16),
        &[SurroundSource::at(src, 150.0)], // hard rear-left → Lrs (discrete ch 6)
    )
    .expect("build 7.1.4 mix");
    net.pipe_output(mix);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("atmos_stereo.wav");
    export(net, ChannelLayout::Stereo, 0.3, &path);

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
