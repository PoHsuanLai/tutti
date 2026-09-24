//! The allocation gate `lib.rs` and `output.rs` promise for [`process_audio`].
//!
//! Both crate docs state that the RT callback "must not allocate, lock, or
//! block", that its buffers are sized once at stream build to `MAX_FRAMES`,
//! and that `tests/rt_no_alloc.rs` gates that. This is that file.
//!
//! # Why it is here and not in `src/`
//!
//! `assert_no_alloc` observes nothing unless `#[global_allocator] =
//! AllocDisabler` is installed, and that can only be declared at the root of a
//! *binary*. The crate's unit-test binary declares none, so an
//! `assert_no_alloc` gate written in `src/` passes whether or not the callback
//! allocated — the exact failure
//! `tutti-midi-hardware/tests/rt_no_alloc_sysex.rs` documents having been
//! caught by mutation-testing.
//!
//! `src/output.rs` did carry three such gates plus a fixture assertion, and
//! all four were inert for that reason. They have since been deleted, so this
//! file is now the *only* allocation gate on `process_audio` — a test that
//! cannot fail is worse than no test, and keeping an inert copy beside a live
//! one invites reading a green `src/` run as coverage. **Add new allocation
//! gates here, never in `src/`.**
//!
//! # The fixture has to render
//!
//! A `Net` with nothing wired leaves every output edge on `Port::Zero`, so
//! `process_audio` folds silence without walking a single vertex — an
//! allocation gate around that proves nothing. The graph below is a sine
//! through a filter into both device channels, so the render path evaluates
//! real vertices, gathers per-vertex buffers, and folds to the device width.
//! `renders_something_to_gate` asserts that, so the gates cannot pass by
//! rendering nothing.

use assert_no_alloc::AllocDisabler;
use parking_lot::Mutex;
use tutti_core::dsp::Net;
use tutti_core::{AudioTap, Engine, Hz, MasterMeter, Q};
use tutti_core::{ChannelLayout, InterleavedMut};
use tutti_core::{MotionEvent, Transport, TransportClock};
use tutti_cpal::{process_audio, AudioCallbackState, MAX_FRAMES};
use tutti_nodes::testing::Osc;
use tutti_nodes::{SvfFilterNode, SvfType};

// Without this every `assert_no_alloc` below is a silent no-op. See the module
// header — this is the whole reason the gate is an integration test.
#[global_allocator]
static A: AllocDisabler = AllocDisabler;

const SAMPLE_RATE: f64 = 48_000.0;

/// Build an engine + transport pair whose graph actually renders, rolling.
///
/// Mirrors `output.rs`'s own `build_callback_state` fixture; it is duplicated
/// rather than shared because that one is `#[cfg(test)]`-private to the unit
/// binary, and every type it needs is public.
fn rolling_state() -> (Transport, AudioCallbackState) {
    let transport = Transport::new(SAMPLE_RATE);

    let mut net = Net::new(0, 2);
    net.push(Box::new(TransportClock::new(
        transport.clock_links(),
        SAMPLE_RATE,
    )));

    let source = net.push(Box::new(Osc::sine(Hz(220.0))));
    let filter = net.push(Box::new(SvfFilterNode::<f64>::new(
        SvfType::LowPass,
        Hz(2_000.0),
        Q(0.7),
    )));
    net.connect(source, 0, filter, 0);
    net.pipe_output(filter);

    let backend = net.backend();
    // The backend borrows the engine through its inner `NetBackend`, so the
    // net must outlive it. Leaked deliberately: a test process is the whole
    // lifetime, and this keeps the fixture free of a self-referential handle.
    let _keep_net_alive: &'static Mutex<Net> = Box::leak(Box::new(Mutex::new(net)));

    let engine = Engine::new(transport.motion.clone(), backend);
    let state = AudioCallbackState::new(engine, MasterMeter::new(), AudioTap::new());

    transport.settings.set_tempo(120.0);
    let _ = transport.motion.try_send(MotionEvent::Play);
    transport.motion.drain();

    (transport, state)
}

/// Render `frames` once, outside any gate, to prime whatever the first call
/// sizes.
fn warm_up(state: &AudioCallbackState, buf: &mut [f32]) {
    process_audio(state, &mut InterleavedMut::new(buf, ChannelLayout::STEREO));
}

/// The fixture renders audible output.
///
/// Without this every gate below could pass by folding silence over an unwired
/// graph — the vacuous version of all of them.
#[test]
fn renders_something_to_gate() {
    let (_transport, state) = rolling_state();
    let mut out = vec![0.0f32; 512 * 2];

    // The filter needs a few blocks before its output is clearly non-zero.
    for _ in 0..4 {
        warm_up(&state, &mut out);
    }

    assert!(
        out.iter().any(|s| s.abs() > 1e-6),
        "the fixture graph rendered silence — the gates below would prove nothing"
    );
}

/// **The gate the crate docs promise.** `process_audio` must not allocate in
/// steady state.
///
/// Mutation check: add a `Vec::with_capacity(1)` anywhere inside
/// `process_audio` or the `Engine::process` beneath it and this aborts.
#[test]
fn process_audio_is_allocation_free() {
    let (_transport, state) = rolling_state();
    let mut out = vec![0.0f32; 1024 * 2];
    warm_up(&state, &mut out);

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1_000 {
            process_audio(
                &state,
                &mut InterleavedMut::new(&mut out, ChannelLayout::STEREO),
            );
        }
    });
}

/// A callback buffer at the `MAX_FRAMES` ceiling must also render without
/// allocating.
///
/// The stream's mix buffer is sized to `MAX_FRAMES` once and never resized, so
/// this is the largest block the render path can ever be handed. The
/// steady-state gate above uses 1024 frames and would not notice a size
/// dependency in the per-vertex buffers the graph gathers.
#[test]
fn process_audio_at_max_frames_is_allocation_free() {
    let (_transport, state) = rolling_state();
    let mut out = vec![0.0f32; MAX_FRAMES * 2];
    warm_up(&state, &mut out);

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..16 {
            process_audio(
                &state,
                &mut InterleavedMut::new(&mut out, ChannelLayout::STEREO),
            );
        }
    });
}

/// Rendering stays allocation-free across changing block sizes.
///
/// A device may change its buffer size mid-session, and the graph's per-vertex
/// buffers are sized from the block. Warming at the *largest* size first is
/// deliberate: warming small and then growing would attribute the growth
/// allocation to the gate, which reads as a failure of the render path rather
/// than of the warm-up — the trap `rt_no_alloc_hardware_poll.rs` documents.
#[test]
fn process_audio_across_block_sizes_is_allocation_free() {
    const SIZES: [usize; 5] = [1024, 512, 256, 128, 64];

    let (_transport, state) = rolling_state();
    let mut out = vec![0.0f32; SIZES[0] * 2];

    // Warm every size, largest first, outside the gate.
    for frames in SIZES {
        warm_up(&state, &mut out[..frames * 2]);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..64 {
            for frames in SIZES {
                process_audio(
                    &state,
                    &mut InterleavedMut::new(&mut out[..frames * 2], ChannelLayout::STEREO),
                );
            }
        }
    });
}
