use super::*;
use crate::test_sink::Full;
use crate::{Error, SpecError};
use std::io::Cursor;
use tutti_types::SampleRate;

/// Every format this build writes, at 16 bits where it has a width, with the
/// name its file would carry.
#[allow(clippy::vec_init_then_push)]
fn formats() -> Vec<(&'static str, Format)> {
    #[allow(unused_mut)]
    let mut all: Vec<(&'static str, Format)> = Vec::new();
    #[cfg(feature = "wav")]
    all.push(("wav", Format::Wav(crate::Pcm::Int16)));
    #[cfg(feature = "aiff")]
    all.push(("aiff", Format::Aiff(crate::Pcm::Int16)));
    #[cfg(feature = "flac")]
    all.push((
        "flac",
        Format::Flac {
            depth: crate::IntDepth::Int16,
            level: crate::FlacLevel::default(),
        },
    ));
    #[cfg(feature = "vorbis")]
    all.push((
        "ogg",
        Format::Vorbis {
            quality: crate::Quality::default(),
        },
    ));
    all
}

fn spec(format: Format, channels: u16) -> Spec {
    Spec::new(format, ChannelLayout::from(channels), SampleRate(44_100.0))
}

/// `frames` frames of a sawtooth under a hash, exact in binary and not
/// compressible to nothing.
fn saw(channels: u16, frames: usize) -> Vec<f32> {
    (0..frames * usize::from(channels))
        .map(|i| {
            let noise = ((i as u64 * i as u64 * 2_654_435_761) >> 11) % 32_768;
            (i % 128) as f32 / 128.0 - 0.5 + noise as f32 / 32_768.0 / 4.0
        })
        .collect()
}

/// The names in `dir`, sorted.
fn listing(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// A file written with `create_file` appears at its path only when `finish`
/// moves it there, and replaces what was there.
///
/// Mutation: `create_file` opening `path` itself (`File::create(path)`) puts
/// the new bytes at the path while the encoder is open and fails the
/// "untouched until finish" assertion; deleting the `staged.commit()` in
/// `finish` leaves the old bytes and fails the last one.
#[test]
fn a_file_appears_at_its_path_only_when_finished() {
    let dir = tempfile::tempdir().unwrap();
    for (ext, format) in formats() {
        let path = dir.path().join(format!("take.{ext}"));
        std::fs::write(&path, b"previous").unwrap();
        let spec = spec(format, 2);
        let mut encoder = Encoder::create_file(&spec, &path).unwrap();
        encoder
            .write(Interleaved::new(&saw(2, 5_000), spec.layout))
            .unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"previous",
            "{ext}: the path is untouched while the encoder is open"
        );
        encoder.finish().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.len() > 100, "{ext}: the finished file replaced it");
        assert_eq!(
            listing(dir.path()),
            [format!("take.{ext}")],
            "{ext}: no temporary left"
        );
        std::fs::remove_file(&path).unwrap();
    }
}

/// An encoder dropped before `finish`, by `abort` or after an error, removes
/// its file and leaves what was at the path alone.
///
/// Mutation: deleting the `remove_file` in `Staged::drop` leaves the temporary
/// and fails the listing; making `abort` call `finish` replaces the path and
/// fails the `previous` check.
#[test]
fn an_aborted_encoder_removes_its_file_and_leaves_the_path_alone() {
    let dir = tempfile::tempdir().unwrap();
    for (ext, format) in formats() {
        let path = dir.path().join(format!("take.{ext}"));
        std::fs::write(&path, b"previous").unwrap();
        let spec = spec(format, 2);

        let mut encoder = Encoder::create_file(&spec, &path).unwrap();
        encoder
            .write(Interleaved::new(&saw(2, 5_000), spec.layout))
            .unwrap();
        encoder.abort();

        let mut dropped = Encoder::create_file(&spec, &path).unwrap();
        dropped
            .write(Interleaved::new(&saw(2, 5_000), spec.layout))
            .unwrap();
        drop(dropped);

        assert_eq!(std::fs::read(&path).unwrap(), b"previous", "{ext}");
        assert_eq!(listing(dir.path()), [format!("take.{ext}")], "{ext}");
        std::fs::remove_file(&path).unwrap();
    }
}

/// A write that fails mid-take, and the abort that follows, leave the path as
/// it was and no temporary. The failure is the width check, which every format
/// shares; the full-disk case is below.
///
/// Mutation: letting the width check pass (comparing channel counts of the
/// wrong layout) writes the block and fails the `expect_err`.
#[test]
fn a_refused_block_leaves_nothing_behind_once_dropped() {
    let dir = tempfile::tempdir().unwrap();
    for (ext, format) in formats() {
        let path = dir.path().join(format!("take.{ext}"));
        let spec = spec(format, 2);
        let mut encoder = Encoder::create_file(&spec, &path).unwrap();
        let err = encoder
            .write(Interleaved::new(&saw(1, 100), ChannelLayout::MONO))
            .expect_err("a mono block is not stereo");
        assert!(
            matches!(
                err,
                Error::BlockWidth { expected, got }
                    if expected == ChannelLayout::STEREO && got == ChannelLayout::MONO
            ),
            "{ext}: {err}"
        );
        drop(encoder);
        assert!(
            listing(dir.path()).is_empty(),
            "{ext}: {:?}",
            listing(dir.path())
        );
    }
}

