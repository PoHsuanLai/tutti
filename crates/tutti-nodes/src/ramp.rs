//! Per-block control reads and the ramps that keep them from stepping.
//!
//! The width-generic nodes read every [`Param`](tutti_types::Param) **once per
//! `process` call** instead of once per sample — a per-sample load would be up
//! to four atomics per channel per sample on the ladder, and none of them can
//! change faster than a block anyway, since the control thread writes between
//! callbacks. What a block-rate read *can* do is step:
//! a cutoff or a delay time that jumps at a block edge clicks. So a value that
//! moved since the last block is ramped **linearly across the block**, from the
//! value the previous block ended on to the new one, arriving exactly on the
//! block's last sample.
//!
//! Ending exactly on the target is what makes a block of one agree with a
//! per-sample read: a one-sample ramp *is* the new value.

/// The last finite value a control cell held — what a non-finite write reads
/// as.
///
/// A raw `Arc<AtomicF32>` cell accepts anything, and a NaN or ±∞ that reaches
/// a recursive node's state (a filter integrator, a delay line, an LFO phase,
/// an envelope follower) stays there for good: every later sample is computed
/// from it. So each per-block read goes through one of these, and a
/// non-finite value reads as "unchanged" — the last good value — until a
/// finite one is written.
///
/// Clamping cannot stand in for this: `f32::clamp` passes NaN through.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LastGood(f32);

impl LastGood {
    /// Starts at `initial`, or at `fallback` if `initial` is not finite.
    ///
    /// `fallback` is the control's own default, chosen by the caller: a
    /// control whose range excludes zero (a ratio, a Q) must not start at
    /// `0.0`, which is outside it.
    pub(crate) fn new(initial: f32, fallback: f32) -> Self {
        Self(if initial.is_finite() {
            initial
        } else {
            fallback
        })
    }

    /// Starts at `initial` if `valid`, else at `fallback`: a control that accepts more
    /// than the finite values says so (`ControlSet::valid`).
    pub(crate) fn new_if(initial: f32, fallback: f32, valid: bool) -> Self {
        Self(if valid { initial } else { fallback })
    }

    /// Starts at a finite constant.
    pub(crate) fn fixed(initial: f32) -> Self {
        Self(initial)
    }

    /// `value` if finite (and remembered), otherwise the last finite value.
    #[inline]
    pub(crate) fn read(&mut self, value: f32) -> f32 {
        self.read_if(value, value.is_finite())
    }

    /// `value` if `valid` (and remembered), otherwise the last valid value: for a
    /// control whose valid values are not the finite ones (a ceiling accepts
    /// `+-inf`, `ControlSet::valid`).
    #[inline]
    pub(crate) fn read_if(&mut self, value: f32, valid: bool) -> f32 {
        if valid {
            self.0 = value;
        }
        self.0
    }
}
