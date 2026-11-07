//! The cases every [`FormatWriter`] must pass. A new format is one more
//! `contract!` line and inherits all of them.

use crate::test_sink::Full;
use crate::{Format, FormatWriter, Result, Spec};
use std::io::{Cursor, Seek, Write};
use tutti_types::{ChannelLayout, Interleaved, SampleRate};

fn spec(format: Format) -> Spec {
    Spec::new(format, ChannelLayout::STEREO, SampleRate(44_100.0))
}

/// `frames` stereo frames of a sawtooth under a hash: exact in binary and not
/// compressible to nothing.
fn input(frames: usize) -> Vec<f32> {
    (0..frames * 2)
        .map(|i| {
            let noise = ((i as u64 * i as u64 * 2_654_435_761) >> 11) % 32_768;
            (i % 128) as f32 / 128.0 - 0.5 + noise as f32 / 32_768.0 / 4.0
        })
        .collect()
}

fn block(samples: &[f32]) -> Interleaved<'_> {
    Interleaved::new(samples, ChannelLayout::STEREO)
}

/// Runs the cases against one format. `make` starts a writer of it in a sink;
/// `deterministic` is whether the same input always gives the same bytes (Ogg
/// stamps a random stream serial, so it does not).
macro_rules! contract {
    ($name:ident, $format:expr, $deterministic:expr, $make:expr) => {
        mod $name {
            use super::*;

            fn make<S: Write + Seek + Send + 'static>(
                sink: S,
            ) -> Result<impl FormatWriter<Sink = S>> {
                ($make)(&spec($format), sink)
            }

            fn file(blocks: &[&[f32]]) -> Vec<u8> {
                let mut w = make(Cursor::new(Vec::new())).unwrap();
                for b in blocks {
                    w.write(block(b)).unwrap();
                }
                w.finish().unwrap().into_inner()
            }

            /// A take cut into blocks of any size, empty ones included, is the
            /// file the whole take is (Ogg: still a finished, non-empty stream,
            /// the empty block not ending it).
            ///
            /// Mutation: a writer that encodes only the first block of a cut
            /// take, or that drops a block's tail, changes its bytes and fails
            /// the equality (Ogg: a vorbis writer that forwards the empty block
            /// ends the stream early and shrinks the file below the whole
            /// take's).
            #[test]
            fn blocks_of_any_size_make_the_same_file() {
                let all = input(10_000);
                let whole = file(&[&all]);
                let cut = file(&[&all[..2], &[], &all[2..9_000], &all[9_000..]]);
                if $deterministic {
                    assert_eq!(whole, cut);
                } else {
                    assert!(cut.len() > 1_000, "a finished stream");
                    let (a, b) = (whole.len() as f64, cut.len() as f64);
                    assert!((a - b).abs() / a < 0.1, "{a} bytes whole, {b} cut");
                }
            }

            /// A take of no frames is a file: the header at least.
            ///
            /// Mutation: a writer whose `create` writes no header, or whose
            /// `finish` skips the final flush, hands back an empty sink and
            /// fails the length check.
            #[test]
            fn zero_frames_is_a_file() {
                let sink = make(Cursor::new(Vec::new())).unwrap().finish().unwrap();
                assert!(
                    sink.get_ref().len() > 20,
                    "a header, got {}",
                    sink.get_ref().len()
                );
            }

            /// `finish` hands the sink back positioned at the end of the file,
            /// after the header it patched at the start.
            ///
            /// Mutation: removing the `seek(End(0))` from the WAV writer's
            /// `finish` (the header patch leaves the position at 44) or the
            /// FLAC writer's fails this.
            #[test]
            fn finish_hands_the_sink_back_at_the_end() {
                let mut w = make(Cursor::new(Vec::new())).unwrap();
                w.write(block(&input(3_000))).unwrap();
                let mut sink = w.finish().unwrap();
                let len = sink.get_ref().len() as u64;
                assert_eq!(sink.stream_position().unwrap(), len);
            }

            /// A sink that fills up never lets a take report success: a write,
            /// or at the latest `finish`, fails.
            ///
            /// Mutation: swallowing a sink error in the writer's `write` and
            /// in its `finish` lets the take finish `Ok` and fails this.
            #[test]
            fn a_full_sink_fails_the_take() {
                let mut w = make(Full::holding(6_000)).unwrap();
                let samples = input(4_096);
                let mut failed = false;
                for _ in 0..40 {
                    if w.write(block(&samples)).is_err() {
                        failed = true;
                        break;
                    }
                }
                assert!(
                    failed || w.finish().is_err(),
                    "a 6000-byte sink held the take"
                );
            }

            /// A writer dropped before `finish` (an abort) returns, with
            /// whatever threads it started stopped, rather than hanging.
            ///
            /// `finish` takes the writer by value, so closing a file twice, or
            /// writing after it, does not compile.
            ///
            /// Mutation: a FLAC or Vorbis writer whose drop waits on a worker
            /// that waits on the queue the drop still holds open never returns
            /// and fails the bound.
            #[test]
            fn a_dropped_writer_stops() {
                let (done, finished) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let mut w = make(Cursor::new(Vec::new())).unwrap();
                    w.write(block(&input(9_000))).unwrap();
                    drop(w);
                    let _ = done.send(());
                });
                finished
                    .recv_timeout(std::time::Duration::from_secs(30))
                    .expect("the writer did not stop");
            }
        }
    };
}

#[cfg(feature = "wav")]
contract!(
    wav,
    Format::Wav(crate::Pcm::Int24),
    true,
    |spec: &Spec, sink| crate::WavWriter::create(spec, crate::Pcm::Int24, sink)
);

#[cfg(feature = "aiff")]
contract!(
    aiff,
    Format::Aiff(crate::Pcm::Int16),
    true,
    |spec: &Spec, sink| crate::AiffWriter::create(spec, crate::Pcm::Int16, sink)
);

#[cfg(feature = "flac")]
contract!(
    flac,
    Format::Flac {
        depth: crate::IntDepth::Int16,
        level: crate::FlacLevel::Standard
    },
    true,
    |spec: &Spec, sink| crate::FlacWriter::create(
        spec,
        crate::IntDepth::Int16,
        crate::FlacLevel::Standard,
        sink
    )
);

#[cfg(feature = "vorbis")]
contract!(
    vorbis,
    Format::Vorbis {
        quality: crate::Quality::LOWEST
    },
    false,
    |spec: &Spec, sink| crate::VorbisWriter::create(spec, crate::Quality::LOWEST, sink)
);
