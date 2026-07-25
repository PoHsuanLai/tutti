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
use std::sync::Arc;
use tutti_core::dsp::{bell_hz, limiter_stereo, pan, sine_hz, An, AudioUnit};
use tutti_core::engine::Engine;
use tutti_core::meter::{BeatsPerBar, MeterChange, MeterMap, NoteValue, TimeSignature};
use tutti_core::params::Beat;
use tutti_core::{
    dsp::Net, ClickNode, ClickSettings, MetronomeMode, SampleRate, Transport, TransportClock,
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
        engine.process(&mut output, 512, 2);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1_000 {
            engine.process(&mut output, 512, 2);
        }
    });
}

#[test]
fn engine_process_real_chain_small_buffer_is_allocation_free() {
    // Small buffers stress the per-buffer setup overhead.
    let engine = build_engine_with_chain();

    let mut output = vec![0.0f32; 64 * 2];
    for _ in 0..16 {
        engine.process(&mut output, 64, 2);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..5_000 {
            engine.process(&mut output, 64, 2);
        }
    });
}

/// The metronome reads its meter through an `ArcSwap`, which is the one
/// non-scalar read on the audio thread. `load_full` bumps a refcount rather than
/// allocating — but that is a claim about a dependency's internals, so it gets
/// pinned here rather than reasoned about.
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
    let click_id = net.push(Box::new(An(click)));
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
        engine.process(&mut output, 512, 2);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..1_000 {
            // Move the playhead so beat changes — and therefore the retrigger
            // path — run inside the gate rather than only the steady state.
            transport.settings.set_beat(i as f64 * 0.25);
            engine.process(&mut output, 512, 2);
        }
    });
}

/// The case the test above does *not* cover: the UI republishing the meter while
/// the audio thread is running.
///
/// `ClickSettings::meter()` is `ArcSwap::load_full`, which clones the `Arc`. The
/// worry is the *drop*, not the load: once the host releases its handle to a
/// retired `MeterMap`, the audio thread could be left holding the last reference
/// and free its `Vec`s inside the callback. `arc_swap` is designed to defer
/// exactly that — but "the dependency handles it" is a claim worth a test rather
/// than a comment, and steady-state processing never exercises it because nothing
/// is ever retired.
///
/// Publishes happen outside the gate (a UI thread does them); what is measured is
/// whichever drop lands inside it.
#[test]
fn metronome_meter_swap_does_not_free_on_the_audio_thread() {
    let sample_rate = 48_000.0;
    let transport = Transport::new(sample_rate);

    let mut net = Net::new(0, 2);
    net.push(Box::new(TransportClock::new(
        transport.clock_links(),
        sample_rate,
    )));

    let settings = Arc::new(ClickSettings::new());
    settings.set_mode(MetronomeMode::Always);
    settings.set_volume(1.0);
    let click = ClickNode::with_transport(transport.clone(), Arc::clone(&settings), sample_rate);
    let click_id = net.push(Box::new(An(click)));
    net.pipe_output(click_id);

    net.set_sample_rate(SampleRate(sample_rate));
    let backend = net.backend();
    let _keep: &'static Mutex<Net> = Box::leak(Box::new(Mutex::new(net)));
    let engine = Engine::new(transport.motion.clone(), backend);

    let _ = transport.motion.try_send(tutti_core::MotionEvent::Play);
    transport.motion.drain();

    // Pre-build the maps: constructing one allocates, and that is the UI's cost,
    // not the audio thread's.
    let maps: Vec<Arc<MeterMap>> = (0..64)
        .map(|i| {
            Arc::new(MeterMap::new([MeterChange::new(
                Beat(0.0),
                TimeSignature::new(BeatsPerBar::new(3 + (i % 5)), NoteValue::EIGHTH),
            )]))
        })
        .collect();

    let mut output = vec![0.0f32; 512 * 2];
    for _ in 0..16 {
        engine.process(&mut output, 512, 2);
    }

    for (i, map) in maps.into_iter().enumerate() {
        // Publish outside the gate, then drop the host's handle immediately —
        // this is the whole point. Keeping a reference alive here would make the
        // audio thread's drop a mere refcount decrement and the test vacuous.
        // With the host's copy gone, the only remaining references are the
        // `ArcSwap` cell and whatever the audio thread loads, so the *next*
        // publish leaves the audio thread holding the last one.
        settings.set_meter(map);
        transport.settings.set_beat(i as f64 * 0.5);

        assert_no_alloc::assert_no_alloc(|| {
            engine.process(&mut output, 512, 2);
        });
    }
}
