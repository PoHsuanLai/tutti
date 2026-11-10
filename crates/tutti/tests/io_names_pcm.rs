//! `WavOut::create` takes a `Pcm`, which `tutti-io` does not re-export: a build
//! with `io` (or `audio-io`) and no format feature must still name it.
//!
//! Compiled only with `io`; `cargo xtask check-features` and CI's `dark features` job
//! run it with `audio-io`. Mutation (run): `io = ["dep:tutti-io"]` without
//! `codec` fails to compile this file.
#![cfg(feature = "io")]

use tutti::types::AudioOut;

#[test]
fn io_reaches_pcm_without_a_format_feature() {
    let dir = tempfile::tempdir().expect("temp dir");
    let sink = tutti::io::WavOut::create(
        dir.path().join("take.wav"),
        48_000.0,
        2u16,
        tutti::codec::Pcm::Int16,
    );
    sink.expect("a WAV sink opens").finalize().expect("closes");
}
