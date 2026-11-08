//! The panic guard every `extern "C"` callback body runs in.
//!
//! A panic unwinding out of an `extern "C"` function aborts the process, which
//! takes the host with it. A callback answers its "could not do that" value
//! instead.

use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};

static REPORTED: AtomicBool = AtomicBool::new(false);

/// Run `f`; if it panics, return `fallback` and note it on stderr once per
/// process.
///
/// `catch_unwind` does not allocate on the no-panic path, so the guard is safe
/// on the audio thread. The panic payload is forgotten rather than dropped,
/// since a payload's `Drop` could itself panic outside the `catch_unwind`.
/// Under `panic = "abort"` the guard does nothing and a panic aborts.
pub(crate) fn guard<R>(callback: &str, fallback: R, f: impl FnOnce() -> R) -> R {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(payload) => {
            std::mem::forget(payload);
            if !REPORTED.swap(true, Ordering::Relaxed) {
                let _ = writeln!(
                    std::io::stderr(),
                    "[tutti-au-ffi] a panic in {callback} was contained to avoid unwinding \
                     into AudioToolbox"
                );
            }
            fallback
        }
    }
}
