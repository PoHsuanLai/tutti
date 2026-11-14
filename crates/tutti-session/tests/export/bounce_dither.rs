//! A bounce's dither: triangular unless it says otherwise, and none when it
//! says none.
//!
//! A constant that falls between two 16-bit steps shows what each setting
//! does to the quantization: undithered, every sample is the plain rounding;
//! dithered, the samples scatter around it by at most the shape's peak.

use tutti_codec::{Format, IntDepth, Pcm};
use tutti_graph::ForkByClone;
use tutti_nodes::testing::Const;
use tutti_session::{Bounce, DitherShape, Range, Root, Session};
use tutti_types::{ChannelLayout, SampleRate, Seconds};

const LEVEL: f32 = 0.3;

/// The 16-bit samples of a bounce of a constant `LEVEL`, as `configure` sets
/// its dither.
fn bounced(
    configure: impl FnOnce(
        Bounce<tutti_graph::NodeKey, tutti_session::FileMaster>,
    ) -> Bounce<tutti_graph::NodeKey, tutti_session::FileMaster>,
) -> Vec<i32> {
    let mut session = Session::builder()
        .inputs(ChannelLayout::EMPTY)
        .outputs_floor(ChannelLayout::STEREO)
        .build_detached(SampleRate::SR_48K)
        .expect("a detached session builds");
    let (dc, _) = session
        .graph()
        .add(ForkByClone(Const::new(LEVEL, ChannelLayout::STEREO)));
    session.graph().connect(dc, Root).unwrap().hard();
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("out.wav");
    let bounce = Bounce::project()
        .range(Range::Seconds(Seconds(0.0), Seconds(0.1)))
        .master(&path, Format::Wav(Pcm::Int16))
        .tail(tutti_session::BounceTail::None);
    session.bounce(configure(bounce)).expect("renders");
    hound::WavReader::open(&path)
        .expect("readable")
        .samples::<i16>()
        .map(|s| i32::from(s.expect("a sample")))
        .collect()
}

/// **`no_dither` leaves the plain rounding.** Every sample is the quantization
/// of the level, with nothing added.
#[test]
fn no_dither_writes_the_plain_quantization() {
    let plain = IntDepth::Int16.quantize(LEVEL);
    let samples = bounced(Bounce::no_dither);
    assert!(!samples.is_empty());
    assert!(samples.iter().all(|&s| s == plain), "{:?}", &samples[..8]);
}

/// The mean squared distance of `samples` from the exact level, in steps.
fn error_power(samples: &[i32]) -> f64 {
    let exact = f64::from(LEVEL) * 32_767.0;
    samples
        .iter()
        .map(|&s| (f64::from(s) - exact).powi(2))
        .sum::<f64>()
        / samples.len() as f64
}

/// **Triangular is the default, with the error power triangular dither has.**
/// Triangular dither on top of rounding leaves a mean squared error of 1/4
/// step^2 whatever the level (1/12 for the rounding, 1/6 for the dither's own
/// variance), which neither the plain rounding of a level off the step (0.01
/// here) nor rectangular dither (under 1/6 at this level) has. A rectangular
/// dither moves a sample by at most one step.
#[test]
fn the_default_dither_is_triangular() {
    let default = bounced(|b| b);
    assert!(
        (error_power(&default) - 0.25).abs() < 0.02,
        "{}",
        error_power(&default)
    );

    let plain = IntDepth::Int16.quantize(LEVEL);
    let rectangular = bounced(|b| b.dither(DitherShape::Rectangular));
    assert!(rectangular.iter().any(|&s| s != plain), "it dithers");
    assert!(rectangular.iter().all(|&s| (s - plain).abs() <= 1));
    assert!(
        error_power(&rectangular) < 0.2,
        "{}",
        error_power(&rectangular)
    );
}
