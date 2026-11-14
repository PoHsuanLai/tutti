//! A target with no OS main-thread query wired: no thread is known to be the
//! main one, so `MainThreadMarker::new` is always `None` and an in-process
//! plugin host is refused at the call. The crate still compiles.

pub(super) fn is_main_thread() -> bool {
    false
}
