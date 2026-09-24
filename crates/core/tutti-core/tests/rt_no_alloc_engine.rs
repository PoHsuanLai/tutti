//! Regression gate for `Engine::process` with the engine's own nodes in the
//! graph.
//!
//! The existing `process_audio_is_allocation_free` test in `tutti/src/audio_io.rs`
//! only exercises the bare `TransportClock` — no signal nodes. This file adds
//! the metronome, whose meter read is the one non-scalar read on the audio
//! thread.
//!
//! The gate over a chain of real DSP nodes (EQ, strip, limiter) is
//! `tutti-nodes`' `tests/rt_no_alloc_engine.rs`. It used to live here, built
//! from fundsp's filters; running it on the nodes the engine ships means
//! depending on `tutti-nodes`, which depends on this crate, so it moved there
//! rather than create a cycle.
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
use tutti_core::AudioUnit;
use tutti_core::Engine;
use tutti_core::{
    dsp::Net, ChannelLayout, ClickNode, ClickSettings, InterleavedMut, MetronomeMode, SampleRate,
    Transport, TransportClock,
};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

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
    let clock_id = net.push(Box::new(clock));

    let settings = Arc::new(ClickSettings::new());
    settings.set_mode(MetronomeMode::Always);
    settings.set_volume(1.0);
    let click = ClickNode::with_transport(transport.clone(), Arc::clone(&settings), sample_rate);
    let click_id = net.push(Box::new(click));
    // The click reads the beat per sample off the clock's two ports.
    net.connect(clock_id, 0, click_id, 0);
    net.connect(clock_id, 1, click_id, 1);
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
        for _ in 0..1_000 {
            // The rolling clock moves the beat the click reads — ~21 beats over
            // these ~10.7 s — so the retrigger path runs inside the gate rather
            // than only the steady state.
            engine.process(&mut InterleavedMut::new(&mut output, ChannelLayout::STEREO));
        }
    });
}
