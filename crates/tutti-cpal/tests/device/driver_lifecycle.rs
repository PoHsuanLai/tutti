//! `AudioEngine` and `TuttiDriver` lifecycle, and the backend fault path —
//! neither of which had a single test.
//!
//! `driver.rs` had **zero** `#[test]`, and every `AudioEngine` method funnelled
//! through `cpal::default_host()`, so opening a sound card was the price of
//! testing any of it. `AudioEngine::from_spec` plus
//! [`ManualStreamDriver`](tutti_cpal::ManualStreamDriver) removes that: the
//! spec, the fault sink and the stop are the shipped ones, and a driver
//! decides only *where* the callback runs. An engine over a manual driver is
//! the same engine.

use crate::support::{SAMPLE_RATE, fan, graph_engine, rolling_state, spec};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tutti_cpal::{
    AudioCallbackState, AudioEngine, ManualStreamDriver, StreamFaultKind, TuttiDriver,
};
use tutti_graph::{Cx, ForkByClone, Io, Node, NodeKey, Prepare, Shape, Status};
use tutti_transport::Transport;
use tutti_types::{ChannelLayout, SampleRate};

fn engine_and_state() -> (AudioEngine, Arc<AudioCallbackState>) {
    let (_transport, state) = rolling_state(2);
    (
        AudioEngine::from_spec(spec(2, cpal::SampleFormat::F32)),
        state,
    )
}

/// **A device-free engine runs the shipped lifecycle.**
///
/// The baseline the rest of this file rests on: start, render, stop, restart,
/// with no sound card and no CPAL stream, through the same `AudioEngine`
/// methods a host calls.
#[test]
fn an_engine_over_a_manual_driver_starts_renders_and_stops() {
    let (mut engine, state) = engine_and_state();
    let (driver, stream) = ManualStreamDriver::new();

    assert!(!engine.is_running(), "nothing is running before start");
    engine
        .start_with(Arc::clone(&state), driver)
        .expect("a manual driver always opens");
    assert!(engine.is_running());
    assert!(stream.is_open());

    let block = stream.render_block(128).expect("the stream is open");
    assert_eq!(block.len(), 128 * 2, "stereo frames, interleaved");
    assert!(
        block.iter().any(|&s| s != 0.0),
        "the fixture graph must render audibly, or every assertion over it is \
         vacuous"
    );

    engine.stop();
    assert!(!engine.is_running());
    assert!(
        !stream.is_open(),
        "stopping must take the block out of the driver, not merely stop \
         polling it"
    );
    assert_eq!(
        stream.render_block(128),
        None,
        "a stopped stream renders nothing"
    );
}

/// **A second start is refused, and `stop` can be called twice.**
#[test]
fn a_second_start_is_refused_and_stop_is_repeatable() {
    let (mut engine, state) = engine_and_state();
    let (driver, _stream) = ManualStreamDriver::new();

    engine.start_with(Arc::clone(&state), driver).unwrap();
    // A second start with a fresh driver is an error, not a second stream
    // over the same state.
    let (driver2, stream2) = ManualStreamDriver::new();
    let err = engine
        .start_with(Arc::clone(&state), driver2)
        .expect_err("a running engine refuses a second start");
    assert!(matches!(err, tutti_cpal::Error::AlreadyRunning), "{err}");
    assert!(
        !stream2.is_open(),
        "starting a running engine must not open a second stream"
    );

    engine.stop();
    engine.stop();
    assert!(!engine.is_running());
}

/// **A started engine reports the spec it actually opened.**
///
/// `AudioEngine::start` re-reads the device config so `channels()` describes
/// the device that is playing; a reader sizing a buffer from a stale value
/// would get the wrong width with nothing to warn it. A bare-spec engine
/// makes that checkable without two sound cards.
#[test]
fn the_engine_reports_the_spec_it_was_opened_at() {
    let mut engine = AudioEngine::from_spec(spec(6, cpal::SampleFormat::I16));
    let (_t, state) = rolling_state(6);
    let (driver, stream) = ManualStreamDriver::new();

    assert_eq!(engine.channels().count(), 6);
    engine.start_with(state, driver).unwrap();
    assert_eq!(
        stream.channels(),
        Some(6),
        "the block must be built at the spec's width, not a default"
    );
}

