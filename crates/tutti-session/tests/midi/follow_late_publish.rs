//! A late publish: a map whose step is
//! behind the playhead moves no beat.

use tutti_buffers::InterleavedMut;
use tutti_session::Session;
use tutti_tempo::{TempoMap, TempoPoint};
use tutti_transport::MotionEvent;
use tutti_types::{Beat, Bpm, ChannelLayout, SampleRate};

const RATE: f64 = 48_000.0;
const BLOCK: usize = 512;

/// **The beat stays continuous when a published step is behind the playhead.**
/// The transport plays at 120 bpm to beat ~8, then a map with a step to 60 bpm at
/// beat 2 is published: a step the walker has long rendered past. The clock
/// re-seats at its current beat on the new map (`tempo_for_block`), so the beat
/// at the start of each block, read from what the engine renders, advances by
/// the block's beats at the tempo in force and never jumps: the block that picks
/// the map up already runs at 60 bpm from where the beat was, and no block
/// advances by more than a block at the old tempo.
#[test]
fn a_step_published_behind_the_playhead_moves_no_beat() {
    let s = Session::builder()
        .sample_rate(SampleRate::of(RATE))
        .build()
        .expect("builds");
    let mut buf = vec![0.0f32; BLOCK * 2];
    let mut block = |s: &Session| {
        s.render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO).unwrap());
        s.playhead().beat().get()
    };

    s.transport()
        .motion()
        .try_send(MotionEvent::Play)
        .expect("room");
    // 120 bpm: a block is 512 / 48000 * 2 beats.
    let old_block = BLOCK as f64 / RATE * 2.0;
    let mut beats = vec![block(&s)];
    while *beats.last().unwrap() < 8.0 {
        beats.push(block(&s));
    }
    let published_at = beats.len();
    let behind = TempoMap::new([
        TempoPoint::step(Beat(0.0), Bpm::of(120.0)),
        TempoPoint::step(Beat(2.0), Bpm::of(60.0)),
    ])
    .unwrap();
    s.set_tempo_map(behind).expect("not following");
    for _ in 0..8 {
        beats.push(block(&s));
    }

    // The beat at the start of each block, and what each block advanced it by.
    let steps: Vec<f64> = beats.windows(2).map(|w| w[1] - w[0]).collect();
    let new_block = BLOCK as f64 / RATE;
    for (i, step) in steps.iter().enumerate() {
        assert!(
            *step >= 0.0 && *step <= old_block * (1.0 + 1e-9),
            "block {i} moved the beat by {step}, more than a block at the old tempo ({old_block})"
        );
        // `beats[i]` is the start of block `i`; the map was published after block
        // `published_at - 1`, so block `published_at` reads it and it and every
        // block after run at the new tempo: 60 bpm is one beat a second.
        let expected = if i >= published_at {
            new_block
        } else {
            old_block
        };
        assert!(
            (step - expected).abs() < 1e-9,
            "block {i} advanced {step}, expected {expected}"
        );
    }
    assert_eq!(s.playhead().tempo(), Bpm::of(60.0));
}
