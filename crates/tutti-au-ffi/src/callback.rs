//! The one shape of a C callback that carries a state pointer.

use std::os::raw::c_void;

use crate::guard::guard;

/// Runs `f` on the `S` behind `refcon`, under [`guard`].
///
/// A null `refcon` answers `null_answer`; a panic in `f` answers `panic_answer`.
///
/// # Safety
/// `refcon` is null or points at a live `S` that nothing else mutates for the
/// call. Registrations uphold it by withdrawing the callback before the state
/// is freed.
pub(crate) unsafe fn with_state<S, R>(
    name: &str,
    refcon: *mut c_void,
    null_answer: R,
    panic_answer: R,
    f: impl FnOnce(&S) -> R,
) -> R {
    let _ = (name, panic_answer);
    if refcon.is_null() {
        return null_answer;
    }
    // SAFETY: mutation.
    f(unsafe { &*refcon.cast::<S>() })
}

/// As [`with_state`], for a state the callback mutates.
///
/// # Safety
/// As [`with_state`], and no other reference to the `S` exists for the call.
pub(crate) unsafe fn with_state_mut<S, R>(
    name: &str,
    refcon: *mut c_void,
    null_answer: R,
    panic_answer: R,
    f: impl FnOnce(&mut S) -> R,
) -> R {
    guard(name, panic_answer, || {
        if refcon.is_null() {
            return null_answer;
        }
        // SAFETY: non-null, and live and exclusive for the call by the contract.
        f(unsafe { &mut *refcon.cast::<S>() })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A panic in the body is contained and answers the panic value.
    ///
    /// Mutation (run): call `f` outside `guard` in `with_state` -> this test
    /// panics instead of returning 7.
    #[test]
    fn a_panic_in_the_body_is_contained() {
        let mut state = 1u32;
        let refcon = (&mut state as *mut u32).cast::<c_void>();
        // SAFETY: `refcon` is a live `u32` on this stack.
        let answer =
            unsafe { with_state::<u32, i32>("test", refcon, -1, 7, |_| panic!("callback bug")) };
        assert_eq!(answer, 7);
        // SAFETY: as above, exclusive for the call.
        let answer = unsafe {
            with_state_mut::<u32, i32>("test", refcon, -1, 7, |_| panic!("callback bug"))
        };
        assert_eq!(answer, 7);
    }

    /// The state reaches the body, and a null pointer never does.
    #[test]
    fn the_state_is_recovered_and_null_is_refused() {
        let mut state = 41u32;
        let refcon = (&mut state as *mut u32).cast::<c_void>();
        // SAFETY: `refcon` is a live `u32` on this stack.
        let seen = unsafe { with_state::<u32, i32>("test", refcon, -1, 7, |s| *s as i32) };
        assert_eq!(seen, 41);
        // SAFETY: exclusive for the call.
        unsafe {
            with_state_mut::<u32, i32>("test", refcon, -1, 7, |s| {
                *s += 1;
                0
            })
        };
        assert_eq!(state, 42);
        // SAFETY: null is allowed.
        let null = unsafe {
            with_state::<u32, i32>("test", std::ptr::null_mut(), -1, 7, |_| unreachable!())
        };
        assert_eq!(null, -1);
    }
}