/// **A restart moves the callback to a new thread, and rendering keeps
/// working.**
///
/// This is the property a host actually depends on: CPAL gives a restarted
/// stream a *different* backend thread, and the RT cells must tolerate that.
///
/// There is nothing to reset between the two threads: the engine's cells
/// check for a *concurrent* borrow, not a foreign thread, so the old
/// `reset_owners` call (a no-op) is
/// gone.
#[test]
fn a_restarted_stream_renders_from_a_different_thread() {
    let (_t, state) = rolling_state(2);
    let engine = AudioEngine::from_spec(spec(2, cpal::SampleFormat::F32));
    let mut driver = TuttiDriver::new(engine, state);

    let (d1, s1) = ManualStreamDriver::new();
    driver.start_with(d1).unwrap();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            s1.render_block(64).expect("the first stream is open");
        });
    });

    let (d2, s2) = ManualStreamDriver::new();
    driver.start_with(d2).unwrap();
    assert!(
        !s1.is_open(),
        "the restart must take the block from the old driver"
    );

    std::thread::scope(|scope| {
        scope.spawn(|| {
            let block = s2.render_block(64).expect("the restarted stream is open");
            assert_eq!(block.len(), 128);
            assert!(
                block.iter().any(|&x| x != 0.0),
                "and it must still render audibly after the move"
            );
        });
    });
}

/// **A backend fault reaches a handle taken before it happened.**
///
/// CPAL's error callback returns nothing, and both of this crate's were
/// literally `|_err| {}`. A fault surfaced nowhere at all.
#[test]
fn a_backend_fault_reaches_the_handle_taken_before_it() {
    let (mut engine, state) = engine_and_state();
    let faults = engine.faults(); // taken BEFORE anything goes wrong
    let (driver, stream) = ManualStreamDriver::new();
    engine.start_with(state, driver).unwrap();

    assert!(!faults.is_faulted());
    stream.fail(cpal::StreamError::BackendSpecific {
        err: cpal::BackendSpecificError {
            description: "xrun".into(),
        },
    });

    assert_eq!(faults.count(), 1);
    let fault = faults.last().expect("a recorded fault");
    assert_eq!(fault.kind, StreamFaultKind::Backend);
    assert!(
        fault.message.contains("xrun"),
        "the backend's own words must survive, got {:?}",
        fault.message
    );
    assert!(
        engine.is_running(),
        "a transient backend error is not a disconnect — the stream is still \
         there"
    );
}

/// **A disconnect stops the engine reporting healthy.**
///
/// `is_running()` must turn `false` for a stream whose device was unplugged,
/// or a host polling it goes on telling the user everything is fine.
#[test]
fn a_disconnect_stops_the_engine_reporting_healthy() {
    let (mut engine, state) = engine_and_state();
    let faults = engine.faults();
    let (driver, stream) = ManualStreamDriver::new();
    engine.start_with(state, driver).unwrap();
    assert!(engine.is_running());

    stream.fail(cpal::StreamError::DeviceNotAvailable);

    assert!(faults.is_disconnected());
    assert_eq!(
        faults.last().map(|f| f.kind),
        Some(StreamFaultKind::Disconnected)
    );
    assert!(
        !engine.is_running(),
        "a stream whose device is gone is not running, whatever the handle says"
    );
}

/// **A restart clears the fault state.**
///
/// Without this a single disconnect would make the engine permanently report
/// unhealthy, and reconnecting the device would not help.
#[test]
fn a_restart_clears_the_fault_state() {
    let (mut engine, state) = engine_and_state();
    let faults = engine.faults();
    let (driver, stream) = ManualStreamDriver::new();
    engine.start_with(Arc::clone(&state), driver).unwrap();
    stream.fail(cpal::StreamError::DeviceNotAvailable);
    assert!(!engine.is_running());

    engine.stop();
    let (driver2, _s2) = ManualStreamDriver::new();
    engine.start_with(state, driver2).unwrap();

    assert_eq!(faults.count(), 0, "a restart starts from a clean slate");
    assert!(!faults.is_disconnected());
    assert!(
        engine.is_running(),
        "after a restart the engine reports healthy again, or a single \
         unplug would be permanent"
    );
    assert!(
        faults.last().is_none(),
        "and the stale message must go with the count"
    );
}

/// The fault handle outlives a stop/start cycle, so a host can take it once.
#[test]
fn the_fault_handle_survives_a_restart() {
    let (mut engine, state) = engine_and_state();
    let faults = engine.faults();

    let (d1, s1) = ManualStreamDriver::new();
    engine.start_with(Arc::clone(&state), d1).unwrap();
    engine.stop();
    drop(s1);

    let (d2, s2) = ManualStreamDriver::new();
    engine.start_with(state, d2).unwrap();
    s2.fail(cpal::StreamError::DeviceNotAvailable);

    assert!(
        faults.is_disconnected(),
        "the handle taken before the first start must see a fault from the \
         second stream"
    );
}

