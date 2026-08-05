//! Retirement of replaced units must never free them on the audio thread.
//!
//! The return queue holds 256 entries and drains only in `Net::commit`, on the
//! control thread. Crossfading more than that between two commits used to hand
//! the surplus back through a `Result` whose `Err` was dropped where it stood —
//! inside `process`, i.e. inside the audio callback.

use fundsp_tutti::audiounit::*;
use fundsp_tutti::prelude::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A unit that reports its own destruction, so a test can tell *when* it is
/// freed rather than merely whether.
#[derive(Clone)]
struct DropCounter {
    dropped: Arc<AtomicUsize>,
}

impl Drop for DropCounter {
    fn drop(&mut self) {
        self.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

impl AudioUnit for DropCounter {
    fn reset(&mut self) {}
    fn set_sample_rate(&mut self, _sample_rate: SampleRate) {}
    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        output[0] = 0.0;
    }
    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        for i in 0..size {
            output.set_f32(0, i, 0.0);
        }
    }
    fn inputs(&self) -> usize {
        0
    }
    fn outputs(&self) -> usize {
        1
    }
    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        SignalFrame::new(1)
    }
    fn get_id(&self) -> u64 {
        0xD40D
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>()
    }
}

/// Overflow the 256-slot return queue with crossfades and render throughout.
///
/// Nothing may be freed while only the backend is running: the frontend has not
/// committed, so it has not drained, so every retired unit must still be held.
#[test]
fn retired_units_are_not_freed_during_render() {
    let dropped = Arc::new(AtomicUsize::new(0));

    let mut net = Net::new(0, 1);
    let id = net.push(Box::new(DropCounter {
        dropped: Arc::clone(&dropped),
    }));
    net.pipe_output(id);
    let mut backend = net.backend();

    // 400 crossfades — comfortably past the queue's 256 — queued into a *single*
    // commit. Committing once per fade would drain the return queue on this
    // thread between fades and the overflow would never happen; the bug needs
    // more retirements than the queue holds without a frontend drain in between.
    for _ in 0..400 {
        net.crossfade(
            id,
            Fade::Smooth,
            0.0,
            Box::new(DropCounter {
                dropped: Arc::clone(&dropped),
            }),
        );
    }
    net.commit();

    // Render enough blocks for every queued edit to fade through and retire.
    // A `fade_time` of 0 retires the old unit on the very next block, so each
    // block advances one retirement.
    let mut output = BufferVec::new(1);
    let before_render = dropped.load(Ordering::SeqCst);
    for _ in 0..500 {
        backend.process(64, &BufferRef::empty(), &mut output.buffer_mut());
    }

    let freed = dropped.load(Ordering::SeqCst) - before_render;
    assert_eq!(
        freed, 0,
        "retired units must be parked for the frontend, never dropped on the \
         render path (freed {freed} while rendering)"
    );
}

/// A setting that outruns the message queue is counted, not silently lost.
#[test]
fn dropped_settings_are_counted() {
    let mut net = Net::new(0, 1);
    let id = net.push(Box::new(DropCounter {
        dropped: Arc::new(AtomicUsize::new(0)),
    }));
    net.pipe_output(id);
    let mut backend = net.backend();

    assert_eq!(net.take_dropped_settings(), 0, "nothing dropped yet");

    // The queue holds 256 messages and only the backend drains it, so 300
    // settings with no render in between must overflow.
    for i in 0..300 {
        net.set(Setting::value(i as f32).node(id));
    }
    let dropped = net.take_dropped_settings();
    assert!(
        dropped > 0,
        "overflowing the message queue must be counted, not silent"
    );
    assert_eq!(net.take_dropped_settings(), 0, "taking the count resets it");

    // The backend still runs; a full queue is not a broken one.
    let mut output = BufferVec::new(1);
    backend.process(64, &BufferRef::empty(), &mut output.buffer_mut());
}

/// The parked units are a deferral, not a leak: once the frontend drains the
/// return queue, they are freed on the control thread.
#[test]
fn parked_units_are_freed_once_the_frontend_drains() {
    let dropped = Arc::new(AtomicUsize::new(0));

    let mut net = Net::new(0, 1);
    let id = net.push(Box::new(DropCounter {
        dropped: Arc::clone(&dropped),
    }));
    net.pipe_output(id);
    let mut backend = net.backend();

    for _ in 0..400 {
        net.crossfade(
            id,
            Fade::Smooth,
            0.0,
            Box::new(DropCounter {
                dropped: Arc::clone(&dropped),
            }),
        );
    }
    net.commit();

    let mut output = BufferVec::new(1);
    for _ in 0..500 {
        backend.process(64, &BufferRef::empty(), &mut output.buffer_mut());
    }

    let before = dropped.load(Ordering::SeqCst);

    // Keep committing and rendering. Each commit drains the queue on this
    // thread, which lets the backend hand over its backlog on the next block.
    for _ in 0..600 {
        net.commit();
        backend.process(64, &BufferRef::empty(), &mut output.buffer_mut());
    }

    assert!(
        dropped.load(Ordering::SeqCst) > before,
        "parked units must reach the frontend and be freed there"
    );
}

