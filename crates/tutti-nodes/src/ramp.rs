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

/// The block's value of declared param port `k` when the graph drives it
/// (a control lane on the port): the value on the block's **last** frame,
/// which a block-rate control ramps to exactly as it does to a new cell
/// value. `None`: nothing drives it, so the node reads its own cell.
///
/// For a control the node reads once per block (an LFO rate, a time
/// constant, a wet/dry mix ramped across the block); a kernel that reads a
/// param per sample takes `io.param(k).frames()` itself.
///
/// Last frame rather than first is not pinned by a test: a lane's control
/// rate is one value per block, so every test drive reads the same at every
/// frame. It is the choice that agrees with the cell's own ramp.
#[inline]
pub(crate) fn block_driven(io: &tutti_graph::Io<'_>, k: usize) -> Option<f32> {
    io.param(k).frames().and_then(|v| v.last().copied())
}

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

    /// Starts at a finite constant.
    pub(crate) fn fixed(initial: f32) -> Self {
        Self(initial)
    }

    /// `value` if finite (and remembered), otherwise the last finite value.
    #[inline]
    pub(crate) fn read(&mut self, value: f32) -> f32 {
        if value.is_finite() {
            self.0 = value;
        }
        self.0
    }
}

/// A driven value through its setter's `clamp`, a non-finite one left as it
/// is for the [`LastGood`] read that follows (a floor written with `f32::max`
/// would turn a NaN into the floor and hide it from that read).
#[inline]
pub(crate) fn clamp_finite(value: f32, clamp: impl FnOnce(f32) -> f32) -> f32 {
    if value.is_finite() {
        clamp(value)
    } else {
        value
    }
}

/// `value` if finite, else `fallback` — for audio-rate param ports, which are
/// read per sample and fall back to the block's (already sanitised) value.
#[inline]
pub(crate) fn finite_or(value: f32, fallback: f32) -> f32 {
    if value.is_finite() { value } else { fallback }
}
