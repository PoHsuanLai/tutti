//! [`Curve`] — a value as a pure function of a musical position.
//!
//! The trait lives here, beside its `AutomationEnvelope` impl: the envelope is
//! foreign (`audio_automation`) and `tutti-types` cannot see it, so co-locating
//! the trait with the impl is the orphan-rule-legal home. The one dependency on
//! the value vocabulary is [`Beat`], which `tutti-core` already re-exports.

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
/// [`AutomationLane`](super::AutomationLane) substitutes `0.0`, a plugin
/// parameter source leaves the plugin at its last value.
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
