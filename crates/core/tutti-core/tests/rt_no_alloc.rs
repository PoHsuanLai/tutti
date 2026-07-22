//! Regression gate for RT-safety: metering's audio-thread update path
//! must not allocate. Covers `MeteringManager::update_rt`, which is
//! called from the CPAL callback every buffer, and the LUFS publish
//! step that writes into the `AtomicLufs` snapshot.

use assert_no_alloc::AllocDisabler;
use std::time::Duration;
use tutti_core::{MeteringContext, MeteringManager};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

fn interleaved_stereo(frames: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(frames * 2);
    for i in 0..frames {
        let s = (i as f32 / frames as f32).sin();
        out.push(s);
        out.push(s * 0.5);
    }
    out
}

#[test]
fn update_rt_is_allocation_free_with_amp_and_corr() {
    let mgr = MeteringManager::new(48_000.0);
    mgr.enable_amp();
    mgr.enable_corr();

    let buffer = interleaved_stereo(512);
    let mut ctx = MeteringContext::new();
    let elapsed = Duration::from_micros(100);

    mgr.update_rt(&buffer, 512, elapsed, &mut ctx);

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1_000 {
            mgr.update_rt(&buffer, 512, elapsed, &mut ctx);
        }
    });
}

#[test]
fn update_rt_is_allocation_free_with_lufs_enabled() {
    // With `Mode::HISTOGRAM` set in `MeteringManager::new`, the ebur128
    // per-block history is a fixed `Box<[u64; 1000]>` — no VecDeque
    // growth, no `add_frames_*` allocation on steady state.
    //
    // This also covers the short-term 3-second window: we run enough
    // buffers (10_000 × 512 frames at 48 kHz ≈ 107 s of audio) for the
    // short-term path to take its periodic `short_term_block_energy_history.add`
    // branch many times, exercising the histogram add inside the harness.
    let mgr = MeteringManager::new(48_000.0);
    mgr.enable_amp();
    mgr.enable_corr();
    mgr.enable_lufs();

    let buffer = interleaved_stereo(512);
    let mut ctx = MeteringContext::new();
    let elapsed = Duration::from_micros(100);

    // Warm-up: prime the `AtomicLufs` snapshot and let ebur128 settle
    // past its first integrated block.
    for _ in 0..16 {
        mgr.update_rt(&buffer, 512, elapsed, &mut ctx);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..10_000 {
            mgr.update_rt(&buffer, 512, elapsed, &mut ctx);
        }
    });
}

#[test]
fn update_rt_is_allocation_free_with_none_enabled() {
    let mgr = MeteringManager::new(48_000.0);
    // All meters disabled by default — update_rt should be a no-op
    // beyond the CPU counter.
    let buffer = interleaved_stereo(256);
    let mut ctx = MeteringContext::new();
    let elapsed = Duration::from_micros(50);

    mgr.update_rt(&buffer, 256, elapsed, &mut ctx);

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1_000 {
            mgr.update_rt(&buffer, 256, elapsed, &mut ctx);
        }
    });
}

#[test]
fn lufs_snapshot_read_is_allocation_free() {
    let mgr = MeteringManager::new(48_000.0);
    mgr.enable_lufs();

    let buffer = interleaved_stereo(1024);
    let mut ctx = MeteringContext::new();
    // Run enough to populate the snapshot.
    for _ in 0..8 {
        mgr.update_rt(&buffer, 1024, Duration::from_micros(500), &mut ctx);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..10_000 {
            let _ = mgr.lufs();
            let _ = mgr.lufs_short();
            let _ = mgr.lufs_range();
            let _ = mgr.true_peak(0);
            let _ = mgr.true_peak(1);
        }
    });
}
