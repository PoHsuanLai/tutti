//! The concrete modulation accumulator: [`ModAccumulator`] (the value type) and
//! [`AtomicTarget`] (its `&self` [`ModTarget`] handle, mirroring into a shared
//! atomic).
//!
//! `ModAccumulator` is `base + Σ (LayerKey, offset)` clamped to `[min, max]` —
//! the base is authored (owned by exactly one writer, projection/UI); every
//! other contributor writes an *offset* into its own keyed layer, so summation
//! is order-independent and reading [`final_value`](ModAccumulator::final_value)
//! never drifts the base. It lives here so tutti ships the whole modulation
//! subsystem — source, target, and routing.
//!
//! [`AtomicTarget`] is the one concrete [`ModTarget`]: it owns the keyed layers
//! and mirrors the folded result into a shared `AtomicF32` the consumer reads
//! lock-free.

use crate::fold;
use crate::id::LayerKey;
use crate::target::ModTarget;

/// A parameter as **base + named offset layers**, clamped to a range.
///
/// ```text
/// final = clamp(base + Σ layer_offset, [min, max])
/// ```
///
/// Layers are `(LayerKey, offset)` pairs — tiny in practice (automation + a
/// couple of modulators), so a plain `Vec` is used. `dirty` is set on every
/// mutation and cleared by [`take_dirty`](Self::take_dirty), so a host can push
/// the computed value to a node exactly once, only when it moved.
#[cfg_attr(
    feature = "bevy",
    derive(bevy_ecs::prelude::Component, bevy_reflect::Reflect)
)]
#[derive(Debug, Clone, PartialEq)]
pub struct ModAccumulator {
    base: f32,
    min: f32,
    max: f32,
    layers: std::vec::Vec<(LayerKey, f32)>,
    dirty: bool,
}

impl ModAccumulator {
    /// Create a parameter with an authored `base` clamped into `[min, max]`.
    #[inline]
    pub fn new(base: f32, min: f32, max: f32) -> Self {
        Self {
            base: base.clamp(min, max),
            min,
            max,
            layers: std::vec::Vec::new(),
            dirty: true,
        }
    }

    /// The authored value, before any layer offsets — modulation reads this as
    /// its reference.
    #[inline]
    pub fn base(&self) -> f32 {
        self.base
    }

    /// The `[min, max]` range. Offsets are in the parameter's own units; the
    /// final value is clamped to this range.
    #[inline]
    pub fn range(&self) -> (f32, f32) {
        (self.min, self.max)
    }

    /// Set the authored base (projection / UI). Does not touch layers.
    #[inline]
    pub fn set_base(&mut self, value: f32) {
        let value = value.clamp(self.min, self.max);
        if self.base != value {
            self.base = value;
            self.dirty = true;
        }
    }

    /// Add or update the offset contributed by `key`. Updating an existing key
    /// replaces its offset in place — contributions never accumulate duplicates.
    #[inline]
    pub fn set_layer(&mut self, key: LayerKey, offset: f32) {
        if let Some(entry) = self.layers.iter_mut().find(|(k, _)| *k == key) {
            if entry.1 != offset {
                entry.1 = offset;
                self.dirty = true;
            }
        } else {
            self.layers.push((key, offset));
            self.dirty = true;
        }
    }

    /// Remove the offset contributed by `key`. No-op if absent.
    #[inline]
    pub fn clear_layer(&mut self, key: LayerKey) {
        let before = self.layers.len();
        self.layers.retain(|(k, _)| *k != key);
        if self.layers.len() != before {
            self.dirty = true;
        }
    }

    /// Remove every modulation layer (all keys except [`LayerKey::AUTOMATION`]).
    /// The owner may call this at the start of a modulation pass so stale
    /// sources stop contributing; automation persists. Deliberately **not** on
    /// the [`ModTarget`] trait — a single source must not be able to wipe every
    /// other contributor.
    #[inline]
    pub fn clear_mod_layers(&mut self) {
        let before = self.layers.len();
        self.layers.retain(|(k, _)| *k == LayerKey::AUTOMATION);
        if self.layers.len() != before {
            self.dirty = true;
        }
    }

