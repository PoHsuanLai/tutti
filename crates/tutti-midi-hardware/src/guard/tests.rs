use super::*;

/// **A panic in the callback is caught at its edge, marks the input dead and
/// stops the callback**: the first call panics and returns `None` without
/// unwinding into the caller (which, for CoreMIDI, is C), `died` is set, and a
/// later call does not run its body at all.
#[test]
fn a_panicking_callback_stops_at_the_guard_and_ends_the_input() {
    let died = Arc::new(AtomicBool::new(false));
    let guard = Guard::new(Arc::clone(&died));
    assert_eq!(guard.run(|| 7), Some(7));
    assert!(!died.load(Ordering::Acquire));

    let caught =
        std::panic::catch_unwind(|| guard.run(|| -> u32 { panic!("a bug in the callback") }));
    assert_eq!(caught.ok(), Some(None), "the panic did not unwind out");
    assert!(died.load(Ordering::Acquire));

    let mut ran = false;
    assert_eq!(guard.run(|| ran = true), None);
    assert!(!ran, "a dead input's callback does nothing");
}