/// **A restart hands its hook the new device's config while no stream runs,
/// and starts only after the hook** — so a host re-rates the graph before
/// the first block at the new rate, never after it.
#[test]
fn a_restart_runs_its_hook_between_the_stop_and_the_start() {
    let (_t, state) = rolling_state(2);
    let engine = AudioEngine::from_spec(spec(2, cpal::SampleFormat::F32));
    let mut driver = TuttiDriver::new(engine, state);
    let (d1, s1) = ManualStreamDriver::new();
    driver.start_with(d1).unwrap();

    let (d2, s2) = ManualStreamDriver::new();
    let new = tutti_cpal::OutputSpec::new(
        tutti_types::SampleRate::SR_96K,
        tutti_types::ChannelLayout::from(6u16),
        cpal::SampleFormat::F32,
    );
    let mut seen = None;
    driver
        .restart_on(new.clone(), d2, |spec, _| {
            assert!(!s1.is_open(), "the old stream is stopped before the hook");
            assert!(!s2.is_open(), "the new one starts after it");
            seen = Some(spec.clone());
            Ok::<(), tutti_cpal::Error>(())
        })
        .expect("restarts");
    assert_eq!(seen, Some(new.clone()), "the hook saw the new config");
    assert_eq!(driver.engine().spec(), &new, "and the driver reports it");
    assert!(s2.is_open() && driver.engine().is_running());
    assert_eq!(s2.channels(), Some(6), "opened at the new width");
}

/// **A hook that fails leaves the stream stopped**, and its error is the
/// restart's: a host that could not re-rate its graph gets no audio at the
/// wrong rate.
#[test]
fn a_restart_whose_hook_fails_stays_stopped() {
    let (_t, state) = rolling_state(2);
    let engine = AudioEngine::from_spec(spec(2, cpal::SampleFormat::F32));
    let mut driver = TuttiDriver::new(engine, state);
    let (d1, _s1) = ManualStreamDriver::new();
    driver.start_with(d1).unwrap();

    let (d2, s2) = ManualStreamDriver::new();
    let err = driver
        .restart_on(spec(2, cpal::SampleFormat::F32), d2, |_, _| {
            Err(tutti_cpal::Error::InvalidConfig("no".into()))
        })
        .expect_err("the hook's error");
    assert!(matches!(err, tutti_cpal::Error::InvalidConfig(_)));
    assert!(!driver.engine().is_running());
    assert!(!s2.is_open());
}

/// A source that counts its renders and panics in `process` on the `panic_at`th.
#[derive(Clone)]
struct PanicsAt {
    calls: Arc<AtomicUsize>,
    panic_at: usize,
}

impl Node for PanicsAt {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
    }

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        io.output(0).fill(0.5);
        assert!(n != self.panic_at, "the node fails on call {n}");
        Status::Modified
    }

    fn reset(&mut self) {}
}

fn panicking_state(calls: &Arc<AtomicUsize>, panic_at: usize) -> Arc<AudioCallbackState> {
    let engine = graph_engine(&Transport::new(SampleRate::of(SAMPLE_RATE)), |ed| {
        ed.insert(
            NodeKey(1),
            "panics",
            ForkByClone(PanicsAt {
                calls: Arc::clone(calls),
                panic_at,
            }),
        );
        fan(ed, NodeKey(1), 2);
    });
    Arc::new(AudioCallbackState::new(engine))
}

/// **A panic in the render does not leave the callback.**
///
/// The callback is called from a C frame, and unwinding through one aborts or
/// is undefined behaviour. The panicking block must return normally with a
/// silent buffer, record `CallbackPanicked`, and read as no longer running;
/// every later block is silent and never calls the engine again (its state may
/// be torn), until a new stream starts and renders afresh.
#[test]
fn a_panic_in_the_render_is_caught_latched_and_cleared_by_a_restart() {
    let calls = Arc::new(AtomicUsize::new(0));
    let state = panicking_state(&calls, 2);
    let mut engine = AudioEngine::from_spec(spec(2, cpal::SampleFormat::F32));
    let faults = engine.faults();
    let (driver, stream) = ManualStreamDriver::new();
    engine.start_with(Arc::clone(&state), driver).unwrap();

    let first = stream.render_block(64).unwrap();
    assert!(first.iter().all(|&s| s == 0.5), "the first block renders");
    assert!(!faults.is_faulted());

    let mut dirty = vec![1.0f32; 64 * 2];
    stream
        .callback_once(&mut dirty)
        .expect("the callback returns normally on a panic");
    assert!(
        dirty.iter().all(|&s| s == 0.0),
        "the panicked block is silent"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let fault = faults.last().expect("the panic is recorded");
    assert_eq!(fault.kind, StreamFaultKind::CallbackPanicked);
    assert!(faults.is_disconnected());
    assert!(!engine.is_running(), "a panicked stream needs a restart");

    for _ in 0..3 {
        let mut later = vec![1.0f32; 64 * 2];
        stream.callback_once(&mut later).unwrap();
        assert!(later.iter().all(|&s| s == 0.0), "later blocks are silent");
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "the engine is never called again after a panic"
    );
    assert_eq!(faults.count(), 1, "the latch records the panic once");

    engine.stop();
    let (driver, stream) = ManualStreamDriver::new();
    engine.start_with(state, driver).unwrap();
    assert!(!faults.is_faulted(), "a new stream reads clear");
    assert!(engine.is_running());
    let again = stream.render_block(64).unwrap();
    assert!(
        again.iter().all(|&s| s == 0.5),
        "a new stream renders again"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}
