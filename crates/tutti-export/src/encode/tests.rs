use super::*;
use crate::{Job, Resample};
use tutti_codec::{Format, Pcm};

/// What these tests vary: a 1 kHz, eight-frame render into a WAV of `pcm`.
struct Case {
    rate: SampleRate,
    channels: ChannelLayout,
    pcm: Pcm,
    topology: Option<tutti_types::ChannelTopology>,
    resample: Option<Resample>,
}

impl Case {
    fn plan(&self) -> RenderPlan {
        RenderPlan::of(&Job::new(self.rate, Samples(8), Samples(0), Samples(0)).unwrap())
    }

    fn target(&self, path: &Path) -> FileTarget {
        FileTarget::new(path, Format::Wav(self.pcm), self.channels)
            .with_topology(self.topology.clone())
            .with_resample(self.resample)
    }
}

fn config(channels: u16, pcm: Pcm) -> Case {
    Case {
        rate: SampleRate::new(1_000.0),
        channels: ChannelLayout::from(channels),
        pcm,
        topology: None,
        resample: None,
    }
}

/// A plan that cannot fit a WAV file is refused before any of it is rendered
/// and before the path is touched: bytes already at the path survive.
///
/// The plan is real: 2^32 mono frames at 16 bits is 8 GiB, past the format's
/// 32-bit sizes, and no frame is written because the refusal is made before
/// the file is opened.
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
    let rate = config.rate;
    let path = dir.path().join("keep.wav");
    std::fs::write(&path, b"sentinel").unwrap();
    let err = open_file(&config.target(&path), &plan, rate)
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
#[test]
fn the_limit_is_judged_at_the_output_rate() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.wav");
    let mut config = config(1, Pcm::Int16);
    let frames = Samples((1 << 31) - 1_000);
    let plan = RenderPlan {
        total: frames,
        output_length: frames,
        latency: Samples(0),
    };
    let rate = config.rate;
    open_file(&config.target(&path), &plan, rate)
        .map(drop)
        .expect("2^31 - 1000 mono 16-bit frames fit");
    config.resample = Some(Resample::to(SampleRate::new(2_000.0)));
    let err = open_file(&config.target(&path), &plan, rate)
        .map(drop)
        .expect_err("twice the frames do not");
    assert!(
        matches!(err, Error::Encode(tutti_codec::Error::WavTooLarge { .. })),
        "{err}"
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
        cfg.topology = topology;
        let path = dir.path().join(format!("m{channels}_{mask}.wav"));
        let layout = ChannelLayout::from(channels);
        let mut chain = open_file(&cfg.target(&path), &cfg.plan(), cfg.rate).unwrap();
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
    cfg.topology = Some(tutti_types::ChannelTopology::quad());
    let path = dir.path().join("bad.wav");
    let err = open_file(&cfg.target(&path), &cfg.plan(), cfg.rate)
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
