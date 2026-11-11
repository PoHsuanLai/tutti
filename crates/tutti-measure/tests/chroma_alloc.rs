//! A chroma step allocates nothing once its state holds the buffers.
//!
//! The crate carries no `assert_no_alloc` dev-dependency (see its Cargo.toml),
//! so this binary counts allocations with its own global allocator. It counts
//! per thread, so the harness's other threads do not show up in a count.

use tutti_dsp::{FftScratch, StftGeometry};
use tutti_fixtures::{ThreadCounting, allocations};
use tutti_measure::{ChromaConfig, ChromaState, Tuning, step_chroma};
use tutti_types::{Cents, SampleRate, Samples};

#[global_allocator]
static A: ThreadCounting = ThreadCounting;

/// After one step has built the buffers and planned the FFT, further steps, a
/// `reset` and a step, and a step under a config that differs only in tuning
/// allocate nothing. A different geometry rebuilds the buffers, which does.
#[test]
fn steps_after_the_first_do_not_allocate() {
    let geometry =
        StftGeometry::new(SampleRate::new(22_050.0), Samples(2048), Samples(512)).unwrap();
    let cfg = ChromaConfig::new(geometry).unwrap();
    let fixed = cfg.with_tuning(Tuning::Fixed(Cents(10.0))).unwrap();
    let frame: Vec<f32> = (0..2048)
        .map(|i| (std::f32::consts::TAU * 440.0 * i as f32 / 22_050.0).sin() * 0.5)
        .collect();
    let (mut state, mut fft) = (ChromaState::new(), FftScratch::new());

    let first = allocations(|| {
        step_chroma(&cfg, &mut state, &frame, &mut fft).unwrap();
    });
    assert!(first > 0, "the first step builds the buffers");

    let again = allocations(|| {
        step_chroma(&cfg, &mut state, &frame, &mut fft).unwrap();
    });
    assert_eq!(again, 0, "a second step");

    let reset = allocations(|| {
        state.reset();
        step_chroma(&cfg, &mut state, &frame, &mut fft).unwrap();
    });
    assert_eq!(reset, 0, "reset and a step");
    assert_eq!(state.frames_seen(), 1);

    let retuned = allocations(|| {
        step_chroma(&fixed, &mut state, &frame, &mut fft).unwrap();
    });
    assert_eq!(retuned, 0, "a tuning-only switch and a step");
    assert_eq!(state.frames_seen(), 1, "the switch reset the carry");

    let wider = StftGeometry::new(SampleRate::new(22_050.0), Samples(4096), Samples(512)).unwrap();
    let wide = ChromaConfig::new(wider).unwrap();
    let rebuilt = allocations(|| {
        step_chroma(&wide, &mut state, &vec![0.0; 4096], &mut fft).unwrap();
    });
    assert!(rebuilt > 0, "a new geometry rebuilds");
}
