//! `FileOut` writes a `Format`, which `tutti::io` does not re-export: a build
//! with `io` (or `audio-io`) and no format feature must still name one, and
//! build a WAV sink from it.
//!
//! Compiled only with `io`.
#![cfg(feature = "io")]

use tutti::codec::{FileOut, Format, Pcm, Spec};
use tutti::types::{ChannelLayout, SampleRate, Sink};

#[test]
fn io_reaches_a_wav_sink_without_a_format_feature() {
    let dir = tempfile::tempdir().expect("temp dir");
    let spec = Spec::of_width(
        Format::Wav(Pcm::Int16),
        ChannelLayout::STEREO,
        SampleRate::SR_48K,
    );
    let sink = FileOut::create(&spec, dir.path().join("take.wav"));
    sink.expect("a WAV sink opens").finalize().expect("closes");
}
