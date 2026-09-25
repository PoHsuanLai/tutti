//! [`LayeredCurve`] — a [`Curve`] that sums other [`Curve`]s.
//!
//! The rate-generic accumulator: `final(beat) = clamp(base + Σ layer(beat),
//! [min, max])`. Each layer is an `Arc<dyn Curve>` — an automation envelope, an
//! LFO, a constant — keyed by its contributor [`LayerKey`] so a source updates
//! its own layer in place (never doubling up) and can be cleared independently.
//!
//! Because it is **itself** a [`Curve`], the same value is available at any rate
//! the consumer samples at:
//! - a native sink samples `value_at(beat_now)` once per frame and mirrors the
//!   scalar into an atomic (frame-rate — see [`crate::AtomicTarget`]);
//! - a plugin sink hands the whole `LayeredCurve` to its per-block parameter
//!   producer, which samples `value_at(beat)` several times per audio block
//!   (sub-block — the LFO layers trace a smooth ramp, the automation layer sums
//!   in, all at the block's real beats).
//!
//! One accumulator, one summation rule, the rate chosen by whoever reads it.
//!
//! `U` is the target param's unit (`Hz`, `Amplitude`, `Db`, …): `base`/`min`/`max`
//! are typed, so the range that defines the param's space can't be seeded in the
//! wrong unit. The offsets sum in `f32` (the layers are already unit-erased
//! `dyn Curve`s), and the clamp happens in `f32` between the `U`→`f32` range
//! bounds — so `U` needs only `Into<f32>`, no unit arithmetic (which lets
//! non-additive units like `Amplitude` participate).

use std::sync::Arc;

use tutti_types::Beat;

use crate::curve::Curve;
use crate::id::LayerKey;

/// One contribution to a [`LayeredCurve`]. Either a plain `f32` (a frame-rate
/// value — an automation snapshot, a native modulator the driver already
/// collapsed) or a beat-varying [`Curve`] (an LFO sampled sub-block).
///
/// The scalar case is a bare `f32`, **not** boxed in an `Arc` — the native
/// control-rate path adds one of these per edge per frame, so keeping it
/// allocation-free matters. The curve case pays the `Arc` only when a
/// contributor genuinely varies within a frame.
#[derive(Clone)]
enum Layer {
    Scalar(f32),
    Curve(Arc<dyn Curve>),
}

impl Layer {
    /// This layer's offset at `beat`, or `None` if it has no value there (a
    /// disabled/empty curve — the empty-vs-zero distinction). A scalar always
    /// contributes.
    #[inline]
    fn offset_at(&self, beat: Beat) -> Option<f32> {
        match self {
            Layer::Scalar(v) => Some(*v),
            Layer::Curve(c) => c.value_at(beat),
        }
    }
}

/// A keyed sum of contribution layers over a typed base + range. See the module doc.
///
/// `Clone` is cheap — an `Arc` bump per *curve* layer, a copy per scalar — so a
/// consumer that wants lock-free reads can hold it in an `ArcSwap` and swap a
/// fresh snapshot on each control-rate edit (what the plugin sub-block sink does).
#[derive(Clone)]
pub struct LayeredCurve<U> {
    base: U,
    min: U,
    max: U,
    layers: Vec<(LayerKey, Layer)>,
}

