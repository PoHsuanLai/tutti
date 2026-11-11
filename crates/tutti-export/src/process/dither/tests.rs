use super::*;
use crate::{ChannelLayout, Format, Pcm, Quality};
use tutti_dsp::DitherShape;

fn config(dither: Option<DitherShape>, format: Format) -> FileTarget {
    FileTarget::new("unused.wav", format, ChannelLayout::STEREO).with_dither(dither)
}

fn wav(pcm: Pcm) -> Format {
    Format::Wav(pcm)
}

/// At `Float32` there is nothing to add: no spec, so the sink is the
/// identity. Mutation (run): drop the `?` on `int_depth` (use `unwrap_or`
/// `Int24`) and a spec appears.
#[test]
fn float32_is_never_dithered() {
    assert_eq!(
        dither_spec(&config(Some(DitherShape::Triangular), wav(Pcm::Float32))),
        None
    );
}

/// An Ogg export with dither on adds nothing.
///
/// Mutation (run): make `Format::int_depth` return `Some(IntDepth::default())`
/// for `Vorbis` and a spec appears.
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

/// Mutation (run): default `None` to `Triangular` in `dither_spec` and a spec appears.
#[test]
fn off_has_no_spec() {
    assert_eq!(dither_spec(&config(None, wav(Pcm::Int16))), None);
}

/// The shape follows the option, and the LSB the depth: 16-bit is 2^-15 and
/// 24-bit is 2^-23, derived from the depth, not read back from the
/// implementation. Mutation (run): `dither_spec` always naming `Triangular` makes the rectangular row fail.
#[test]
fn the_spec_matches_the_option_and_the_depth() {
    let t16 = dither_spec(&config(Some(DitherShape::Triangular), wav(Pcm::Int16))).unwrap();
    assert_eq!(t16.shape, DitherShape::Triangular);
    assert_eq!(t16.lsb.0, 1.0 / 32768.0);
    let r24 = dither_spec(&config(Some(DitherShape::Rectangular), wav(Pcm::Int24))).unwrap();
    assert_eq!(r24.shape, DitherShape::Rectangular);
    assert_eq!(r24.lsb.0, 1.0 / 8_388_608.0);
}
