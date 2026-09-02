//! Regression gate for `Engine::process` over a real DSP chain.
//!
//! The existing `process_audio_is_allocation_free` test in `tutti/src/audio_io.rs`
//! only exercises the bare `TransportClock` — no signal nodes. This test
//! pushes an oscillator → bell EQ → limiter chain into the net so the
//! gate covers the per-buffer hot path through actual fundsp `AudioUnit`
//! impls: `Sine`, `FixedSvf` (bell), and `Limiter`. A regression in any
//! of those (or in `Net`'s vertex iteration) shows up here as an
//! allocation panic.
//!
//! What this file deliberately does *not* test: that the audio thread never
//! *frees* a retired non-scalar value (a `MeterMap`, a routing table). That
//! property is carried by the type — `RtPublish::read` returns an `RtRef`, a
//! `!Send` borrow tied to the cell's lifetime, so there is no owning handle for
//! the callback to be left holding. It is a compile-time guarantee, not a
//! sampled one, and it cannot be pinned by a no-alloc gate: the hazard is a
//! race, and a sampling schedule cannot exhaust one. A test that tried anyway
//! lived here until it was removed — it passed while asserting a property it
//! structurally could not observe, which reads as coverage and is worse than
//! nothing. See #34 for the residual case (`RtPublish` wraps `ArcSwap` today,
//! making RT deallocation very unlikely and bounded rather than impossible).

use assert_no_alloc::AllocDisabler;
use parking_lot::Mutex;
use std::sync::Arc;
use tutti_core::dsp::{bell_hz, limiter_stereo, pan, sine_hz};
use tutti_core::AudioUnit;
use tutti_core::Engine;
use tutti_core::{
    dsp::Net, ChannelLayout, ClickNode, ClickSettings, InterleavedMut, MetronomeMode, SampleRate,
    Transport, TransportClock,
};

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
    // `clock_links()` supplies the writeback too — it is no longer a separate
    // builder call a caller can forget.
    let clock = TransportClock::new(transport.clock_links(), sample_rate);
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
        engine.process(&mut InterleavedMut::new(&mut output, ChannelLayout::STEREO));
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1_000 {
            engine.process(&mut InterleavedMut::new(&mut output, ChannelLayout::STEREO));
        }
    });
}

#[test]
fn engine_process_real_chain_small_buffer_is_allocation_free() {
    // Small buffers stress the per-buffer setup overhead.
    let engine = build_engine_with_chain();

    let mut output = vec![0.0f32; 64 * 2];
    for _ in 0..16 {
        engine.process(&mut InterleavedMut::new(&mut output, ChannelLayout::STEREO));
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..5_000 {
            engine.process(&mut InterleavedMut::new(&mut output, ChannelLayout::STEREO));
        }
    });
}

/// The metronome reads its meter through an [`RtPublish`], which is the one
/// non-scalar read on the audio thread. That read is a thread-local lookup plus
/// two atomic loads, not an allocation — but that is a claim about a
/// dependency's internals, so it gets pinned here rather than reasoned about.
///
/// [`RtPublish`]: tutti_types::RtPublish
///
/// The click must be *enabled and sounding*: a gated-off metronome returns before
/// touching the meter at all, which would make this test prove nothing.
#[test]
fn engine_process_with_metronome_is_allocation_free() {
    let sample_rate = 48_000.0;
    let transport = Transport::new(sample_rate);

    let mut net = Net::new(0, 2);
    let clock = TransportClock::new(transport.clock_links(), sample_rate);
    net.push(Box::new(clock));

    let settings = Arc::new(ClickSettings::new());
    settings.set_mode(MetronomeMode::Always);
    settings.set_volume(1.0);
    let click = ClickNode::with_transport(transport.clone(), Arc::clone(&settings), sample_rate);
    let click_id = net.push(Box::new(click));
    net.pipe_output(click_id);

    net.set_sample_rate(SampleRate(sample_rate));
    let backend = net.backend();
    let _keep: &'static Mutex<Net> = Box::leak(Box::new(Mutex::new(net)));

    let engine = Engine::new(transport.motion.clone(), backend);

    // Roll the transport so the click is actually generating, not gated off.
    let _ = transport.motion.try_send(tutti_core::MotionEvent::Play);
    transport.motion.drain();

    let mut output = vec![0.0f32; 512 * 2];
    for _ in 0..16 {
        engine.process(&mut InterleavedMut::new(&mut output, ChannelLayout::STEREO));
    }

    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..1_000 {
            // Move the playhead so beat changes — and therefore the retrigger
            // path — run inside the gate rather than only the steady state.
            transport.settings.set_beat(i as f64 * 0.25);
            engine.process(&mut InterleavedMut::new(&mut output, ChannelLayout::STEREO));
        }
    });
}
