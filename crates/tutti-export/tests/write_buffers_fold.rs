//! `write_buffers` places planes of another width than the file's the way the
//! graph route does: a fold by the engine's order (or the file's stated
//! topology), never a truncation or a zero-pad, and an error for a width it
//! cannot place.
//!
//! Every expected figure is the analytic ITU/Dolby coefficient (`M3DB` on the
//! centre and the surrounds, the LFE left out), not another render.

mod support;
use crate::support::{EncodeConfig, ExportConfig, write_buffers};
use tutti_codec::{Format, Pcm};
use tutti_export::{ConfigError, Error, Rendered};
use tutti_types::ChannelLayout;
use tutti_types::{ChannelTopology, M3DB, SampleRate, Speaker};

const RATE: SampleRate = SampleRate(48_000.0);
const FRAMES: usize = 256;

/// One constant per channel, so a channel that goes missing shows in the sum.
fn rendered(values: &[f32]) -> Rendered {
    Rendered::new(values.iter().map(|&v| vec![v; FRAMES]).collect(), RATE, 0)
        .expect("equal planes at a valid rate")
}

fn config(channels: ChannelLayout, topology: Option<ChannelTopology>) -> ExportConfig {
    ExportConfig {
        encode: EncodeConfig {
            format: Format::Wav(Pcm::Float32),
            channels,
            topology,
        },
        ..Default::default()
    }
}

/// The first frame of the written file.
fn first_frame(path: &std::path::Path, width: u16) -> Vec<f32> {
    let reader = hound::WavReader::open(path).unwrap();
    assert_eq!(reader.spec().channels, width);
    let samples: Vec<f32> = reader.into_samples::<f32>().map(|s| s.unwrap()).collect();
    assert_eq!(samples.len(), FRAMES * usize::from(width));
    samples[..usize::from(width)].to_vec()
}

/// 5.1 planes written as stereo carry the centre and the surrounds at -3 dB
/// into the front pair, and drop only the LFE: `L + C*M3DB + Ls*M3DB`.
#[test]
fn surround_planes_written_as_stereo_are_folded_not_truncated() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("down.wav");
    // L R C LFE Ls Rs
    let planes = rendered(&[0.1, 0.2, 0.3, 0.4, 0.5, 0.6]);
    write_buffers(&planes, &config(ChannelLayout::STEREO, None), &path).unwrap();
    let frame = first_frame(&path, 2);
    let left = 0.1 + 0.3 * M3DB + 0.5 * M3DB;
    let right = 0.2 + 0.3 * M3DB + 0.6 * M3DB;
    assert!((frame[0] - left).abs() < 1e-6, "{} vs {left}", frame[0]);
    assert!((frame[1] - right).abs() < 1e-6, "{} vs {right}", frame[1]);
}

/// The file's stated topology decides the placement: 5.1 into a file whose
/// four channels are L R C LFE puts the centre in its own channel at full
/// level, where the default quad order would fold it into the fronts.
#[test]
fn the_files_topology_places_the_channels() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lcr.wav");
    let topology = ChannelTopology::new([
        Speaker::FrontLeft,
        Speaker::FrontRight,
        Speaker::FrontCenter,
        Speaker::LowFrequency,
    ]);
    let planes = rendered(&[0.1, 0.2, 0.3, 0.4, 0.5, 0.6]);
    write_buffers(&planes, &config(ChannelLayout::QUAD, Some(topology)), &path).unwrap();
    let frame = first_frame(&path, 4);
    assert!(
        (frame[2] - 0.3).abs() < 1e-6,
        "the centre keeps its channel"
    );
    assert!((frame[3] - 0.4).abs() < 1e-6, "and the LFE its own");
}

/// A width with no order that would have to drop a channel is refused, not
/// truncated: 5 planes into a stereo file.
#[test]
fn a_width_it_cannot_place_is_an_error_not_a_drop() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("refused.wav");
    let err = write_buffers(
        &rendered(&[0.1; 5]),
        &config(ChannelLayout::STEREO, None),
        &path,
    )
    .expect_err("five unordered channels have no place in two");
    assert!(
        matches!(err, Error::InvalidConfig(ConfigError::Placement(_))),
        "{err:?}"
    );
    assert!(!path.exists(), "nothing was written");
}

/// Planes already at the file's width are written as they are, whatever the
/// topology says.
#[test]
fn planes_at_the_files_width_are_written_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("same.wav");
    write_buffers(
        &rendered(&[0.25, -0.5]),
        &config(ChannelLayout::STEREO, None),
        &path,
    )
    .unwrap();
    assert_eq!(first_frame(&path, 2), vec![0.25, -0.5]);
}
