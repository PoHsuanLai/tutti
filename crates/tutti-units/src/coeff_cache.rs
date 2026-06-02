//! Lazy coefficient cache shared by filter and envelope nodes.
//!
//! Replaces the `last_freq = -1.0` sentinel pattern that recurred across
//! `filter::svf`, `filter::ladder`, and `dynamics::envelope`. `Option<I>` is
//! the semantically correct "not yet computed" marker; `None` triggers a
//! recompute on first call, matching the sentinel behavior exactly.
//!
//! `get_or_recompute` is `#[inline]` so the hot-path callsites compile to
//! the same shape as the hand-rolled sentinel comparison.

use core::mem::MaybeUninit;

pub struct CoeffCache<I, C> {
    last_input: Option<I>,
    coeffs: MaybeUninit<C>,
}

impl<I, C> CoeffCache<I, C> {
    pub const fn new() -> Self {
        Self {
            last_input: None,
            coeffs: MaybeUninit::uninit(),
        }
    }

    /// Force the next call to `get_or_recompute` to run the closure, even if
    /// the input matches. Used when a mode switch invalidates cached values
    /// without the input field itself changing (e.g. filter type swap).
    #[inline]
    pub fn invalidate(&mut self) {
        self.last_input = None;
    }
}

impl<I: Copy, C: Copy> CoeffCache<I, C> {
    /// Returns the cached coefficients if `input` equals the last seen input,
    /// otherwise calls `compute(input)`, stores the result, and returns it.
    #[inline]
    pub fn get_or_recompute<F, Eq>(&mut self, input: I, eq: Eq, compute: F) -> C
    where
        F: FnOnce(I) -> C,
        Eq: FnOnce(I, I) -> bool,
    {
        // SAFETY: `coeffs` is initialized exactly when `last_input.is_some()`,
        // maintained by this method as the sole writer.
        if let Some(prev) = self.last_input {
            if eq(prev, input) {
                return unsafe { self.coeffs.assume_init() };
            }
        }
        let c = compute(input);
        self.coeffs = MaybeUninit::new(c);
        self.last_input = Some(input);
        c
    }
}

impl<I, C> Default for CoeffCache<I, C> {
    fn default() -> Self {
        Self::new()
    }
}

impl<I: Copy, C: Copy> Clone for CoeffCache<I, C> {
    fn clone(&self) -> Self {
        Self {
            last_input: self.last_input,
            coeffs: self.coeffs,
        }
    }
}
