//! [`AtomicTarget`] — the concrete [`ModTarget`], a frame-rate cap over a
//! [`LayeredCurve`].
//!
//! [`LayeredCurve`] is the one accumulator (`clamp(base + Σ layer(beat),
//! [min, max])`, curve layers keyed by [`LayerKey`]). `AtomicTarget` is the
//! *native* sink over it: it collapses the curve to a scalar at the current
//! frame and mirrors that into a shared `AtomicF32` the audio thread / UI reads
//! lock-free — no per-frame readback step.
//!
//! Because `AtomicTarget`'s [`ModTarget`] contribution API is scalar
//! (`accumulate(key, offset: f32)`), every layer it holds is a [`Const`] curve,
//! so its value is beat-independent and collapsing at any beat is exact. A sink
//! that wants *sub-block* evaluation (a plugin's per-block param producer) holds
//! the [`LayeredCurve`] directly and samples it at each block beat instead — same
//! accumulator, finer rate.
//!
//! No audio-rate sink exists yet. A native param that wants per-sample
//! modulation has nowhere to receive a curve, because `AtomicTarget` is the only
//! sink native nodes use and it collapses at a fixed beat. That gap is
//! deliberate and tracked, not an oversight to route around.

use tutti_types::Beat;

use crate::curve::Curve;
use crate::id::LayerKey;
use crate::layered::LayeredCurve;
use crate::target::ModTarget;

/// The beat a native sink collapses at. Its layers are all [`Const`] (the scalar
/// contribution API), so the value is beat-independent — any beat is exact.
const FRAME_BEAT: Beat = Beat(0.0);

/// The concrete [`ModTarget`]: a keyed accumulator whose final value is
/// **mirrored into a shared `AtomicF32`** on every write, so the consumer
/// reads it lock-free — no per-frame readback step.
///
/// The layers live in a `Mutex<LayeredCurve<f32>>` (an `AtomicF32` holds only the
/// final `f32`, not the layer set). The `Mutex` is never on the hot path: the
/// modulation driver writes it once per frame (control-rate), and the audio
/// thread / UI reads the *atomic*, never the lock. Every mutation flushes the
/// collapsed value into the atomic (`Release`-ordered), so a consumer's
/// `Acquire` load always sees a consistent value.
///
/// Two ways to build it:
/// - [`AtomicTarget::new`] owns a fresh atomic — call [`mirror`](Self::mirror)
///   to hand a read handle to the consumer. Use for UI values, tests, or a new
///   param.
/// - [`AtomicTarget::with_mirror`] mirrors into an atomic the consumer already
///   has — a native DSP node's param, or **another modulator's own param**
///   (which is how modulation *cascades* — LFO-modulates-LFO — fall out for
///   free).
pub struct AtomicTarget {
    acc: std::sync::Mutex<LayeredCurve<f32>>,
    mirror: std::sync::Arc<atomic_float::AtomicF32>,
}

impl AtomicTarget {
    /// A target over a fresh accumulator with its own atomic mirror. Seed the
    /// atomic with the initial value (== clamped base). Read the value via
    /// [`final_value`](ModTarget::final_value) or hand out [`mirror`](Self::mirror).
    pub fn new(base: f32, min: f32, max: f32) -> Self {
        Self::with_mirror(
            base,
            min,
            max,
            std::sync::Arc::new(atomic_float::AtomicF32::new(0.0)),
        )
    }

    /// A target that mirrors its final value into `mirror` — an atomic the
    /// consumer already reads (a node's param, a cascade source's param). The
    /// atomic is immediately seeded with the initial value.
    pub fn with_mirror(
        base: f32,
        min: f32,
        max: f32,
        mirror: std::sync::Arc<atomic_float::AtomicF32>,
    ) -> Self {
        let acc = LayeredCurve::new(base, min, max);
        mirror.store(
            acc.value_at(FRAME_BEAT).unwrap_or(base),
            core::sync::atomic::Ordering::Release,
        );
        Self {
            acc: std::sync::Mutex::new(acc),
            mirror,
        }
    }

    /// The shared atomic the final value is mirrored into — clone it to hand a
    /// lock-free read handle to the audio thread / consumer.
    pub fn mirror(&self) -> std::sync::Arc<atomic_float::AtomicF32> {
        std::sync::Arc::clone(&self.mirror)
    }

    /// Drop every non-automation layer (owner-side reset) and re-mirror.
    /// Deliberately not on [`ModTarget`] — a single source must not be able to
    /// wipe every other contributor.
    pub fn clear_mod_layers(&self) {
        let mut acc = self.acc.lock().unwrap();
        acc.clear_mod_layers();
        self.flush(&acc);
    }

    #[inline]
    fn flush(&self, acc: &LayeredCurve<f32>) {
        let v = acc.value_at(FRAME_BEAT).unwrap_or_else(|| acc.base());
        self.mirror.store(v, core::sync::atomic::Ordering::Release);
    }
}