    /// The computed value: `clamp(base + Σ offsets, [min, max])`. Pure — never
    /// mutates state, so repeated reads are stable and modulation cannot drift
    /// the base. Uses the crate's own [`fold`].
    #[inline]
    pub fn final_value(&self) -> f32 {
        fold(
            self.base,
            self.layers.iter().map(|(_, off)| *off),
            self.min,
            self.max,
        )
    }

    /// Whether the value changed since the last call, clearing the flag.
    #[inline]
    pub fn take_dirty(&mut self) -> bool {
        core::mem::replace(&mut self.dirty, false)
    }

    /// Whether the parameter carries no offset layers at all — i.e.
    /// `final_value() == base`.
    #[inline]
    pub fn is_unlayered(&self) -> bool {
        self.layers.is_empty()
    }
}

/// The concrete [`ModTarget`]: a keyed accumulator whose final value is
/// **mirrored into a shared [`AtomicF32`]** on every write, so the consumer
/// reads it lock-free — no per-frame readback step.
///
/// The layers live in a `Mutex<ModAccumulator>` (an `AtomicF32` holds only the
/// final `f32`, not the `Vec<(LayerKey, offset)>`). The `Mutex` is never on the
/// hot path: the modulation driver writes it once per frame (control-rate), and
/// the audio thread / UI reads the *atomic*, never the lock. Every mutation
/// flushes `final_value()` into the atomic (`Release`-ordered), so a consumer's
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
    acc: std::sync::Mutex<ModAccumulator>,
    mirror: std::sync::Arc<atomic_float::AtomicF32>,
}

