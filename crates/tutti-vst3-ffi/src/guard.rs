//! The panic guard the COM methods the host implements run their bodies in
//! (all but `createContextMenu`, which only returns null).
//!
//! A panic unwinding out of the `extern "system"` shim the `vst3` crate
//! generates aborts the process, which takes the host (in-process) or the
//! plugin server with it. A method answers `kInternalError` (or its null /
//! zero) instead.

use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};

static REPORTED: AtomicBool = AtomicBool::new(false);

/// Run `f`; if it panics, return `fallback` (the method's "not handled"
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
                    "[tutti-vst3-ffi] a host COM method panicked; answered 'not handled'"
                );
            }
            fallback
        }
    }
}

#[cfg(test)]
mod tests {
    use super::guard;
    use vst3::Steinberg::{kInternalError, tresult};

    #[test]
    fn a_panicking_body_returns_the_fallback() {
        // Mutation: replace `catch_unwind(..)` with `Ok(f())`: the panic
        // propagates and the test fails.
        let r: tresult = guard(kInternalError, || panic!("method body"));
        assert_eq!(r, kInternalError);
        let p = guard(std::ptr::null::<u8>(), || -> *const u8 { panic!("again") });
        assert!(p.is_null());
    }

    #[test]
    fn a_returning_body_keeps_its_value() {
        // Mutation: return `fallback` unconditionally: fails here.
        assert_eq!(guard(0_i32, || 41 + 1), 42);
    }
}
