//! What `Error::UnsupportedFormat` carries, so a caller can match it.

use std::path::Path;
use tutti_core::SampleRate;
use tutti_export::{AudioFormat, Error, Rendered, UnsupportedReason};

fn unsupported(r: tutti_export::Result<AudioFormat>) -> (String, UnsupportedReason) {
    match r {
        Err(Error::UnsupportedFormat { format, reason }) => (format, reason),
        other => panic!("expected UnsupportedFormat, got {other:?}"),
    }
}

// Mutation: have `from_path` skip `to_lowercase`, or put "" in `format` for a
// path with no extension; the first and second assertions fail.
#[test]
fn an_unknown_extension_is_named_lowercased() {
    let (format, reason) = unsupported(AudioFormat::from_path(Path::new("x.XYZ")));
    assert_eq!(format, "xyz");
    assert_eq!(reason, UnsupportedReason::UnknownExtension);
}

#[test]
fn a_path_with_no_extension_names_none() {
    let (format, reason) = unsupported(AudioFormat::from_path(Path::new("x")));
    assert_eq!(format, "(none)");
    assert_eq!(reason, UnsupportedReason::UnknownExtension);
}

// Mutation: have the FLAC encoder accept `Float32`, or report another reason;
// this fails.
#[cfg(feature = "flac")]
#[test]
fn flac_refuses_float32_with_the_bit_depth_reason() {
    use tutti_export::{BitDepth, EncodeConfig, ExportConfig, write_buffers};
    let rendered = Rendered {
        planes: vec![vec![0.0; 64]; 2],
        sample_rate: SampleRate(44_100.0),
    };
    let config = ExportConfig {
        encode: EncodeConfig {
            format: AudioFormat::Flac(Default::default()),
            bit_depth: BitDepth::Float32,
            ..Default::default()
        },
        ..Default::default()
    };
    let dir = tempfile::tempdir().unwrap();
    match write_buffers(&rendered, &config, dir.path().join("x.flac")) {
        Err(Error::UnsupportedFormat { format, reason }) => {
            assert_eq!(format, "FLAC");
            assert_eq!(reason, UnsupportedReason::BitDepth);
        }
        other => panic!("expected UnsupportedFormat, got {other:?}"),
    }
}