// A `Curve` layer is `Arc<dyn Curve>` (not `Debug`), so derive can't apply — a
// manual impl prints the base/range and the layer keys, eliding the curves.
impl<U: core::fmt::Debug> core::fmt::Debug for LayeredCurve<U> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LayeredCurve")
            .field("base", &self.base)
            .field("min", &self.min)
            .field("max", &self.max)
            .field(
                "layer_keys",
                &self.layers.iter().map(|(k, _)| k).collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl<U: Into<f32> + From<f32> + Copy> LayeredCurve<U> {
    /// A layered curve seeded with an authored `base`, **clamped into `[min,
    /// max]`**. Starts with no layers — `value_at` is just the base until a
    /// source is added. Clamping at the boundary is what keeps `base()` in range
    /// for consumers that offset against it (an automation lane's
    /// `value - base`, the app-side driver): an unclamped base lets a downward
    /// offset bite into out-of-range headroom instead of the clamped ceiling.
    pub fn new(base: U, min: U, max: U) -> Self {
        Self {
            base: Self::clamp_base(base, min, max),
            min,
            max,
            layers: Vec::new(),
        }
    }

    /// Clamp an authored base into `[min, max]` (in `f32` space, since `U` may be
    /// a non-orderable/non-additive unit like `Amplitude`).
    #[inline]
    fn clamp_base(base: U, min: U, max: U) -> U {
        U::from(base.into().clamp(min.into(), max.into()))
    }

    /// The authored base value (before any layers), erased to `f32`. Always in
    /// range (clamped at construction / `set_base`).
    #[inline]
    pub fn base(&self) -> f32 {
        self.base.into()
    }

    /// The `(min, max)` clamp range, erased to `f32`.
    #[inline]
    pub fn range(&self) -> (f32, f32) {
        (self.min.into(), self.max.into())
    }

    /// Set the authored base (the one write reserved for the base's owner —
    /// projection / UI — never a modulation source). Clamped into `[min, max]`.
    #[inline]
    pub fn set_base(&mut self, base: U) {
        self.base = Self::clamp_base(base, self.min, self.max);
    }

    /// Add or replace a **scalar** (frame-rate) contribution under `key` — the
    /// allocation-free path for a native modulator offset or an automation
    /// snapshot. Re-inserting the same key updates in place.
    pub fn set_scalar_layer(&mut self, key: LayerKey, offset: f32) {
        self.upsert(key, Layer::Scalar(offset));
    }

    /// Add or replace a **beat-varying** [`Curve`] contribution under `key` — an
    /// LFO, an automation envelope. Re-inserting the same key updates in place.
    pub fn set_layer(&mut self, key: LayerKey, curve: Arc<dyn Curve>) {
        self.upsert(key, Layer::Curve(curve));
    }

    #[inline]
    fn upsert(&mut self, key: LayerKey, layer: Layer) {
        if let Some(slot) = self.layers.iter_mut().find(|(k, _)| *k == key) {
            slot.1 = layer;
        } else {
            self.layers.push((key, layer));
        }
    }

    /// Remove the layer contributed by `key`. No-op if absent.
    pub fn clear_layer(&mut self, key: LayerKey) {
        self.layers.retain(|(k, _)| *k != key);
    }

    /// Remove every modulation layer (all keys except [`LayerKey::AUTOMATION`]).
    /// The owner may call this at the start of a modulation pass so stale sources
    /// stop contributing; automation persists.
    pub fn clear_mod_layers(&mut self) {
        self.layers.retain(|(k, _)| *k == LayerKey::AUTOMATION);
    }

    /// Whether any layers are present (an unlayered curve reads exactly `base`).
    #[inline]
    pub fn is_unlayered(&self) -> bool {
        self.layers.is_empty()
    }
}

impl<U: Into<f32> + From<f32> + Copy + Send + Sync + 'static> Curve for LayeredCurve<U> {
    /// `clamp(base + Σ layer(beat), [min, max])`. A layer with no value at this
    /// beat (`None` — disabled / empty) contributes nothing, preserving the
    /// empty-vs-zero distinction each layer carries.
    fn value_at(&self, beat: Beat) -> Option<f32> {
        let sum: f32 = self
            .layers
            .iter()
            .filter_map(|(_, l)| l.offset_at(beat))
            .sum();
        let (min, max) = self.range();
        Some((self.base.into() + sum).clamp(min, max))
    }

    /// A value is already a copy; only a curve **layer** that reads live state
    /// needs freezing, and then the whole sum is rebuilt around its frozen copy.
    fn frozen(&self) -> Option<Arc<dyn Curve>> {
        let mut any = false;
        let layers = self
            .layers
            .iter()
            .map(|(k, l)| match l {
                Layer::Curve(c) => match c.frozen() {
                    Some(f) => {
                        any = true;
                        (*k, Layer::Curve(f))
                    }
                    None => (*k, l.clone()),
                },
                Layer::Scalar(_) => (*k, l.clone()),
            })
            .collect();
        any.then(|| {
            Arc::new(Self {
                base: self.base,
                min: self.min,
                max: self.max,
                layers,
            }) as Arc<dyn Curve>
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_types::{Amplitude, Depth, Hz, Mix};

    /// A constant offset as a `Curve` (automation snapshot / static contribution).
    struct ConstOffset(f32);
    impl Curve for ConstOffset {
        fn value_at(&self, _beat: Beat) -> Option<f32> {
            Some(self.0)
        }
    }

    /// A ramp offset: `slope · beat` — stands in for a temporal (beat-varying)
    /// layer like an LFO, so the sum is exercised at multiple beats.
    struct Ramp(f32);
    impl Curve for Ramp {
        fn value_at(&self, beat: Beat) -> Option<f32> {
            Some(self.0 * beat.get() as f32)
        }
    }

    #[test]
    fn unlayered_reads_base() {
        let lc = LayeredCurve::new(Hz(1000.0), Hz(20.0), Hz(20000.0));
        assert!(lc.is_unlayered());
        assert_eq!(lc.value_at(Beat(5.0)), Some(1000.0));
    }

    #[test]
    fn base_is_clamped_into_range() {
        // An out-of-range authored base is clamped at the boundary, so base()
        // stays in range for consumers that offset against it. The third
        // assertion is the one that bites: an unclamped base lets a downward
        // offset spend out-of-range headroom instead of moving the value.
        let mut lc = LayeredCurve::new(Mix(1.5), Mix::DRY, Mix::WET);
        assert_eq!(lc.base(), 1.0, "over-range base clamped at construction");
        lc.set_base(Mix(-0.3));
        assert_eq!(lc.base(), 0.0, "under-range base clamped on set_base");
        // With base clamped to 1.0, a -0.3 offset lands at 0.7 (not swallowed by
        // out-of-range headroom).
        lc.set_base(Mix(1.5));
        lc.set_layer(LayerKey(1), Arc::new(ConstOffset(-0.3)));
        assert!((lc.value_at(Beat(0.0)).unwrap() - 0.7).abs() < 1e-6);
    }

    #[test]
    fn layers_sum_over_base_order_independent() {
        let mut a = LayeredCurve::new(Amplitude(0.5), Amplitude::SILENT, Amplitude(2.0));
        a.set_layer(LayerKey(1), Arc::new(ConstOffset(0.2)));
        a.set_layer(LayerKey(2), Arc::new(ConstOffset(-0.1)));
        let mut b = LayeredCurve::new(Amplitude(0.5), Amplitude::SILENT, Amplitude(2.0));
        b.set_layer(LayerKey(2), Arc::new(ConstOffset(-0.1)));
        b.set_layer(LayerKey(1), Arc::new(ConstOffset(0.2)));
        // 0.5 + 0.2 - 0.1 = 0.6, same regardless of insertion order.
        assert_eq!(a.value_at(Beat(0.0)), b.value_at(Beat(0.0)));
        assert!((a.value_at(Beat(0.0)).unwrap() - 0.6).abs() < 1e-6);
    }

    #[test]
    fn temporal_layer_is_re_evaluated_per_beat() {
        // The whole point: a beat-varying layer traces a curve when sampled at
        // different beats — NOT a frozen scalar.
        let mut lc = LayeredCurve::new(Hz(100.0), Hz(0.0), Hz(1000.0));
        lc.set_layer(LayerKey(1), Arc::new(Ramp(10.0))); // +10 Hz per beat
        assert_eq!(lc.value_at(Beat(0.0)), Some(100.0));
        assert_eq!(lc.value_at(Beat(5.0)), Some(150.0));
        assert_eq!(lc.value_at(Beat(20.0)), Some(300.0));
    }

    #[test]
    fn scalar_layers_sum_and_mix_with_curves() {
        // The allocation-free scalar lane sums identically to a curve layer, and
        // the two representations coexist (a native scalar modulator + an LFO).
        let mut lc = LayeredCurve::new(Hz(100.0), Hz(0.0), Hz(1000.0));
        lc.set_scalar_layer(LayerKey::AUTOMATION, 50.0); // bare f32, no Arc
        lc.set_layer(LayerKey(1), Arc::new(Ramp(10.0))); // beat-varying
                                                         // base 100 + scalar 50 + ramp@beat2 (20) = 170.
        assert_eq!(lc.value_at(Beat(2.0)), Some(170.0));
        // Scalar upsert replaces in place (not stacks).
        lc.set_scalar_layer(LayerKey::AUTOMATION, 20.0);
        assert_eq!(lc.value_at(Beat(0.0)), Some(120.0)); // 100 + 20 + ramp@0 (0)
                                                         // A curve key can replace a scalar key and vice-versa under the same key.
        lc.set_layer(LayerKey::AUTOMATION, Arc::new(ConstOffset(30.0)));
        assert_eq!(lc.value_at(Beat(0.0)), Some(130.0)); // 100 + 30
    }

    #[test]
    fn constant_and_temporal_layers_compose() {
        // Automation (constant) + LFO (temporal) sum in one accumulator — the
        // composition the plugin path lacked.
        let mut lc = LayeredCurve::new(Hz(100.0), Hz(0.0), Hz(1000.0));
        lc.set_layer(LayerKey::AUTOMATION, Arc::new(ConstOffset(50.0)));
        lc.set_layer(LayerKey(1), Arc::new(Ramp(10.0)));
        // base 100 + automation 50 + ramp@beat3 (30) = 180.
        assert_eq!(lc.value_at(Beat(3.0)), Some(180.0));
    }

    #[test]
    fn set_layer_replaces_in_place() {
        let mut lc = LayeredCurve::new(Depth::NONE, Depth::INVERTED, Depth::FULL);
        lc.set_layer(LayerKey(1), Arc::new(ConstOffset(0.3)));
        lc.set_layer(LayerKey(1), Arc::new(ConstOffset(0.4))); // same key → replace
        assert!((lc.value_at(Beat(0.0)).unwrap() - 0.4).abs() < 1e-6); // not 0.7
    }

    #[test]
    fn clear_layer_drops_to_remaining() {
        let mut lc = LayeredCurve::new(Hz(500.0), Hz(0.0), Hz(1000.0));
        lc.set_layer(LayerKey(1), Arc::new(ConstOffset(100.0)));
        lc.set_layer(LayerKey(2), Arc::new(ConstOffset(50.0)));
        assert_eq!(lc.value_at(Beat(0.0)), Some(650.0));
        lc.clear_layer(LayerKey(1));
        assert_eq!(lc.value_at(Beat(0.0)), Some(550.0));
    }

    #[test]
    fn value_clamps_to_range() {
        let mut lc = LayeredCurve::new(Mix(0.9), Mix::DRY, Mix::WET);
        lc.set_layer(LayerKey(1), Arc::new(ConstOffset(0.5)));
        assert_eq!(lc.value_at(Beat(0.0)), Some(1.0)); // 1.4 clamped
        lc.set_layer(LayerKey(1), Arc::new(ConstOffset(-2.0)));
        assert_eq!(lc.value_at(Beat(0.0)), Some(0.0)); // -1.1 clamped
    }
}
