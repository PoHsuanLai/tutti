//! [`Curve`] — a value as a pure function of a musical position.
//!
//! The rate-agnostic `beat -> value` interface shared by automation and
//! modulation: an automation envelope, an LFO, a constant, and the summing
//! [`crate::LayeredCurve`] are all `Curve`s. Homed here (not in tutti-units)
//! because modulation depends on it and tutti-units already depends on tutti-mod
//! — so tutti-units re-exports it rather than owning it. The one foreign impl,
//! [`AutomationEnvelope`], is orphan-legal here since `audio_automation` is a
//! direct dependency.

use audio_automation::AutomationEnvelope;
use tutti_types::Beat;

/// A curve: a value as a pure function of a musical position.
///
/// The stored form — breakpoints, a constant, an LFO shape, an expression — is
/// the implementor's business. Callers supply an already-resolved, already
/// loop-wrapped [`Beat`]; the curve holds no clock and consults no loop range.
/// That keeps it evaluable from a live transport, an offline render, or a
/// per-sample port signal alike — the curve is the `a` in `y = a(w(b(s)))`,
/// composed with the clock at the call site rather than owning one.
///
/// Returns `None` where the curve has no value (disabled / empty), so callers
/// keep the empty-vs-zero distinction the playback consumers rely on: an
/// automation lane substitutes `0.0`, a plugin parameter source leaves the
/// plugin at its last value.
pub trait Curve: Send + Sync {
    /// Evaluate the curve at `beat`, or `None` if it has no value there.
    fn value_at(&self, beat: Beat) -> Option<f32>;
}

/// `T` is the envelope's target *label* — never the evaluated value, which is
/// always `f32` — so the impl ignores it beyond the thread bounds a `Curve`
/// trait object needs.
impl<T: Send + Sync> Curve for AutomationEnvelope<T> {
    fn value_at(&self, beat: Beat) -> Option<f32> {
        self.get_value_at(beat.get())
    }
}
