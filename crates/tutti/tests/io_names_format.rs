//! `FileOut` writes a `Format`, which `tutti::io` does not re-export: a build
//! with `io` (or `audio-io`) and no format feature must still name one, and
//! build a WAV sink from it.
//!
//! Compiled only with `io`; `cargo xtask check-features` and CI's `dark features` job
//! run it with `audio-io`. Mutation (run): `io = [.., "codec"]` without
//! `tutti-codec/encode-wav` leaves `Format` with no value, so `Format::Wav` fails
//! to compile; without `codec` the file fails to compile.
#![cfg(feature = "io")]

use tutti::codec::{FileOut, Format, Pcm, Spec};
use tutti::types::{ChannelLayout, SampleRate, Sink};

#[test]
fn io_reaches_a_wav_sink_without_a_format_feature() {
    let dir = tempfile::tempdir().expect("temp dir");
    let spec = Spec::new(
        Format::Wav(Pcm::Int16),
        ChannelLayout::STEREO,
        SampleRate(48_000.0),
    );
    let sink = FileOut::create(dir.path().join("take.wav"), &spec);
    sink.expect("a WAV sink opens").finalize().expect("closes");
}
