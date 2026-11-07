use super::*;
use tutti_fixtures::{RangeContract, check_range_contract};

/// `Quality` keeps the range-checked contract: `new`, `TryFrom`, `of` and
/// serde agree, and every raw outside `0.0..=1.0`, NaN and infinity included,
/// is refused.
///
/// Mutation: making `new` accept `raw <= 1.5` fails the `1.25` row; replacing
/// the range test with `raw >= 0.0 && raw <= 1.0` is the same and passes;
/// dropping `try_from`/`into` from the serde attribute lets `-1.0` through and
/// fails the "deserialize accepted" check.
#[test]
fn quality_keeps_the_range_contract() {
    check_range_contract(&RangeContract {
        new: Quality::new,
        get: Quality::get,
        of: Some(Quality::of),
        valid: &[0.0, 0.25, 0.5, 1.0],
        invalid: &[-0.1, 1.25, f32::NAN, f32::INFINITY, f32::NEG_INFINITY],
    });
}

#[test]
fn quality_defaults_to_the_middle() {
    assert_eq!(Quality::default().get(), 0.5);
    assert_eq!(Quality::LOWEST.get(), 0.0);
    assert_eq!(Quality::HIGHEST.get(), 1.0);
}

/// An extension names its format with the defaults, case and `aif` aside; an
/// unknown one is named lowercased, and a missing one is `(none)`.
///
/// Mutation: skipping `to_lowercase`, or putting "" in `extension` for a path
/// with none, fails the second and third assertions.
#[test]
fn extensions_name_formats_and_refuse_the_rest() {
    let name = |r: Result<Format>| match r {
        Err(Error::UnknownExtension { extension }) => extension,
        other => panic!("expected UnknownExtension, got {other:?}"),
    };
    assert_eq!(name(Format::from_path("x.XYZ")), "xyz");
    assert_eq!(name(Format::from_path("x")), "(none)");
    #[cfg(feature = "wav")]
    assert_eq!(
        Format::from_path("a/b.WAV").unwrap(),
        Format::Wav(Pcm::Int24)
    );
    #[cfg(feature = "aiff")]
    assert_eq!(
        Format::from_extension("aif").unwrap(),
        Format::Aiff(Pcm::Int24)
    );
    #[cfg(feature = "flac")]
    assert_eq!(
        Format::from_extension("flac").unwrap(),
        Format::Flac {
            depth: IntDepth::Int24,
            level: FlacLevel::Standard
        }
    );
    #[cfg(feature = "vorbis")]
    assert_eq!(
        Format::from_extension("ogg").unwrap(),
        Format::Vorbis {
            quality: Quality::of(0.5)
        }
    );
}

/// Only a format that quantizes names an integer width, and only WAV bounds
/// its length.
///
/// Mutation: `Aiff(pcm) => Some(IntDepth::default())` fails the float row.
#[test]
fn the_integer_width_and_the_length_bound_follow_the_format() {
    #[cfg(feature = "wav")]
    {
        assert_eq!(Format::Wav(Pcm::Int16).int_depth(), Some(IntDepth::Int16));
        assert_eq!(Format::Wav(Pcm::Float32).int_depth(), None);
        assert!(
            Format::Wav(Pcm::Int16)
                .max_frames(ChannelLayout::STEREO)
                .is_some()
        );
    }
    #[cfg(feature = "aiff")]
    {
        assert_eq!(Format::Aiff(Pcm::Float32).int_depth(), None);
        assert_eq!(
            Format::Aiff(Pcm::Int24).max_frames(ChannelLayout::STEREO),
            None
        );
    }
    #[cfg(feature = "flac")]
    assert_eq!(
        Format::Flac {
            depth: IntDepth::Int16,
            level: FlacLevel::Best
        }
        .int_depth(),
        Some(IntDepth::Int16)
    );
    #[cfg(feature = "vorbis")]
    assert_eq!(
        Format::Vorbis {
            quality: Quality::default()
        }
        .int_depth(),
        None
    );
}