/// A spec the format refuses fails at `create`, before a file exists.
///
/// Mutation: deleting the `spec.channels()?` in `Encoder::create` lets a
/// zero-width spec through to the WAV header and fails the `ZeroChannels`
/// row; deleting the rate check in `Spec::header_rate` fails the rate row.
#[test]
fn a_spec_that_cannot_be_written_is_refused_without_a_file() {
    let dir = tempfile::tempdir().unwrap();
    for (ext, format) in formats() {
        let path = dir.path().join(format!("take.{ext}"));
        let zero_width = spec(format, 0);
        let err = Encoder::create_file(&zero_width, &path).expect_err("no channels");
        assert!(
            matches!(err, Error::Spec(SpecError::ZeroChannels)),
            "{ext}: {err}"
        );
        let zero_rate = Spec {
            rate: SampleRate(0.0),
            ..spec(format, 2)
        };
        let err = Encoder::create_file(&zero_rate, &path).expect_err("no rate");
        assert!(
            matches!(err, Error::Spec(SpecError::ZeroSampleRate)),
            "{ext}: {err}"
        );
        assert!(
            listing(dir.path()).is_empty(),
            "{ext}: a temporary was left"
        );
    }
}

/// Widths past a container's header are refused as spec errors where the
/// container counts them: AIFF in an `i16`, Ogg Vorbis in a `u8`.
///
/// Mutation: casting with `as` in place of `try_from` wraps 65 535 and 256 into
/// small counts and fails the match.
#[test]
fn a_width_past_the_containers_header_is_refused() {
    #[cfg(feature = "aiff")]
    {
        let s = spec(Format::Aiff(crate::Pcm::Int16), 40_000);
        let err = Encoder::create(&s, Cursor::new(Vec::new())).expect_err("past i16");
        assert!(
            matches!(
                err,
                Error::Spec(SpecError::TooManyChannels {
                    channels: 40_000,
                    max: 32_767
                })
            ),
            "{err}"
        );
    }
    #[cfg(feature = "vorbis")]
    {
        let s = spec(
            Format::Vorbis {
                quality: crate::Quality::default(),
            },
            256,
        );
        let err = Encoder::create(&s, Cursor::new(Vec::new())).expect_err("past u8");
        assert!(
            matches!(
                err,
                Error::Spec(SpecError::TooManyChannels {
                    channels: 256,
                    max: 255
                })
            ),
            "{err}"
        );
    }
}

/// A take that outgrows its sink never reports success: some write, or at
/// the latest `finish`, fails, and the failure is the sink's own.
///
/// The sink holds the headers (Ogg's are a few KiB) and little more. WAV, AIFF and FLAC fail on the
/// write that reaches the sink; Ogg Vorbis encodes on a thread, so its
/// failure may come with the next write or `finish`.
///
/// Mutation (run): ignoring the result of `out.write_all` in FLAC's
/// `FrameWriter::receive`, and of `write_all` in the WAV writer, lets that
/// format finish `Ok` and fails its row. Swallowing the error in AIFF's `write`
/// or Vorbis's thread survives: the sink's failure comes back from `finalize` or
/// `finish` instead, which this test allows ("by finish at the latest").
#[test]
fn a_take_that_outgrows_its_sink_fails_by_finish_at_the_latest() {
    for (ext, format) in formats() {
        let s = spec(format, 2);
        let mut encoder = Encoder::create(&s, Full::holding(6_000)).unwrap();
        let mut failed: Option<Error> = None;
        for _ in 0..40 {
            if let Err(e) = encoder.write(Interleaved::new(&saw(2, 4_096), s.layout)) {
                failed = Some(e);
                break;
            }
        }
        let outcome = match failed {
            Some(e) => Err(e),
            None => encoder.finish().map(drop),
        };
        let err = outcome.expect_err(&format!("{ext}: a 6000-byte sink cannot hold the take"));
        match err {
            Error::Io(e) => assert_eq!(e.kind(), std::io::ErrorKind::StorageFull, "{ext}"),
            // Codecs that wrap the sink's error in their own report.
            Error::Encoding { detail, .. } => {
                assert!(!detail.is_empty(), "{ext}: the report names the cause")
            }
            other => panic!("{ext}: expected the sink's failure, got {other}"),
        }
    }
}

/// An encoder is `Send`, so a recording can move it to its pump thread.
///
/// The property is a compile-time one, so there is no mutation to run:
/// holding the `VorbisEncoder` in `VorbisWriter` (built on the caller's thread)
/// makes this stop compiling.
#[test]
fn an_encoder_can_move_to_another_thread() {
    fn is_send<T: Send>() {}
    is_send::<Encoder<Cursor<Vec<u8>>>>();
    is_send::<Encoder<std::io::BufWriter<std::fs::File>>>();
}
