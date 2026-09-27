//! A stretched memory voice has sound right after a seek.
//!
//! A seek flushes the stretch filter so pre-jump audio does not smear over
//! the new region, and a flushed filter is silent until it has taken in a
//! window of input again (a window times the effective stretch: about 4 100
//! frames at 2x). Without priming, every seek or loop wrap of a stretched
//! voice left ~85 ms of silence. `verify-audio`'s `seek_while_stretched` case heard
//! it; this pins it in the suite.

use std::f32::consts::TAU;
use std::sync::Arc;

use tutti_core::{Beat, Bpm, SampleRate, StretchFactor};
use tutti_graph::contract;
use tutti_io::Wave;
use tutti_sampler::testing::MockTransport;
use tutti_sampler::{MemorySource, Playback, SlotId, Voice, VoiceCommand, VoicePool, VoiceSource};

const SR: f64 = 48_000.0;
const BLOCK: usize = 64;

fn rms(x: &[f32]) -> f32 {
    (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt()
}

/// Left channel of a 2x-stretched 440 Hz voice, seeked to beat 5 at the
/// block starting on frame `seek_at`.
fn render(frames: usize, seek_at: usize) -> Vec<f32> {
    let (pool, handle) = VoicePool::new().with_handle();
    let mut pool = contract::prepared(pool, SampleRate(SR), BLOCK);
    let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
    let mut wave = Wave::new(1, SR);
    for i in 0..frames * 4 {
        wave.push_frame(&[(TAU * 440.0 * i as f32 / SR as f32).sin()]);
    }
    handle
        .send(VoiceCommand::AddVoice {
            id: SlotId(1),
            voice: Box::new(Voice {
                source: VoiceSource::Memory(MemorySource::placed(
                    Arc::new(wave),
                    Beat::new(0.0),
                    None,
                )),
                play: Playback {
                    stretch: StretchFactor::new(2.0),
                    ..Default::default()
                },
                channel_index: None,
            }),
            stretch: None,
        })
        .expect("the command queue has room in a test");

    let mut out = Vec::with_capacity(frames);
    while out.len() < frames {
        if out.len() == seek_at {
            transport.seek(Beat::new(5.0));
        }
        let block = contract::drive_in(&mut pool, &transport.env(BLOCK, SR), &[], &[], &[]).audio;
        out.extend_from_slice(&block[0]);
        transport.advance(BLOCK as i64, SR);
    }
    out
}

/// The first 2 048 frames after a seek are as loud as the steady state
/// before it (within 3 dB), not the flushed filter's silence.
///
/// Mutations: drop `prime_stretch`'s call in `PlaybackSlot::render_into` →
/// the post-seek window's RMS is 0 (the filter is still refilling) and this
/// fails; prime only `latency_samples` frames (a window, not a window times
/// the stretch) → the first loud frame is 2 054 after the seek, RMS 0, fails.
#[test]
fn a_stretched_voice_is_audible_right_after_a_seek() {
    let seek_at = 375 * BLOCK; // 24 000
    let out = render(32_000, seek_at);
    let before = rms(&out[seek_at - 8_192..seek_at]);
    let after = rms(&out[seek_at..seek_at + 2_048]);
    assert!(before > 0.3, "the steady state is audible: rms {before}");
    assert!(
        after > before * 0.707,
        "silent after the seek: rms {after} against {before} before it"
    );
}
