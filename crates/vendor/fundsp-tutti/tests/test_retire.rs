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
