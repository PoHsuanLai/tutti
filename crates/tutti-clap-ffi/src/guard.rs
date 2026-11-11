//! The panic guard every `extern "C"` host callback runs its body in.
//!
//! A panic unwinding out of an `extern "C"` function aborts the process, which
//! takes the host (in-process) or the plugin server with it. A callback
//! answers "not handled" instead.

use std::ffi::{CStr, c_char, c_void};
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
                    "[tutti-clap-ffi] a host callback panicked; answered 'not handled'"
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
/// a panic after it is not recoverable, so it aborts.
pub(crate) fn or_abort<R>(f: impl FnOnce() -> R) -> R {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(payload) => {
            std::mem::forget(payload);
            let _ = writeln!(
                std::io::stderr(),
                "[tutti-clap-ffi] a panic inside clap.thread-pool request_exec; aborting"
            );
            std::process::abort()
        }
    }
}
/// A nul-terminated string a plugin passed, copied lossily; empty for null.
///
/// # Safety
/// `ptr` is null or points to a nul-terminated string readable for the call.
pub(crate) unsafe fn lossy(ptr: *const c_char) -> String {
    if ptr.is_null() {
        String::new()
    } else {
        // SAFETY: the caller's contract.
        unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned()
    }
}

/// The `T` a callback's `ctx` names, shared.
///
/// # Safety
/// `ctx` is the pointer to a live `T` that the call which built the C object
/// stored (`with_input`, `with_raw`, ...), and nothing mutates the `T` while
/// the plugin runs inside that call.
pub(crate) unsafe fn ctx_ref<'a, T>(ctx: *mut c_void) -> &'a T {
    // SAFETY: the contract.
    unsafe { &*ctx.cast::<T>() }
}

/// The `T` a callback's `ctx` names, exclusively.
///
/// # Safety
/// As [`ctx_ref`], and no other reference to the `T` is live meanwhile.
pub(crate) unsafe fn ctx_mut<'a, T>(ctx: *mut c_void) -> &'a mut T {
    // SAFETY: the contract.
    unsafe { &mut *ctx.cast::<T>() }
}

#[cfg(test)]
mod tests {
    use super::guard;

    #[test]
    fn a_panicking_body_returns_the_fallback() {
        let r = guard(7_u32, || -> u32 { panic!("callback body") });
        assert_eq!(r, 7);
        let p = guard(std::ptr::null::<u8>(), || -> *const u8 { panic!("again") });
        assert!(p.is_null());
    }

    #[test]
    fn a_returning_body_keeps_its_value() {
        assert_eq!(guard(0_u32, || 41 + 1), 42);
    }
}
