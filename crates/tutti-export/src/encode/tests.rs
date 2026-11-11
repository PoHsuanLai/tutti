use super::*;
use crate::config::{EncodeConfig, ExportConfig, RenderConfig};
use crate::{Format, Pcm};
use tutti_types::Seconds;

/// The plan of a config's render.
fn plan_of(config: &ExportConfig) -> RenderPlan {
    RenderPlan::of(&config.job().expect("a render"))
}

/// `config`'s file at `path`.
fn target(config: &ExportConfig, path: &Path) -> FileTarget {
    config.file_target(path, None)
}

fn config(channels: u16, pcm: Pcm) -> ExportConfig {
    ExportConfig {
        render: RenderConfig {
            sample_rate: SampleRate::new(1_000.0),
            duration_seconds: Seconds(0.008),
            ..Default::default()
        },
        encode: EncodeConfig {
            format: Format::Wav(pcm),
            channels: ChannelLayout::from(channels),
            topology: None,
        },
        ..Default::default()
    }
}

/// A plan that cannot fit a WAV file is refused before any of it is rendered
/// and before the path is touched: bytes already at the path survive.
///
/// The plan is real: 2^32 mono frames at 16 bits is 8 GiB, past the format's
/// 32-bit sizes, and no frame is written because the refusal is made before
/// the file is opened.
///
/// Mutation: deleting the `check_fits` call in `open_file` lets the file open
/// and fails the `expect_err`.
#[test]
fn a_plan_that_cannot_fit_is_refused_before_the_file_is_touched() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(1, Pcm::Int16);
    let frames = Samples(1 << 32);
    let plan = RenderPlan {
        total: frames,
        output_length: frames,
        latency: Samples(0),
    };
    let rate = config.render.sample_rate;
    let path = dir.path().join("keep.wav");
    std::fs::write(&path, b"sentinel").unwrap();
    let err = open_file(&target(&config, &path), &plan, rate)
        .map(drop)
        .expect_err("2^32 mono 16-bit frames do not fit a WAV file");
    assert!(
        matches!(
            err,
            Error::Encode(tutti_codec::Error::WavTooLarge { limit_frames }) if limit_frames < 1 << 32
        ),
        "{err}"
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"sentinel",
        "a refused export must not touch the file at the path"
    );
}

/// The limit is judged on the frames the file will hold, which a resample to a
/// higher rate raises: 2^31 - 1000 frames fit a mono 16-bit WAV at the render
/// rate, and do not once they are written at twice the rate.
///
/// Mutation: returning `kept` before scaling (ignoring the resample) accepts
/// the second plan and fails the last assertion.
#[test]
fn the_limit_is_judged_at_the_output_rate() {
    let mut config = config(1, Pcm::Int16);
    let frames = Samples((1 << 31) - 1_000);
    let plan = RenderPlan {
        total: frames,
        output_length: frames,
        latency: Samples(0),
    };
    let rate = config.render.sample_rate;
    let max = config.encode.format.max_frames(config.encode.channels);
    let path = Path::new("unused.wav");
    check_fits(&plan, rate, &target(&config, path), max)
        .expect("2^31 - 1000 mono 16-bit frames fit");
    config.resample = Some(crate::Resample::to(SampleRate::new(2_000.0)));
    assert!(check_fits(&plan, rate, &target(&config, path), max).is_err());
}

/// The frames a file will hold are the plan's, scaled by the resample ratio
/// and rounded up; a resample to the rate the frames are at is no resample.
///
/// Mutation: rounding down instead of up gives 8 for the 1.1x case (8.8 frames)
/// and fails the second assertion.
#[test]
fn the_planned_frames_follow_the_plan_and_the_resample() {
    let mut config = config(2, Pcm::Int16);
    let plan = plan_of(&config);
    let rate = config.render.sample_rate;
    let path = Path::new("unused.wav");
    assert_eq!(planned_frames(&plan, rate, &target(&config, path)), 8);
    config.resample = Some(crate::Resample::to(SampleRate::new(1_100.0)));
    assert_eq!(planned_frames(&plan, rate, &target(&config, path)), 9);
    config.resample = Some(crate::Resample::to(SampleRate::new(1_000.0)));
    assert_eq!(
        planned_frames(&plan, rate, &target(&config, path)),
        8,
        "the same rate is no resample"
    );
}

/// A topology reaches the file: four channels declare the quad mask `0x33` only
/// when told they are quad, six declare 5.1 by their width, and a bare four
/// are unassigned. A topology of another width than the file is refused before
/// anything is written.
///
/// This restores the export-level half of the old
/// `an_export_at_the_limit_succeeds_and_declares_its_mask`; the limit half
/// lives in `tutti-codec`, where the limit can be lowered.
///
/// Mutation: `topology: None` in `spec()` leaves the quad file at mask 0
/// and fails the first row; deleting the width check in `WavWriter::create`
/// lets the stereo file declare a quad topology and fails the refusal.
#[cfg(feature = "wav")]
#[test]
fn the_topology_reaches_the_file_and_must_match_its_width() {
    let dir = tempfile::tempdir().unwrap();
    for (channels, topology, mask) in [
        (4u16, Some(tutti_types::ChannelTopology::quad()), 0x33u32),
        (4, None, 0),
        (6, None, 0x60F),
    ] {
        let mut cfg = config(channels, Pcm::Int24);
        cfg.encode.topology = topology;
        let path = dir.path().join(format!("m{channels}_{mask}.wav"));
        let layout = ChannelLayout::from(channels);
        let mut chain =
            open_file(&target(&cfg, &path), &plan_of(&cfg), cfg.render.sample_rate).unwrap();
        chain
            .block(Interleaved::new(
                &vec![0.5f32; 8 * usize::from(channels)],
                layout,
            ))
            .unwrap();
        chain.finalize().unwrap().commit().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(
            u32::from_le_bytes(bytes[40..44].try_into().unwrap()),
            mask,
            "{channels} channels"
        );
        assert_eq!(
            hound::WavReader::open(&path).unwrap().len(),
            8 * u32::from(channels)
        );
    }

    let mut cfg = config(2, Pcm::Int24);
    cfg.encode.topology = Some(tutti_types::ChannelTopology::quad());
    let path = dir.path().join("bad.wav");
    let err = open_file(&target(&cfg, &path), &plan_of(&cfg), cfg.render.sample_rate)
        .map(drop)
        .expect_err("a quad topology is not stereo");
    assert!(
        matches!(
            err,
            Error::Encode(tutti_codec::Error::Spec(
                tutti_codec::SpecError::TopologyWidth { .. }
            ))
        ),
        "{err}"
    );
    assert!(!path.exists());
}