impl AtomicTarget {
    /// A target over a fresh accumulator with its own atomic mirror. Seed the
    /// atomic with the initial `final_value()` (== clamped base). Read the
    /// value via [`final_value`](ModTarget::final_value) or hand out
    /// [`mirror`](Self::mirror).
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
    /// atomic is immediately seeded with the initial `final_value()`.
    pub fn with_mirror(
        base: f32,
        min: f32,
        max: f32,
        mirror: std::sync::Arc<atomic_float::AtomicF32>,
    ) -> Self {
        let acc = ModAccumulator::new(base, min, max);
        mirror.store(acc.final_value(), core::sync::atomic::Ordering::Release);
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
    pub fn clear_mod_layers(&self) {
        let mut acc = self.acc.lock().unwrap();
        acc.clear_mod_layers();
        self.flush(&acc);
    }

    #[inline]
    fn flush(&self, acc: &ModAccumulator) {
        self.mirror
            .store(acc.final_value(), core::sync::atomic::Ordering::Release);
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
        let mut acc = self.acc.lock().unwrap();
        acc.set_layer(key, offset);
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

    // ── Summation semantics (ported from dawai's ModParam regression tests) ──

    #[test]
    fn two_layers_sum_not_compound() {
        // Base 1.0 in [0,2]; two contributors +0.2 and -0.5.
        let mut p = ModAccumulator::new(1.0, 0.0, 2.0);
        p.set_layer(MOD_A, 0.2);
        p.set_layer(MOD_B, -0.5);
        let a = p.final_value();

        // Reverse order → same result.
        let mut q = ModAccumulator::new(1.0, 0.0, 2.0);
        q.set_layer(MOD_B, -0.5);
        q.set_layer(MOD_A, 0.2);
        let b = q.final_value();

        assert!((a - b).abs() < 1e-6);
        assert!((a - 0.7).abs() < 1e-6, "1.0 + 0.2 - 0.5 = 0.7, got {a}");
        assert_eq!(p.base(), 1.0, "base untouched by modulation");
    }

    #[test]
    fn reasserting_a_key_overwrites_not_accumulates() {
        let mut p = ModAccumulator::new(0.8, 0.0, 2.0);
        for _ in 0..1000 {
            p.set_layer(MOD_A, 0.3); // re-assert the same key every "frame"
        }
        assert!(
            (p.final_value() - 1.1).abs() < 1e-6,
            "must stay base+offset = 1.1, got {}",
            p.final_value()
        );
        assert_eq!(p.base(), 0.8);
    }

    #[test]
    fn clearing_a_key_drops_to_base() {
        let mut p = ModAccumulator::new(0.6, 0.0, 2.0);
        p.set_layer(MOD_A, 0.4);
        assert!((p.final_value() - 1.0).abs() < 1e-6);
        p.clear_layer(MOD_A);
        assert!((p.final_value() - 0.6).abs() < 1e-6, "back to base");
        assert!(p.is_unlayered());
    }

    #[test]
    fn clamps_to_range() {
        let mut p = ModAccumulator::new(1.0, 0.0, 2.0);
        p.set_layer(MOD_A, 5.0);
        assert!((p.final_value() - 2.0).abs() < 1e-6); // clamp high
        p.set_layer(MOD_A, -5.0);
        assert!(p.final_value().abs() < 1e-6); // clamp low
    }

    #[test]
    fn clear_mod_layers_keeps_automation() {
        let mut p = ModAccumulator::new(0.0, -1.0, 1.0);
        p.set_layer(LayerKey::AUTOMATION, 0.5);
        p.set_layer(MOD_A, 0.2);
        p.clear_mod_layers();
        assert!((p.final_value() - 0.5).abs() < 1e-6, "automation survives");
    }

    #[test]
    fn take_dirty_reports_movement_once() {
        let mut p = ModAccumulator::new(0.0, 0.0, 1.0);
        assert!(p.take_dirty()); // dirty on construction
        assert!(!p.take_dirty()); // cleared
        p.set_layer(MOD_A, 0.1);
        assert!(p.take_dirty());
        assert!(!p.take_dirty());
    }

    // ── AtomicTarget as a ModTarget ──

    #[test]
    fn atomic_target_is_a_mod_target() {
        let t = AtomicTarget::new(1000.0, 0.0, 2000.0);
        assert_eq!(t.range(), (0.0, 2000.0));
        assert_eq!(t.base(), 1000.0);
        t.accumulate(MOD_A, 500.0);
        assert!((t.final_value() - 1500.0).abs() < 1e-6);
        t.accumulate(MOD_A, 200.0); // re-assert overwrites
        assert!((t.final_value() - 1200.0).abs() < 1e-6);
        t.clear(MOD_A);
        assert!((t.final_value() - 1000.0).abs() < 1e-6);
    }

    #[test]
    fn mirror_reflects_every_write_lock_free() {
        // The mirror atomic is what a consumer (audio thread / UI) reads.
        let t = AtomicTarget::new(1000.0, 0.0, 2000.0);
        let m = t.mirror();
        assert!(
            (m.load(Ordering::Acquire) - 1000.0).abs() < 1e-6,
            "seeded to base"
        );
        t.accumulate(MOD_A, 500.0);
        assert!(
            (m.load(Ordering::Acquire) - 1500.0).abs() < 1e-6,
            "reflects the write"
        );
        t.clear(MOD_A);
        assert!(
            (m.load(Ordering::Acquire) - 1000.0).abs() < 1e-6,
            "reflects the clear"
        );
        t.set_base(1200.0);
        assert!(
            (m.load(Ordering::Acquire) - 1200.0).abs() < 1e-6,
            "reflects the base"
        );
    }

    #[test]
    fn with_mirror_drives_an_existing_atomic() {
        // The cascade / native-param case: mirror into an atomic the consumer
        // already holds (e.g. another modulator's depth, or a node's param).
        let param: Arc<atomic_float::AtomicF32> = Arc::new(atomic_float::AtomicF32::new(0.0));
        let t = AtomicTarget::with_mirror(0.5, 0.0, 1.0, param.clone());
        assert!(
            (param.load(Ordering::Acquire) - 0.5).abs() < 1e-6,
            "seeded on construction"
        );
        t.accumulate(MOD_A, 0.3);
        // The consumer reads its OWN atomic and sees the modulated value.
        assert!((param.load(Ordering::Acquire) - 0.8).abs() < 1e-6);
    }
}
