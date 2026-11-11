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

/// An out-parameter a plugin passed to a COM method: null (the plugin wants no
/// answer) or valid for one write for the length of the call.
///
/// It is the one place the host writes through a plugin's out-pointer, so the
/// null check and the SAFETY argument are made once. Build it before the
/// [`guard`] and set it to the "nothing" answer first, so a panic in the body
/// leaves the plugin a defined value.
pub(crate) struct Out<T>(*mut T);

impl<T> Out<T> {
    /// # Safety
    ///
    /// `ptr` is null, or valid for a write of `T` for as long as this `Out`
    /// lives (the COM ABI gives that for the duration of the method call the
    /// `Out` is built in).
    pub(crate) unsafe fn new(ptr: *mut T) -> Self {
        Self(ptr)
    }

    /// Whether the plugin passed no pointer.
    pub(crate) fn is_null(&self) -> bool {
        self.0.is_null()
    }

    /// Writes `value` if the plugin passed a pointer.
    pub(crate) fn set(&self, value: T) {
        if !self.0.is_null() {
            // SAFETY: non-null, and valid for a write by `new`'s contract.
            unsafe { self.0.write(value) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Out, guard};

    #[test]
    fn an_out_writes_through_a_pointer_and_skips_a_null_one() {
        let mut slot = 0;
        // SAFETY: `slot` outlives `out`.
        let out = unsafe { Out::new(&mut slot) };
        out.set(7);
        assert_eq!(slot, 7);
        // SAFETY: null is allowed.
        unsafe { Out::<i32>::new(std::ptr::null_mut()) }.set(9);
    }

    use vst3::Steinberg::{kInternalError, tresult};

    #[test]
    fn a_panicking_body_returns_the_fallback() {
        let r: tresult = guard(kInternalError, || panic!("method body"));
        assert_eq!(r, kInternalError);
        let p = guard(std::ptr::null::<u8>(), || -> *const u8 { panic!("again") });
        assert!(p.is_null());
    }

    #[test]
    fn a_returning_body_keeps_its_value() {
        assert_eq!(guard(0_i32, || 41 + 1), 42);
    }
}
