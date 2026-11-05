//! The panic guard the `extern "C"` host callbacks run their bodies in
//! (all but the ones that only return a constant; `check-guards.sh` lists
//! them).
//!
//! A panic unwinding out of an `extern "C"` function aborts the process, which
//! takes the host (in-process) or the plugin server with it. A callback
//! answers "not handled" instead.

use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};

static REPORTED: AtomicBool = AtomicBool::new(false);

/// Run `f`; if it panics, return `fallback` (the callback's "not handled"
/// value) and note it on stderr once per process.
///
/// No-panic path: `catch_unwind` does not allocate, so this is safe on the
/// audio thread. Only the guard's own line is once-only: the default panic
/// hook still prints, allocates and takes locks on every panic. Under
/// `panic = "abort"` the guards do nothing and a panic aborts.
///
/// The report cannot panic (`writeln!` to a `stderr()` handle returns an
/// error instead), and the panic payload is forgotten rather than dropped,
/// since a payload's `Drop` could itself panic outside the `catch_unwind`.
pub(crate) fn guard<R>(fallback: R, f: impl FnOnce() -> R) -> R {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(payload) => {
            std::mem::forget(payload);
            if !REPORTED.swap(true, Ordering::Relaxed) {
                let _ = writeln!(
                    std::io::stderr(),
                    "[tutti-clap-host] a host callback panicked; answered 'not handled'"
                );
            }
            fallback
        }
    }
}

/// Run `f`; if it panics, abort the process.
///
/// For `clap.thread-pool`'s `request_exec`: a `false` answer tells the plugin
/// to run all its tasks itself, but a panic can come after some already ran
/// (or while a pool participant is still inside the job), which would run a
/// task twice or leave one racing. Nothing before the pool runs can panic;
/// a panic after it is not recoverable, so it aborts, as it did before the
/// guards.
pub(crate) fn or_abort<R>(f: impl FnOnce() -> R) -> R {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(payload) => {
            std::mem::forget(payload);
            let _ = writeln!(
                std::io::stderr(),
                "[tutti-clap-host] a panic inside clap.thread-pool request_exec; aborting"
            );
            std::process::abort()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::guard;

    #[test]
    fn a_panicking_body_returns_the_fallback() {
        // Mutation: replace `catch_unwind(..)` with `Ok(f())`: the panic
        // propagates and the test fails.
        let r = guard(7_u32, || -> u32 { panic!("callback body") });
        assert_eq!(r, 7);
        let p = guard(std::ptr::null::<u8>(), || -> *const u8 { panic!("again") });
        assert!(p.is_null());
    }

    #[test]
    fn a_returning_body_keeps_its_value() {
        // Mutation: return `fallback` unconditionally: fails here.
        assert_eq!(guard(0_u32, || 41 + 1), 42);
    }
}
