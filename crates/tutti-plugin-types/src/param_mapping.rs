//! One parameter's map between its normalized position and its plain value.

use crate::Normalized;

/// One parameter's map between its normalized position and its plain value, as
/// its format defines it.
///
/// Each format implements it on its own type, at its own edge: CLAP's
/// `ParamBounds` (a line over the declared range, stepped values on their
/// integer), AU's `ParamBounds` (a line over the declared range, in `f32`),
/// VST2's integer range (a line between its ends) and VST3's controller (the
/// plugin's own `normalizedParamToPlain`, whatever its shape). No shared code
/// computes a conversion: [`Control::parameter_plain`] and
/// [`Control::parameter_normalized`] build the parameter's mapping and ask it.
///
/// A mapping answers every input; the laws below hold for all of them, and
/// `contract::mapping_contract!` (feature `contract`) checks them over a sweep
/// of positions and edges:
///
/// 1. **Finite.** Every position on `0..=1` has a finite plain value.
/// 2. **Bounded.** Every finite plain value maps back onto `0..=1`, clamped
///    where it lies outside the map's range; a NaN or infinite plain value is a
///    position on `0..=1` too, never an escaping NaN (it is `0.0`, the
///    convention for "no value to read").
/// 3. **Stable from the plain side.** `plain(normalized(p)) == p` for every `p`
///    the map produces, bit for bit for a stepped map (CLAP's integer steps)
///    and within rounding of the map's own width for a continuous one.
/// 4. **Monotonic.** Over increasing positions the plain value never changes
///    direction: a map increases, or decreases (VST3's `tone` in the shared
///    test plugin), or is constant. `normalized` is monotonic the same way.
///
/// A mapping that cannot be built (an id the plugin does not declare, a VST2
/// parameter with no integer range, no VST3 controller) is `None` at the
/// constructor of the format, not a failure of these two calls.
///
/// [`Control::parameter_plain`]: crate::Control::parameter_plain
/// [`Control::parameter_normalized`]: crate::Control::parameter_normalized
pub trait ParamMapping {
    /// The plain value of the position `value`: finite, in the map's range.
    #[must_use]
    fn plain(&self, value: Normalized) -> f64;

    /// The position of the plain value `plain`, clamped onto `0..=1`. NaN is
    /// `0.0`.
    #[must_use]
    fn normalized(&self, plain: f64) -> Normalized;
}
