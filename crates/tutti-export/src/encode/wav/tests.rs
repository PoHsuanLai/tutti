use super::*;
use crate::config::{EncodeConfig, RenderConfig};
use crate::control::Pass;
use crate::options::AudioFormat;
use crate::render::PlaneSource;
use tutti_types::SampleRate;
use tutti_types::Samples;

fn config(channels: u16, depth: BitDepth) -> ExportConfig {
    ExportConfig {
        render: RenderConfig {
            sample_rate: SampleRate::from(1_000.0),
            duration_seconds: 0.008,
            ..Default::default()
        },
        encode: EncodeConfig {
            format: AudioFormat::Wav,
            bit_depth: depth,
            channels: ChannelLayout::from(channels),
            topology: None,
        },
        ..Default::default()
    }
}

fn encode(path: &Path, config: &ExportConfig, max_frames: u64, planes: &[Vec<f32>]) -> Result<()> {
    let plan = RenderPlan::new(&config.render);
    assert_eq!(plan.output_length, Samples(8));
    let rate = config.render.sample_rate;
    WavEncoder::create_with_limit(path, config, rate, &plan, max_frames)?
        .encode(
            &mut PlaneSource::new(planes),
            rate,
            &plan,
            config,
            &mut Control::new(Pass::ONLY, &mut crate::control::never_stop),
        )
        .map(|_| ())
}

/// An export at exactly the limit succeeds, and its 6-channel header
/// carries the 5.1 (side) mask rather than `hound`'s `0x3F`; four channels
/// are unassigned unless the config says they are quad.
///
/// Mutation: dropping the `patch_channel_mask_at` call in `encode` leaves
/// `0x3F` and fails the mask assertion; `admit` using `>=` fails the
/// limit-exact case; ignoring `config.encode.topology` fails the quad row.
#[test]
fn an_export_at_the_limit_succeeds_and_declares_its_mask() {
    let dir = tempfile::tempdir().unwrap();
    for (channels, topology, mask) in [
        (6u16, None, 0x60Fu32),
        (4, None, 0),
        (4, Some(tutti_types::ChannelTopology::quad()), 0x33),
    ] {
        let path = dir.path().join(format!("w{channels}_{mask}.wav"));
        let planes = vec![vec![0.5f32; 8]; channels as usize];
        let mut cfg = config(channels, BitDepth::Int24);
        cfg.encode.topology = topology;
        encode(&path, &cfg, 8, &planes).expect("8 frames fit a limit of 8");
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), mask);
        assert_eq!(
            hound::WavReader::open(&path).unwrap().len(),
            8 * u32::from(channels)
        );
    }
    let mut cfg = config(2, BitDepth::Int24);
    cfg.encode.topology = Some(tutti_types::ChannelTopology::quad());
    let err = encode(&dir.path().join("bad.wav"), &cfg, 8, &vec![vec![0.0; 8]; 2])
        .expect_err("a quad topology is not stereo");
    assert!(
        matches!(err, Error::InvalidConfig(ConfigError::TopologyWidth { .. })),
        "{err}"
    );
}

/// A plan that cannot fit is refused before the file is created: bytes
/// already at the path survive.
///
/// Mutation: moving the `planned_frames` check below `WavWriter::create`
/// (the order before this fix) truncates the sentinel to a 44-byte empty
/// WAV and fails the equality.
#[test]
fn a_plan_that_cannot_fit_is_refused_before_the_file_is_touched() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(2, BitDepth::Int16);
    let plan = RenderPlan::new(&config.render);
    let rate = config.render.sample_rate;
    let path = dir.path().join("keep.wav");
    std::fs::write(&path, b"sentinel").unwrap();
    let err = WavEncoder::create_with_limit(&path, &config, rate, &plan, 7)
        .err()
        .expect("8 frames do not fit a limit of 7");
    assert!(
        matches!(err, Error::WavTooLarge { limit_frames: 7 }),
        "{err}"
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"sentinel",
        "a refused export must not touch the file at the path"
    );
}

/// A render that crosses the limit mid-way leaves no file behind. The plan
/// handed to `create` is a short one so the up-front check passes; the
/// render itself is 8 frames against a limit of 6.
///
/// Mutation: deleting the `remove_file` call leaves a finalized prefix and
/// fails the `exists` assertion.
#[test]
fn a_render_that_crosses_the_limit_removes_its_partial_file() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(2, BitDepth::Int16);
    let rate = config.render.sample_rate;
    let mut short = config.clone();
    short.render.duration_seconds = 0.004;
    let short_plan = RenderPlan::new(&short.render);
    let plan = RenderPlan::new(&config.render);
    let path = dir.path().join("partial.wav");
    let enc = WavEncoder::create_with_limit(&path, &config, rate, &short_plan, 6).unwrap();
    let planes = vec![vec![0.5f32; 8]; 2];
    let err = enc
        .encode(
            &mut PlaneSource::new(&planes),
            rate,
            &plan,
            &config,
            &mut Control::new(Pass::ONLY, &mut crate::control::never_stop),
        )
        .expect_err("8 frames cross a limit of 6");
    assert!(
        matches!(err, Error::WavTooLarge { limit_frames: 6 }),
        "{err}"
    );
    assert!(!path.exists(), "the truncated file must be removed");
}

/// The running count fails on the frame that crosses the limit, not before.
///
/// Mutation: `>` to `>=` in `admit` fails the second assertion; removing
/// the `saturating_add` for `+` would panic on overflow in debug.
#[test]
fn the_running_count_fails_on_the_crossing_frame() {
    let mut budget = FrameBudget { written: 0, max: 5 };
    budget.admit(3).unwrap();
    budget.admit(2).expect("exactly the limit");
    let err = budget.admit(1).expect_err("one past");
    assert!(matches!(err, Error::WavTooLarge { limit_frames: 5 }));
    assert!(FrameBudget { written: 0, max: 5 }.admit(u64::MAX).is_err());
}
