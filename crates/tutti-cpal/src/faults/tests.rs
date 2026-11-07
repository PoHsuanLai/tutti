use super::*;

/// `record` must never wait on a reader holding `last`: it counts the fault
/// and drops the message.
///
/// Mutation (run): replace `try_lock` with `lock` in `record`; the test then
/// deadlocks (the single thread re-locks a mutex it holds) and never returns.
#[test]
fn record_while_last_is_locked_counts_and_returns() {
    let faults = StreamFaults::new();
    let held = faults.last_guard();
    faults.record(&cpal::StreamError::DeviceNotAvailable);
    drop(held);
    assert_eq!(faults.count(), 1);
    assert!(faults.is_disconnected());
    assert!(
        faults.last().is_none(),
        "the message was dropped, not waited for"
    );

    faults.record(&cpal::StreamError::DeviceNotAvailable);
    assert_eq!(faults.count(), 2);
    assert_eq!(
        faults.last().map(|f| f.kind),
        Some(StreamFaultKind::Disconnected)
    );
}

fn pollerr() -> cpal::StreamError {
    cpal::StreamError::BackendSpecific {
        err: cpal::BackendSpecificError {
            description: "`alsa::poll()` returned POLLERR".into(),
        },
    }
}

/// **A device lost on ALSA is detected though cpal never says
/// `DeviceNotAvailable` there.**
///
/// cpal's ALSA worker reports a lost device as `BackendSpecific`
/// "returned POLLERR", repeated at spin rate with no data. This feeds that
/// stream of errors, one per millisecond, and expects a disconnect once
/// `STALL_GRACE` has passed, counted once at the start of the run and once at
/// the promotion rather than once per error.
///
/// Mutation: classifying only `DeviceNotAvailable` (no promotion branch) fails
/// the `is_disconnected` assertion; counting every error fails the count.
#[test]
fn a_spinning_backend_error_with_no_audio_becomes_a_disconnect() {
    let faults = StreamFaults::new();
    let t0 = faults.epoch;
    let err = pollerr();
    let ms = Duration::from_millis;

    faults.record_at(&err, t0 + ms(1));
    assert!(!faults.is_disconnected());
    for i in 2..400 {
        faults.record_at(&err, t0 + ms(i));
    }
    assert!(!faults.is_disconnected(), "inside the grace period");
    for i in 400..700 {
        faults.record_at(&err, t0 + ms(i));
    }
    assert!(faults.is_disconnected());
    assert_eq!(faults.count(), 2);
    assert_eq!(
        faults.last().map(|f| f.kind),
        Some(StreamFaultKind::Disconnected)
    );
    assert!(faults.last().unwrap().message.contains("POLLERR"));

    // Further errors are ignored outright.
    faults.record_at(&err, t0 + ms(900));
    assert_eq!(faults.count(), 2);
}

/// **A transient xrun does not fail the stream.** Bursts of backend errors,
/// each followed by delivered audio, never accumulate into a disconnect, even
/// over far longer than the grace period.
///
/// Mutation: removing `run_start.store(0)` from `delivered` makes the runs add
/// up and fails this.
#[test]
fn xruns_between_delivered_blocks_never_disconnect() {
    let faults = StreamFaults::new();
    let t0 = faults.epoch;
    let ms = Duration::from_millis;
    for burst in 0..20u64 {
        let base = burst * 300;
        for i in 0..5 {
            faults.record_at(&pollerr(), t0 + ms(base + i));
        }
        faults.delivered();
    }
    assert!(!faults.is_disconnected());
    assert!(faults.is_faulted());
}

/// `clear` forgets a run in progress, so a restart does not inherit it.
#[test]
fn clear_forgets_a_run_in_progress() {
    let faults = StreamFaults::new();
    let t0 = faults.epoch;
    faults.record_at(&pollerr(), t0 + Duration::from_millis(1));
    faults.clear();
    faults.record_at(&pollerr(), t0 + Duration::from_millis(900));
    assert!(!faults.is_disconnected(), "a fresh run starts at 900 ms");
    assert_eq!(faults.count(), 1);
}
