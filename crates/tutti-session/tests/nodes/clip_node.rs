//! `tutti_sampler::Clip`, a placed sample that is a graph node as it stands.

use tutti_graph::NodeKey;
use tutti_sampler::Clip;
use tutti_session::{GraphSource, Session};
use tutti_types::{Amplitude, Beat, ChannelLayout, InterleavedMut, MaxBlock, SampleRate, Samples};

const RATE: f64 = 48_000.0;

fn offline() -> Session {
    Session::builder()
        .build_offline(
            SampleRate(RATE),
            ChannelLayout::STEREO,
            MaxBlock::of(Samples(1_024)),
        )
        .expect("builds")
}

/// One second of a constant mono wave.
fn level(value: f32) -> tutti_codec::Wave {
    let mut wave = tutti_codec::Wave::new(1, SampleRate::new(RATE));
    for _ in 0..RATE as usize {
        wave.push_frame(&[value]);
    }
    wave
}

/// `frames` stereo frames of the session, the transport rolling.
fn run(s: &Session, frames: usize) -> Vec<f32> {
    s.play().expect("room");
    let mut out = Vec::new();
    let mut buf = vec![0.0f32; 512 * 2];
    while out.len() < frames * 2 {
        s.render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO));
        out.extend_from_slice(&buf);
    }
    out.truncate(frames * 2);
    out
}

fn wire(s: &mut Session, clip: NodeKey) {
    for port in 0..2 {
        s.graph()
            .set_output_source(port, GraphSource::Node(clip, port));
    }
}

/// **A clip is silent until its beat and plays its wave from it.** At 120 BPM
/// beat 1 is frame 24 000: the output is exactly zero before it, and holds
/// the wave's level (scaled by the clip's gain) on both channels from it.
#[test]
fn a_clip_starts_on_its_beat_at_its_gain() {
    let mut s = offline();
    let (clip, _) = s.graph().add(
        Clip::new(level(0.5))
            .and_then(|c| c.channels(ChannelLayout::STEREO))
            .expect("a placeable wave")
            .at(Beat(1.0))
            .gain(Amplitude::new(0.5)),
    );
    wire(&mut s, clip);
    s.update();
    let out = run(&s, 40_000);
    assert!(out[..2 * 24_000].iter().all(|&x| x == 0.0), "silent before");
    let after = out[2 * 24_000 + 2];
    assert!(after > 0.0, "audible from the beat");
    let first = out.iter().position(|&x| x != 0.0).expect("it plays") / 2;
    assert_eq!(first, 24_000, "on the frame of beat 1");
    // The clip's gain is a scalar on the wave's level: half of the unscaled.
    let (mut full, _) = (offline(), ());
    let (whole, _) = full.graph().add(
        Clip::new(level(0.5))
            .and_then(|c| c.channels(ChannelLayout::STEREO))
            .unwrap()
            .at(Beat(1.0)),
    );
    wire(&mut full, whole);
    full.update();
    let unscaled = run(&full, 40_000)[2 * 24_000 + 2];
    assert!(
        (after - unscaled * 0.5).abs() < 1e-6,
        "{after} vs {unscaled}"
    );
}

/// **A clip is the wave's own width until told another**: a mono wave is a
/// one-output node, a stereo wave a two-output one, and `channels` places it
/// at another width.
#[test]
fn a_clip_is_as_wide_as_its_wave_unless_placed() {
    let mut s = offline();
    let outputs = |s: &mut Session, clip: Clip| {
        let (node, _) = s.graph().add(clip);
        s.graph().buses(node).expect("in the graph").1.width()
    };
    assert_eq!(outputs(&mut s, Clip::new(level(0.5)).unwrap()), 1);
    let wide = Clip::new(level(0.5))
        .unwrap()
        .channels(ChannelLayout::STEREO)
        .unwrap();
    assert_eq!(outputs(&mut s, wide), 2);
}
