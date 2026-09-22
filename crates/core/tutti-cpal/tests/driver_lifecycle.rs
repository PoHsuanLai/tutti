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
//!
//! Mutations run, and which test each broke:
//!
//! | mutation | fails |
//! |---|---|
//! | delete `reset_owners()` from `TuttiDriver::start_with` | **nothing** — see `a_restarted_stream_renders_from_a_different_thread` |
//! | `is_running` → `self.running.is_some()` | `a_disconnect_stops_the_engine_reporting_healthy` |
//! | restore `\|_err\| {}` (drop `faults.record`) | `a_backend_fault_reaches_the_handle_taken_before_it` |
//! | drop `faults.clear()` from `start_with` | `a_restart_clears_the_fault_state` |

mod support;

use std::sync::Arc;
use support::{rolling_state, spec};
use tutti_cpal::{
    AudioCallbackState, AudioEngine, ManualStreamDriver, StreamFaultKind, TuttiDriver,
};

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

/// **`start` is idempotent and `stop` can be called twice.**
#[test]
fn start_is_idempotent_and_stop_is_repeatable() {
    let (mut engine, state) = engine_and_state();
    let (driver, _stream) = ManualStreamDriver::new();

    engine.start_with(Arc::clone(&state), driver).unwrap();
    // A second start with a fresh driver must be refused as a no-op rather
    // than opening a second stream over the same state.
    let (driver2, stream2) = ManualStreamDriver::new();
    engine.start_with(Arc::clone(&state), driver2).unwrap();
    assert!(
        !stream2.is_open(),
        "starting a running engine must be a no-op, not a second stream"
    );

    engine.stop();
    engine.stop();
    assert!(!engine.is_running());
}

/// **A started engine reports the spec it actually opened.**
///
/// `AudioEngine::start`'s long comment defends re-reading the device config:
/// leaving the fields at their construction values makes `channels()` describe
/// a device that is no longer playing while the audio itself is correct, so a
/// reader sizing a buffer from it gets the old width with nothing to warn it.
/// Nothing checked that, and a bare-spec engine is the case where it is
/// checkable without two sound cards.
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
/// **What this test does NOT prove, and why the difference matters.** An
/// earlier draft claimed to pin `TuttiDriver`'s `reset_owners` call, on the
/// strength of the comment that used to sit on it ("the owner checks would
/// otherwise flag the new thread as an intruder"). Mutation-testing says
/// otherwise: deleting that call changes nothing, because every
/// `reset_owner` in the chain bottoms out in `AudioThreadCell::reset_owner`,
/// which is a no-op — the cell's debug check detects a *concurrent borrow*,
/// not a foreign thread. The comment was stale, and is now corrected at the
/// source. A test cannot cover a guarantee that is not implemented, so this
/// one covers the guarantee that is, and says so rather than claiming the
/// other.
#[test]
fn a_restarted_stream_renders_from_a_different_thread() {
    let (_t, state) = rolling_state(2);
    let engine = AudioEngine::from_spec(spec(2, cpal::SampleFormat::F32));
    let mut driver = TuttiDriver::from_parts(engine, state);

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
/// The defect this whole path exists to fix: `is_running()` used to stay
/// `true` for a stream whose device had been unplugged, because nothing read
/// the error callback. A host polling it went on telling the user everything
/// was fine.
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
