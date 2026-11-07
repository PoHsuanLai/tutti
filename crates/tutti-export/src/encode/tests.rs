use super::*;
use crate::config::{EncodeConfig, RenderConfig};
use crate::control::{Pass, never_stop};
use crate::{Format, Pcm};
use tutti_types::SampleRate;

fn config(channels: u16, pcm: Pcm) -> ExportConfig {
    ExportConfig {
        render: RenderConfig {
            sample_rate: SampleRate::from(1_000.0),
            duration_seconds: 0.008,
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
/// 32-bit sizes, and the source is a few frames because the refusal is made
/// before it is read.
///
/// Mutation: moving the `check_fits` call below `Encoder::create_file` and
/// writing to the path itself (not a temporary) would truncate the sentinel;
/// deleting the `check_fits` call lets the (tiny) render succeed and fails the
/// `expect_err`.
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
    let planes = vec![vec![0.0f32; 8]];
    let err = encode_to_file(
        &mut PlaneSource::new(&planes),
        rate,
        &plan,
        &config,
        &path,
        &mut Control::new(Pass::ONLY, &mut never_stop),
    )
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
/// higher rate raises: 2^31 frames fit a mono 16-bit WAV at the render rate,
/// and do not once they are written at twice the rate.
///
/// Mutation: returning `kept` before scaling (ignoring the resample) accepts
/// the second plan and fails the last assertion.
#[test]
fn the_limit_is_judged_at_the_output_rate() {
    let mut config = config(1, Pcm::Int16);
    let frames = Samples(1 << 31);
    let plan = RenderPlan {
        total: frames,
        output_length: frames,
        latency: Samples(0),
    };
    let rate = config.render.sample_rate;
    let max = config.encode.format.max_frames(config.encode.channels);
    assert!(max.is_some_and(|m| m >= 1 << 31 - 1));
    check_fits(&plan, rate, &config, max).expect("2^31 mono 16-bit frames fit");
    config.resample = Some(crate::Resample::to(2_000.0));
    assert!(check_fits(&plan, rate, &config, max).is_err());
}

/// The frames a file will hold are the plan's, scaled by the resample ratio
/// and rounded up; a resample to the rate the frames are at is no resample.
///
/// Mutation: rounding down instead of up gives 11 for the 1.5x case and fails
/// the second assertion (8 frames at 1.5x is 12 exactly, so use the first
/// non-integer ratio below); dropping the `out == src` guard cannot change
/// the figure and is covered by the third assertion only through the
/// resample being skipped by `pump_blocks`.
#[test]
fn the_planned_frames_follow_the_plan_and_the_resample() {
    let mut config = config(2, Pcm::Int16);
    let plan = RenderPlan::new(&config.render);
    let rate = config.render.sample_rate;
    assert_eq!(planned_frames(&plan, rate, &config), 8);
    config.resample = Some(crate::Resample::to(1_500.0));
    assert_eq!(planned_frames(&plan, rate, &config), 12);
    config.resample = Some(crate::Resample::to(1_000.0));
    assert_eq!(
        planned_frames(&plan, rate, &config),
        8,
        "the same rate is no resample"
    );
}