/// A setting addressed to a node the network does not contain is **counted**,
/// not silently discarded.
///
/// This is the top of the silent-no-op stack `tutti-units`' module docs
/// describe, and it was the one layer with no counter at all: `Net::set` had an
/// `if`/`else if` with no `else`, so a wrong `NodeId` vanished with
/// `dropped_settings` still reading zero. A parameter wired to nothing then
/// presented exactly like a working one — "the fader moves on screen and not in
/// the sound" — which is the symptom this whole family of counters exists to
/// make nameable.
///
/// **Read from the frontend, counted on the backend.** That split is the point:
/// the frontend only enqueues, so the address is not resolved until the audio
/// thread calls `set` on its own network. Without the shared cell installed by
/// `Net::backend`, the count would accumulate where no host can reach it.
#[test]
fn unaddressed_settings_are_counted() {
    let mut net = Net::new(0, 1);
    let real = net.push(Box::new(DropCounter {
        dropped: Arc::new(AtomicUsize::new(0)),
    }));
    net.pipe_output(real);

    // A node that exists, then is removed — so its id is well-formed and stale
    // rather than invented. That is the realistic shape of this bug: a caller
    // holding an id across an edit that retired it.
    let ghost = net.push(Box::new(DropCounter {
        dropped: Arc::new(AtomicUsize::new(0)),
    }));

    let mut backend = net.backend();
    let mut output = BufferVec::new(1);
    assert_eq!(
        net.take_unaddressed_settings(),
        0,
        "nothing misaddressed yet"
    );

    // Retire the ghost *after* the backend exists, so the removal reaches the
    // audio thread through a commit — which is the only way its `node_index`
    // stops containing the id.
    net.remove(ghost);
    net.commit();
    backend.process(64, &BufferRef::empty(), &mut output.buffer_mut());

    // Addressed to the live node: must be applied, never counted.
    net.set(Setting::value(0.5).node(real));
    backend.process(64, &BufferRef::empty(), &mut output.buffer_mut());
    assert_eq!(
        net.take_unaddressed_settings(),
        0,
        "a setting that resolves must not be counted as lost"
    );

    // Addressed to the removed node: unroutable, and must say so.
    net.set(Setting::value(0.5).node(ghost));
    backend.process(64, &BufferRef::empty(), &mut output.buffer_mut());
    assert_eq!(
        net.take_unaddressed_settings(),
        1,
        "a setting naming a node the network does not contain must be counted"
    );
    assert_eq!(
        net.take_unaddressed_settings(),
        0,
        "taking the count resets it"
    );

    // And the two counters stay distinct: this was backpressure-free.
    assert_eq!(
        net.take_dropped_settings(),
        0,
        "an unaddressed setting is not a dropped one — merging the counters \
         would make a wiring bug look like a full queue"
    );
}

/// An address a `Net` cannot route at all is counted too, not just a stale id.
///
/// `Net` resolves [`Address::Node`] and nothing else, so `Index`/`Left`/`Right`
/// and the default `Null` are unroutable here — they are addressed for a
/// different shape of unit (a combinator resolves `left`/`right`;
/// `Setting::interval` carries no address). All were discarded in silence before
/// the counter existed. This pins that they now register, because the widened
/// claim in `take_unaddressed_settings`'s doc is otherwise unverified.
#[test]
fn an_address_a_net_cannot_route_is_counted() {
    let mut net = Net::new(0, 1);
    let id = net.push(Box::new(DropCounter {
        dropped: Arc::new(AtomicUsize::new(0)),
    }));
    net.pipe_output(id);
    let mut backend = net.backend();
    let mut output = BufferVec::new(1);

    // No `.node(..)`: `Address::Null`, the default.
    net.set(Setting::value(0.5));
    // Addressed by vertex index, which `Net` does not resolve.
    net.set(Setting::value(0.5).index(0));
    backend.process(64, &BufferRef::empty(), &mut output.buffer_mut());

    assert_eq!(
        net.take_unaddressed_settings(),
        2,
        "a Net routes Address::Node alone; every other address is unroutable \
         here and must be counted rather than vanish"
    );
}
