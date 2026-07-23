//! Regression gate for `Engine::process` over a real DSP chain.
//!
//! The existing `process_audio_is_allocation_free` test in `tutti/src/audio_io.rs`
//! only exercises the bare `TransportClock` — no signal nodes. This test
//! pushes an oscillator → bell EQ → limiter chain into the net so the
//! gate covers the per-buffer hot path through actual fundsp `AudioUnit`
//! impls: `Sine`, `FixedSvf` (bell), and `Limiter`. A regression in any
//! of those (or in `Net`'s vertex iteration) shows up here as an
//! allocation panic.

use assert_no_alloc::AllocDisabler;
use parking_lot::Mutex;
use tutti_core::dsp::{bell_hz, limiter_stereo, pan, sine_hz, AudioUnit};
use tutti_core::engine::Engine;
use tutti_core::{dsp::Net, SampleRate, Transport, TransportClock};

use std::sync::Arc;

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

/// Build an [`Engine`] whose net is `sine → pan → bell EQ → limiter`.
/// The transport-clock node is also pushed so transport advancement runs
/// through the same `process` path.
fn build_engine_with_chain() -> Engine {
    let sample_rate = 48_000.0;
    let transport = Transport::new(sample_rate);

    let mut net = Net::new(0, 2);

    // Transport clock — matches what every real Engine sees.
    let clock = TransportClock::from_inputs(transport.clock_inputs(), sample_rate)
        .with_position_writeback(Arc::clone(&transport.settings.beat));
    net.push(Box::new(clock));

    // sine → pan → bell → limiter, wired with `chain` so each node feeds
    // the next and the final output drives both stereo channels.
    let inner = &mut net;
    inner.chain(Box::new(sine_hz::<f32>(440.0)));
    inner.chain(Box::new(pan(0.0)));
    inner.chain(Box::new(bell_hz::<f32>(1_000.0, 1.0, 6.0)));
    inner.chain(Box::new(limiter_stereo(0.005, 0.050)));

    inner.set_sample_rate(SampleRate(sample_rate));
    let backend = net.backend();

    // The backend holds a pointer back into the net; keep it alive.
    let _keep: &'static Mutex<Net> = Box::leak(Box::new(Mutex::new(net)));

    Engine::new(transport.motion.clone(), backend)
}

#[test]
fn engine_process_real_chain_is_allocation_free() {
    let engine = build_engine_with_chain();

    let mut output = vec![0.0f32; 512 * 2];

    // Warm up outside the gate — prime any first-call state on the
    // limiter / svf filters and the transport clock.
    for _ in 0..16 {
        engine.process(&mut output, 512);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1_000 {
            engine.process(&mut output, 512);
        }
    });
}

#[test]
fn engine_process_real_chain_small_buffer_is_allocation_free() {
    // Small buffers stress the per-buffer setup overhead.
    let engine = build_engine_with_chain();

    let mut output = vec![0.0f32; 64 * 2];
    for _ in 0..16 {
        engine.process(&mut output, 64);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..5_000 {
            engine.process(&mut output, 64);
        }
    });
}