impl ModTarget for AtomicTarget {
    #[inline]
    fn range(&self) -> (f32, f32) {
        self.acc.lock().unwrap().range()
    }
    #[inline]
    fn base(&self) -> f32 {
        self.acc.lock().unwrap().base()
    }
    /// Set the authored base (off-thread; UI / projection) and re-mirror.
    #[inline]
    fn set_base(&self, value: f32) {
        let mut acc = self.acc.lock().unwrap();
        acc.set_base(value);
        self.flush(&acc);
    }
    #[inline]
    fn accumulate(&self, key: LayerKey, offset: f32) {
        // A scalar contribution — beat-independent, so this native sink stays
        // exact when it collapses at `FRAME_BEAT`. `set_scalar_layer` stores a
        // bare `f32` (no `Arc`), so the control-rate driver's per-edge-per-frame
        // write is allocation-free.
        let mut acc = self.acc.lock().unwrap();
        acc.set_scalar_layer(key, offset);
        self.flush(&acc);
    }
    #[inline]
    fn clear(&self, key: LayerKey) {
        let mut acc = self.acc.lock().unwrap();
        acc.clear_layer(key);
        self.flush(&acc);
    }
    #[inline]
    fn final_value(&self) -> f32 {
        // The mirror is authoritative (every mutation flushes it) and lock-free.
        self.mirror.load(core::sync::atomic::Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::Ordering;
    use std::sync::Arc;

    const MOD_A: LayerKey = LayerKey(1);
    const MOD_B: LayerKey = LayerKey(2);

    #[test]
    fn base_is_the_value_with_no_layers() {
        let t = AtomicTarget::new(0.7, 0.0, 2.0);
        assert_eq!(t.final_value(), 0.7);
        assert_eq!(t.base(), 0.7);
    }

    #[test]
    fn layers_sum_order_independent() {
        let a = AtomicTarget::new(0.5, 0.0, 2.0);
        a.accumulate(MOD_A, 0.2);
        a.accumulate(MOD_B, -0.1);
        let b = AtomicTarget::new(0.5, 0.0, 2.0);
        b.accumulate(MOD_B, -0.1);
        b.accumulate(MOD_A, 0.2);
        assert!((a.final_value() - b.final_value()).abs() < 1e-6);
        assert!((a.final_value() - 0.6).abs() < 1e-6);
    }

    #[test]
    fn accumulate_updates_in_place_no_stacking() {
        let t = AtomicTarget::new(0.0, -10.0, 10.0);
        t.accumulate(MOD_A, 1.0);
        t.accumulate(MOD_A, 2.0);
        t.accumulate(MOD_A, 3.0);
        assert!((t.final_value() - 3.0).abs() < 1e-6); // not 1+2+3
    }

    #[test]
    fn value_clamps_to_range() {
        let t = AtomicTarget::new(0.9, 0.0, 1.0);
        t.accumulate(MOD_A, 0.5);
        assert_eq!(t.final_value(), 1.0); // 1.4 clamped
        t.accumulate(MOD_A, -2.0);
        assert_eq!(t.final_value(), 0.0); // -1.1 clamped
    }

    #[test]
    fn clear_removes_a_contribution() {
        let t = AtomicTarget::new(0.5, 0.0, 2.0);
        t.accumulate(MOD_A, 0.4);
        assert!((t.final_value() - 0.9).abs() < 1e-6);
        t.clear(MOD_A);
        assert_eq!(t.final_value(), 0.5);
    }

    #[test]
    fn set_base_re_mirrors() {
        let t = AtomicTarget::new(0.5, 0.0, 2.0);
        t.accumulate(MOD_A, 0.2);
        t.set_base(1.0);
        assert!((t.final_value() - 1.2).abs() < 1e-6); // new base + layer
    }

    #[test]
    fn clear_mod_layers_keeps_automation() {
        let t = AtomicTarget::new(0.5, 0.0, 2.0);
        t.accumulate(LayerKey::AUTOMATION, 0.1);
        t.accumulate(MOD_A, 0.2);
        t.accumulate(MOD_B, 0.3);
        t.clear_mod_layers();
        assert!((t.final_value() - 0.6).abs() < 1e-6); // base + automation only
    }

    #[test]
    fn with_mirror_writes_the_consumers_atomic() {
        let param = Arc::new(atomic_float::AtomicF32::new(0.0));
        let t = AtomicTarget::with_mirror(1000.0, 20.0, 20000.0, param.clone());
        assert_eq!(param.load(Ordering::Acquire), 1000.0); // seeded
        t.accumulate(MOD_A, 500.0);
        assert_eq!(param.load(Ordering::Acquire), 1500.0); // mirrored
    }

    #[test]
    fn does_not_drift_across_reads() {
        let t = AtomicTarget::new(1.0, 0.0, 2.0);
        t.accumulate(MOD_A, 0.3);
        let first = t.final_value();
        for _ in 0..100 {
            assert_eq!(t.final_value(), first);
        }
        assert_eq!(t.base(), 1.0); // base never moved
        assert!((first - 1.3).abs() < 1e-6);
    }
}
