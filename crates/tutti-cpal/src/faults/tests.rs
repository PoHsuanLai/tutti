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
