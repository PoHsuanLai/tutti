use super::*;
use tutti_codec::{Format, Pcm, Quality};
use tutti_dsp::DitherShape;
use tutti_types::ChannelLayout;

fn config(dither: Option<DitherShape>, format: Format) -> FileTarget {
    FileTarget::new("unused.wav", format, ChannelLayout::STEREO).with_dither(dither)
}

fn wav(pcm: Pcm) -> Format {
    Format::Wav(pcm)
}

/// At `Float32` there is nothing to add: no spec, so the sink is the
/// identity.
#[test]
fn float32_is_never_dithered() {
    assert_eq!(
        dither_spec(&config(Some(DitherShape::Triangular), wav(Pcm::Float32))),
        None
    );
}

/// An Ogg export with dither on adds nothing.
#[test]
fn ogg_is_never_dithered() {
    let cfg = config(
        Some(DitherShape::Triangular),
        Format::Vorbis {
            quality: Quality::default(),
        },
    );
    assert_eq!(dither_spec(&cfg), None);
}

#[test]
fn off_has_no_spec() {
    assert_eq!(dither_spec(&config(None, wav(Pcm::Int16))), None);
}

/// The shape follows the option, and the LSB the depth: 16-bit is 2^-15 and
/// 24-bit is 2^-23, derived from the depth, not read back from the
/// implementation.
#[test]
fn the_spec_matches_the_option_and_the_depth() {
    let t16 = dither_spec(&config(Some(DitherShape::Triangular), wav(Pcm::Int16))).unwrap();
    assert_eq!(t16.shape, DitherShape::Triangular);
    assert_eq!(t16.lsb.0, 1.0 / 32768.0);
    let r24 = dither_spec(&config(Some(DitherShape::Rectangular), wav(Pcm::Int24))).unwrap();
    assert_eq!(r24.shape, DitherShape::Rectangular);
    assert_eq!(r24.lsb.0, 1.0 / 8_388_608.0);
}
