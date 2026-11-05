//! The panic guard every `extern "C"` host callback runs its body in.
//!
//! A panic unwinding out of an `extern "C"` function aborts the process, which
//! takes the host (in-process) or the plugin server with it. A callback
//! answers "not handled" instead.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};

static REPORTED: AtomicBool = AtomicBool::new(false);

/// Run `f`; if it panics, return `fallback` (the callback's "not handled"
/// value) and note it on stderr once per process.
///
/// No-panic path: `catch_unwind` does not allocate, so this is safe on the
/// audio thread. The report allocates, but only on the first panic.
pub(crate) fn guard<R>(fallback: R, f: impl FnOnce() -> R) -> R {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(_) => {
            if !REPORTED.swap(true, Ordering::Relaxed) {
                eprintln!("[tutti-clap-host] a host callback panicked; answered 'not handled'");
            }
            fallback
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
