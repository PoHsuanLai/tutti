//! Plugin parameter descriptors — `ParameterInfo` and its parts.
//!
//! Shared value vocabulary across the host crates and the `tutti-plugin` IPC
//! protocol. Automation points/queues live in [`crate::automation`]. The
//! `Serialize`/`Deserialize` derives are gated behind the `serde` feature.
//!
//! The formats disagree about parameters more than about anything else, so the
//! shape here is chosen to keep the disagreement visible rather than averaged
//! away:
//!
//! - [`ParamRange`] is a sum type because there is no safe number to substitute
//!   when a format declares no range. Every pair a host could invent is a claim
//!   the ABI never made — so the `Normalized` arm has no bounds to read.
//! - [`ParamSteps`] separates "continuous" from "nobody said", which a bare
//!   `step_count: u32` fuses at zero.
//! - [`ParamFlags`] is paired with a `known` mask so a flag the format never
//!   reported reads as [`None`] rather than as `false`.

use bitflags::bitflags;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// What the plugin declared about a parameter's value range.
///
/// A sum type rather than bounds plus a discriminant: with a flag, the
/// misleading `0.0..1.0` stays present and every consumer has to remember to
/// check a sibling field. Here the `Normalized` arm has no bounds *to read*.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum ParamRange {
    /// The ABI speaks normalized `0..=1` and the plugin declared no plain range.
    ///
    /// VST2 that declines `effGetParameterProperties`; VST3 with no edit
    /// controller, or whose `normalizedParamToPlain` is not monotonic.
    Normalized {
        /// Default, normalized `0..=1`.
        default: f64,
    },
    /// The plugin's declared range, in the unit [`ParameterInfo::unit`] names.
    ///
    /// AU and CLAP report this natively; VST3 via `normalizedParamToPlain`;
    /// VST2 via `effGetParameterProperties`.
    Plain {
        /// Minimum legal value.
        min: f64,
        /// Maximum legal value.
        max: f64,
        /// Default, in the same units as `min`/`max`.
        default: f64,
    },
}

impl Default for ParamRange {
    fn default() -> Self {
        Self::Normalized { default: 0.0 }
    }
}

impl ParamRange {
    /// The plugin's declared bounds, or [`None`] if it declared none.
    pub fn bounds(&self) -> Option<(f64, f64)> {
        match *self {
            Self::Normalized { .. } => None,
            Self::Plain { min, max, .. } => Some((min, max)),
        }
    }

    /// The default value, in whichever domain this range speaks.
    pub fn default_value(&self) -> f64 {
        match *self {
            Self::Normalized { default } | Self::Plain { default, .. } => default,
        }
    }

    /// Map a normalized `0..=1` value onto this parameter's domain.
    ///
    /// For [`Normalized`](Self::Normalized) this is the identity, clamped —
    /// there is nothing to map onto. For [`Plain`](Self::Plain) it maps the
    /// declared endpoints, linearly and deliberately: a format that also
    /// declares a taper (AU's `kAudioUnitParameterFlag_DisplayLogarithmic`)
    /// must apply it on top.
    ///
    /// A degenerate range (`max <= min`, or either bound non-finite) yields
    /// `min` when finite and `0.0` otherwise. So this never returns a
    /// non-finite value, nor one outside what the plugin declared.
    ///
    /// # NaN
    ///
    /// Every input is untrusted: the bounds are `Deserialize`d straight off the
    /// IPC wire into plain `f64`s, and `normalized` is a wire-supplied
    /// automation value. Neither `clamp` nor `<=` rejects NaN — `f64::clamp`
    /// *returns* NaN for a NaN input, and `max <= min` is `false` when either
    /// bound is NaN, so the degenerate-range guard does not cover it. NaN is
    /// therefore checked explicitly: this output reaches `AudioUnitSetParameter`
    /// on the audio path, and a NaN in a live filter coefficient does not stay
    /// confined to one parameter.
    ///
    /// Only NaN needs the check. `clamp` handles ±∞ correctly — `+∞` is "as high
    /// as this parameter goes" — so an infinite *value* lands on an endpoint. An
    /// infinite *bound* is rejected, because there is no endpoint to land on.
    pub fn to_plain(&self, normalized: f64) -> f64 {
        let n = clamp_unit(normalized);
        match *self {
            Self::Normalized { .. } => n,
            Self::Plain { min, max, .. } => {
                let Some((min, max)) = finite_bounds(min, max) else {
                    return 0.0;
                };
                if max <= min {
                    return min;
                }
                min + n * (max - min)
            }
        }
    }

    /// Inverse of [`to_plain`](Self::to_plain): map a value in this parameter's
    /// domain onto normalized `0..=1`.
    ///
    /// A degenerate or non-finite range yields `0.0`, as does a NaN input — see
    /// [`to_plain`](Self::to_plain) for why NaN is checked rather than clamped.
    pub fn to_normalized(&self, plain: f64) -> f64 {
        match *self {
            Self::Normalized { .. } => clamp_unit(plain),
            Self::Plain { min, max, .. } => {
                let Some((min, max)) = finite_bounds(min, max) else {
                    return 0.0;
                };
                if max <= min || plain.is_nan() {
                    return 0.0;
                }
                (plain.clamp(min, max) - min) / (max - min)
            }
        }
    }
}

/// NaN checked before the clamp, which would pass it through.
fn clamp_unit(v: f64) -> f64 {
    if v.is_nan() {
        0.0
    } else {
        v.clamp(0.0, 1.0)
    }
}

/// `(min, max)` if both are finite, else [`None`].
///
/// Infinities are rejected alongside NaN: with an infinite bound the endpoint
/// map yields either an infinity or, at `n == 0`, a NaN — no more usable than a
/// NaN bound.
fn finite_bounds(min: f64, max: f64) -> Option<(f64, f64)> {
    (min.is_finite() && max.is_finite()).then_some((min, max))
}

/// How many positions a parameter has.
///
/// [`Unknown`](Self::Unknown) is separate from [`Continuous`](Self::Continuous)
/// because a bare count fuses them at zero: VST2 without
/// `effGetParameterProperties` reports nothing, which is not the same as
/// reporting "freely variable".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum ParamSteps {
    /// The format reported no step information.
    #[default]
    Unknown,
    /// Any value in range. VST3 `stepCount == 0`.
    Continuous,
    /// Two positions. AU `kAudioUnitParameterUnit_Boolean`, VST3 `stepCount == 1`.
    Toggle,
    /// `n` positions indexing a named list, `n >= 2`.
    ///
    /// AU `kAudioUnitParameterUnit_Indexed`, CLAP `STEPPED`, VST3 `stepCount + 1`.
    Enumerated(u32),
}

impl ParamSteps {
    /// Build from a span, collapsing the degenerate cases.
    ///
    /// A span below 1 has no positions to step between, so it is
    /// [`Continuous`](Self::Continuous) rather than an invented count; a span of
    /// exactly 1 is two positions, which is a [`Toggle`](Self::Toggle).
    pub fn from_span(span: f64) -> Self {
        if !span.is_finite() || span < 1.0 {
            Self::Continuous
        } else if span == 1.0 {
            Self::Toggle
        } else {
            // `span + 1` positions for an inclusive integer range, saturating
            // at u32 rather than wrapping on an absurd declared span.
            Self::Enumerated((span as u64).saturating_add(1).min(u32::MAX as u64) as u32)
        }
    }

    /// Number of positions, or [`None`] if unreported or freely variable.
    pub fn count(&self) -> Option<u32> {
        match *self {
            Self::Unknown | Self::Continuous => None,
            Self::Toggle => Some(2),
            Self::Enumerated(n) => Some(n),
        }
    }
}

bitflags! {
    /// Per-parameter capabilities, in the same shape as
    /// [`Features`](crate::Features).
    ///
    /// Meaningful only alongside a `known` mask — see
    /// [`ParameterInfo::flag`]. A bit clear here means "the format reported
    /// this as false" *or* "the format never reported it", and only the mask
    /// tells those apart.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
    #[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
    pub struct ParamFlags: u32 {
        /// The host may write automation to this parameter.
        const AUTOMATABLE = 1 << 0;
        /// Readable but not writable.
        const READ_ONLY = 1 << 1;
        /// This is the plugin's bypass parameter.
        const BYPASS = 1 << 2;
        /// Should not be shown in a generic parameter list.
        const HIDDEN = 1 << 3;
        /// Wraps around at its extremes (VST3 `kIsWrapAround`, CLAP `PERIODIC`).
        const WRAP = 1 << 4;

        // --- CLAP-only below. No other hosted format reports these. ---

        /// Accepts modulation, which is distinct from automation in CLAP.
        const MODULATABLE = 1 << 5;
        /// Automation or modulation may target a single note id.
        const PER_NOTE_ID = 1 << 6;
        /// …a single key.
        const PER_KEY = 1 << 7;
        /// …a single channel.
        const PER_CHANNEL = 1 << 8;
        /// …a single port.
        const PER_PORT = 1 << 9;
    }
}

/// A plugin's own name for one of its parameters: VST3 `ParamID`, CLAP
/// `clap_id`, AU `AudioUnitParameterID`.
///
/// **Opaque.** The number is chosen by the plugin and is meaningful only
/// against the instance that reported it — plenty of plugins derive it from a
/// hash of the parameter name. It is not an index, not dense, not ordered, and
/// two plugins may use the same number for unrelated parameters.
///
/// That is the whole reason this is a newtype rather than a `u32`: the type has
/// no algebra, deliberately. There is no `Add`, no `From<usize>`, no `Step`, so
/// `id + 1` and `for id in 0..n` do not compile. A `u32` in a struct field
/// invites exactly those, and VST2 — whose address genuinely *is* a dense index
/// — is the one place they would even seem to work.
///
/// [`Ord`] is derived and is arbitrary-but-total, for map keys and binary
/// search only (AU's parameter-bounds table sorts by it on the load path). A
/// comparison between two ids carries no meaning about the parameters; do not
/// read one as "earlier" or "lower".
///
/// Serializes as the bare `u32` it wraps, so the IPC wire and the WIT boundary
/// are unchanged — conversion happens where a format's number enters, via
/// [`new`](Self::new) / [`get`](Self::get).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize), serde(transparent))]
pub struct ParamId(u32);

impl ParamId {
    /// Wrap a format-native parameter id.
    ///
    /// The absent algebra is the point, so it is pinned here rather than only
    /// asserted in the type's docs — each of these is a way a `u32` field
    /// invites treating an opaque id as a position:
    ///
    /// ```compile_fail
    /// # use tutti_plugin_types::ParamId;
    /// let id = ParamId::new(3);
    /// let _ = id + ParamId::new(1);
    /// ```
    /// ```compile_fail
    /// # use tutti_plugin_types::ParamId;
    /// for _id in ParamId::new(0)..ParamId::new(4) {}
    /// ```
    /// ```compile_fail
    /// # use tutti_plugin_types::ParamId;
    /// let params = ["a", "b"];
    /// let _ = params[ParamId::new(0)];
    /// ```
    /// ```compile_fail
    /// # use tutti_plugin_types::ParamId;
    /// // A count is not an id: no `From<usize>` to make enumerate() fit.
    /// let _: ParamId = 0usize.into();
    /// ```
    ///
    /// What *does* compile is the deliberate crossing at a format boundary:
    ///
    /// ```
    /// # use tutti_plugin_types::ParamId;
    /// let id: ParamId = 0x4000_0001u32.into();
    /// assert_eq!(id.get(), 0x4000_0001);
    /// ```
    pub const fn new(id: u32) -> Self {
        Self(id)
    }

    /// The underlying number, for handing back to the format that issued it.
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl From<u32> for ParamId {
    fn from(id: u32) -> Self {
        Self(id)
    }
}

impl std::fmt::Display for ParamId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// One plugin parameter, as the boundary vocabulary every host crate speaks.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ParameterInfo {
    /// The plugin's own name for this parameter. See [`ParamId`] — opaque, and
    /// meaningful only against the instance that reported it.
    pub id: ParamId,
    pub name: String,
    /// Display unit (`"dB"`, `"Hz"`, …). Empty when the format carries none —
    /// CLAP has no unit string at all.
    pub unit: String,
    pub range: ParamRange,
    pub steps: ParamSteps,
    /// Capability bits. Read through [`flag`](Self::flag), not directly, so an
    /// unreported capability cannot be mistaken for a reported `false`.
    pub flags: ParamFlags,
    /// Which bits of [`flags`](Self::flags) the format actually reported.
    pub known: ParamFlags,
}

impl ParameterInfo {
    pub fn new(id: impl Into<ParamId>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            unit: String::new(),
            range: ParamRange::default(),
            steps: ParamSteps::Unknown,
            flags: ParamFlags::empty(),
            known: ParamFlags::empty(),
        }
    }

    /// Declare the plugin's own range (builder).
    pub fn with_plain_range(mut self, min: f64, max: f64, default: f64) -> Self {
        self.range = ParamRange::Plain { min, max, default };
        self
    }

    /// Declare a normalized range with the given default (builder).
    pub fn with_normalized_default(mut self, default: f64) -> Self {
        self.range = ParamRange::Normalized { default };
        self
    }

    /// Declare step information (builder).
    pub fn with_steps(mut self, steps: ParamSteps) -> Self {
        self.steps = steps;
        self
    }

    /// Report `reported` as the values of the `known` capabilities (builder).
    ///
    /// Takes both together so a format cannot set a bit it never reported, nor
    /// report a bit it left unset.
    pub fn with_flags(mut self, known: ParamFlags, reported: ParamFlags) -> Self {
        self.known = known;
        self.flags = reported & known;
        self
    }

    /// `Some(true)`/`Some(false)` if the format reported this capability,
    /// [`None`] if it did not.
    ///
    /// Pass exactly one bit; a multi-bit query answers whether *all* of them are
    /// known and set.
    pub fn flag(&self, f: ParamFlags) -> Option<bool> {
        self.known.contains(f).then(|| self.flags.contains(f))
    }

    /// Map a normalized `0..=1` value onto this parameter's domain.
    /// See [`ParamRange::to_plain`].
    pub fn to_plain(&self, normalized: f64) -> f64 {
        self.range.to_plain(normalized)
    }

    /// Inverse of [`to_plain`](Self::to_plain). See [`ParamRange::to_normalized`].
    pub fn to_normalized(&self, plain: f64) -> f64 {
        self.range.to_normalized(plain)
    }

    /// Infers a *display* scale from the step count and unit string.
    ///
    /// Distinct from [`to_plain`](Self::to_plain), which maps declared
    /// endpoints. A logarithmic answer here is a rendering hint, not a taper the
    /// plugin declared, so it must not be reused for value conversion.
    pub fn to_range(&self) -> audio_automation::ParameterRange {
        use audio_automation::{ParameterRange, ParameterScale};

        let (min, max) = self.range.bounds().unwrap_or((0.0, 1.0));
        let scale = match self.steps {
            ParamSteps::Toggle => ParameterScale::Toggle,
            ParamSteps::Enumerated(_) => ParameterScale::Integer,
            _ if is_log_unit(&self.unit) && min > 0.0 => ParameterScale::Logarithmic,
            _ => ParameterScale::Linear,
        };

        ParameterRange::new(
            min as f32,
            max as f32,
            self.range.default_value() as f32,
            scale,
        )
    }
}

fn is_log_unit(unit: &str) -> bool {
    unit.contains("dB") || unit.contains("Hz") || unit.contains("hz")
}

#[cfg(test)]
mod tests {
    use super::*;
    use audio_automation::ParameterScale;

    /// A `Plain` parameter maps normalized input onto its declared range.
    #[test]
    fn a_plain_parameter_maps_onto_its_declared_range() {
        let p = ParameterInfo::new(1, "Cutoff").with_plain_range(20.0, 20_000.0, 20.0);
        assert_eq!(p.to_plain(0.0), 20.0);
        assert_eq!(p.to_plain(1.0), 20_000.0);
        assert_eq!(p.to_normalized(20_000.0), 1.0);
    }

    /// A `Normalized` parameter passes its input through, clamped.
    #[test]
    fn a_normalized_parameter_passes_its_input_through() {
        let p = ParameterInfo::new(1, "Mix").with_normalized_default(0.5);
        assert_eq!(p.to_plain(0.25), 0.25);
        assert_eq!(p.to_plain(1.5), 1.0, "out of range must still clamp");
        assert_eq!(p.to_normalized(0.25), 0.25);
    }

    /// Bounds of `0.0..1.0` that the plugin *declared* stay distinguishable
    /// from a parameter that declared none.
    ///
    /// This is what the old `min_value`/`max_value` pair could not express: the
    /// numbers were identical and only a sibling discriminant told them apart.
    /// Now the `Normalized` arm has no bounds at all.
    #[test]
    fn a_declared_unit_range_is_not_the_same_as_no_range() {
        let declared = ParameterInfo::new(1, "Blend").with_plain_range(0.0, 1.0, 0.0);
        let undeclared = ParameterInfo::new(1, "Blend").with_normalized_default(0.0);
        assert_eq!(declared.range.bounds(), Some((0.0, 1.0)));
        assert_eq!(undeclared.range.bounds(), None);
    }

    /// An unreported flag reads as `None`, not as `false`.
    ///
    /// The reason the `known` mask exists: AUv2 has no automation metadata, so
    /// reporting `automatable: false` would be a claim the ABI never made — and
    /// reporting `true` (which `ALL_AUTOMATABLE` used to do) is worse.
    #[test]
    fn an_unreported_flag_is_not_a_false_flag() {
        let p =
            ParameterInfo::new(1, "Gain").with_flags(ParamFlags::READ_ONLY, ParamFlags::empty());

        assert_eq!(p.flag(ParamFlags::READ_ONLY), Some(false));
        assert_eq!(
            p.flag(ParamFlags::AUTOMATABLE),
            None,
            "a capability the format never reported must not read as false"
        );
    }

    /// `with_flags` cannot set a bit outside the reported mask.
    #[test]
    fn with_flags_cannot_report_an_unknown_bit() {
        let p = ParameterInfo::new(1, "Gain").with_flags(
            ParamFlags::READ_ONLY,
            ParamFlags::READ_ONLY | ParamFlags::BYPASS,
        );
        assert_eq!(p.flag(ParamFlags::READ_ONLY), Some(true));
        assert_eq!(
            p.flag(ParamFlags::BYPASS),
            None,
            "a bit outside the known mask must not become readable"
        );
        assert!(!p.flags.contains(ParamFlags::BYPASS));
    }

    /// `Unknown` and `Continuous` are different answers.
    #[test]
    fn unknown_steps_are_not_continuous_steps() {
        assert_ne!(ParamSteps::Unknown, ParamSteps::Continuous);
        assert_eq!(ParamSteps::Unknown.count(), None);
        assert_eq!(ParamSteps::Continuous.count(), None);
    }

    /// A span maps onto positions, with the degenerate cases collapsed.
    #[test]
    fn a_span_becomes_positions() {
        assert_eq!(ParamSteps::from_span(0.0), ParamSteps::Continuous);
        assert_eq!(ParamSteps::from_span(0.5), ParamSteps::Continuous);
        assert_eq!(ParamSteps::from_span(f64::NAN), ParamSteps::Continuous);
        assert_eq!(ParamSteps::from_span(f64::INFINITY), ParamSteps::Continuous);
        assert_eq!(ParamSteps::from_span(1.0), ParamSteps::Toggle);
        assert_eq!(ParamSteps::from_span(10.0), ParamSteps::Enumerated(11));
    }

    /// A `Normalized` parameter still refuses NaN.
    #[test]
    fn the_normalized_path_still_rejects_nan() {
        let p = ParameterInfo::new(1, "Mix").with_normalized_default(0.0);
        assert!(p.to_plain(f64::NAN).is_finite());
        assert!(p.to_normalized(f64::NAN).is_finite());
        assert_eq!(p.to_plain(f64::INFINITY), 1.0);
        assert_eq!(p.to_plain(f64::NEG_INFINITY), 0.0);
    }

    /// The range survives the bincode wire format both IPC peers speak.
    ///
    /// Note what this does *not* prove: bincode is non-self-describing, so a
    /// peer built against an older layout produces a payload that fails to
    /// decode rather than defaulting. `PROTOCOL_VERSION` governs that.
    #[cfg(feature = "serde")]
    #[test]
    fn the_range_survives_the_bincode_round_trip() {
        for want in [
            ParamRange::Normalized { default: 0.25 },
            ParamRange::Plain {
                min: -96.0,
                max: 6.0,
                default: 0.0,
            },
        ] {
            let mut info = ParameterInfo::new(1, "Gain");
            info.range = want;
            let bytes = bincode::serialize(&info).expect("serialize");
            let back: ParameterInfo = bincode::deserialize(&bytes).expect("deserialize");
            assert_eq!(back.range, want);
        }
    }

    /// The `known` mask survives the wire too — a flag that arrives without its
    /// mask bit would read as a reported `false`.
    #[cfg(feature = "serde")]
    #[test]
    fn the_known_mask_survives_the_bincode_round_trip() {
        let info =
            ParameterInfo::new(1, "Gain").with_flags(ParamFlags::READ_ONLY, ParamFlags::READ_ONLY);
        let bytes = bincode::serialize(&info).expect("serialize");
        let back: ParameterInfo = bincode::deserialize(&bytes).expect("deserialize");
        assert_eq!(back.flag(ParamFlags::READ_ONLY), Some(true));
        assert_eq!(back.flag(ParamFlags::AUTOMATABLE), None);
    }

    #[test]
    fn test_to_range_toggle() {
        let info = ParameterInfo::new(1, "Bypass").with_steps(ParamSteps::Toggle);
        assert_eq!(info.to_range().scale, ParameterScale::Toggle);
    }

    #[test]
    fn test_to_range_integer() {
        let info = ParameterInfo::new(2, "Algorithm").with_steps(ParamSteps::Enumerated(5));
        assert_eq!(info.to_range().scale, ParameterScale::Integer);
    }

    #[test]
    fn test_to_range_logarithmic_db() {
        let mut info = ParameterInfo::new(3, "Gain").with_plain_range(0.001, 10.0, 1.0);
        info.unit = "dB".to_string();
        assert_eq!(info.to_range().scale, ParameterScale::Logarithmic);
    }

    #[test]
    fn test_to_range_logarithmic_hz() {
        let mut info = ParameterInfo::new(4, "Cutoff").with_plain_range(20.0, 20_000.0, 440.0);
        info.unit = "Hz".to_string();
        assert_eq!(info.to_range().scale, ParameterScale::Logarithmic);
    }

    #[test]
    fn test_to_range_log_fallback_non_positive_min() {
        let mut info = ParameterInfo::new(5, "Freq").with_plain_range(0.0, 20_000.0, 440.0);
        info.unit = "Hz".to_string();
        assert_eq!(info.to_range().scale, ParameterScale::Linear);
    }

    #[test]
    fn test_to_range_linear_default() {
        let info = ParameterInfo::new(6, "Mix");
        assert_eq!(info.to_range().scale, ParameterScale::Linear);
    }

    /// Apple's AUDelay Lowpass Cutoff: `[10, 22050]` Hz, native units. Writing
    /// a normalized `1.0` straight through set 1 Hz; `to_plain` is the call that
    /// makes it 22050.
    #[test]
    fn to_plain_maps_normalized_onto_the_declared_range() {
        let info =
            ParameterInfo::new(1, "Lowpass Cutoff").with_plain_range(10.0, 22_050.0, 22_050.0);

        assert_eq!(info.to_plain(0.0), 10.0);
        assert_eq!(info.to_plain(1.0), 22_050.0);
        assert_eq!(info.to_plain(0.5), 11_030.0);
    }

    #[test]
    fn to_normalized_inverts_to_plain() {
        let info = ParameterInfo::new(1, "Gain").with_plain_range(-96.0, 6.0, 0.0);

        for n in [0.0, 0.25, 0.5, 0.75, 1.0] {
            assert!((info.to_normalized(info.to_plain(n)) - n).abs() < 1e-12);
        }
    }

    /// Out-of-contract inputs clamp rather than escaping the plugin's declared
    /// range — a plugin never receives a value it didn't advertise.
    #[test]
    fn conversions_clamp_out_of_range_inputs() {
        let info = ParameterInfo::new(1, "Mix").with_plain_range(0.0, 100.0, 50.0);

        assert_eq!(info.to_plain(-5.0), 0.0);
        assert_eq!(info.to_plain(9.0), 100.0);
        assert_eq!(info.to_normalized(-50.0), 0.0);
        assert_eq!(info.to_normalized(500.0), 1.0);
    }

    /// A degenerate range (min == max) must not divide by zero.
    #[test]
    fn degenerate_range_does_not_produce_nan() {
        let info = ParameterInfo::new(1, "Fixed").with_plain_range(3.0, 3.0, 3.0);

        assert_eq!(info.to_plain(0.5), 3.0);
        assert_eq!(info.to_normalized(3.0), 0.0);
    }

    /// A NaN must never leave these conversions.
    ///
    /// The output of `to_plain` reaches `AudioUnitSetParameter` on a live unit,
    /// so a NaN here becomes a NaN filter coefficient — which does not stay in
    /// one parameter. Both the value and the bounds are untrusted:
    /// `ParameterInfo` is `Deserialize`d off the IPC wire into plain `f64`
    /// fields with no validating constructor, so a buggy or hostile peer
    /// supplies all three.
    ///
    /// Specifically *not* covered by clamping: `f64::clamp` returns NaN for a
    /// NaN input, and the `max <= min` degenerate guard is `false` when either
    /// bound is NaN.
    #[test]
    fn nan_never_escapes_a_conversion() {
        let info = ParameterInfo::new(1, "Cutoff").with_plain_range(10.0, 22_050.0, 10.0);

        assert!(
            info.to_plain(f64::NAN).is_finite(),
            "a NaN normalized value must not reach the plugin"
        );
        assert!(info.to_normalized(f64::NAN).is_finite());

        // Infinities are the same hazard: `clamp` passes them through unchanged.
        assert_eq!(info.to_plain(f64::INFINITY), 22_050.0);
        assert_eq!(info.to_plain(f64::NEG_INFINITY), 10.0);
        assert_eq!(info.to_normalized(f64::INFINITY), 1.0);

        // A NaN *bound*, which the degenerate-range check alone cannot catch.
        for (min, max) in [
            (f64::NAN, 1.0),
            (0.0, f64::NAN),
            (f64::NAN, f64::NAN),
            (f64::NEG_INFINITY, 1.0),
            (0.0, f64::INFINITY),
        ] {
            let broken = ParameterInfo::new(2, "Broken").with_plain_range(min, max, 0.0);
            for v in [0.0, 0.5, 1.0, f64::NAN] {
                assert!(
                    broken.to_plain(v).is_finite(),
                    "to_plain({v}) with bounds [{min}, {max}] returned non-finite"
                );
                assert!(
                    broken.to_normalized(v).is_finite(),
                    "to_normalized({v}) with bounds [{min}, {max}] returned non-finite"
                );
            }
        }
    }

    /// The NaN guard must not have cost the ordinary contract: every finite
    /// input still lands inside the declared range, at the declared endpoints.
    #[test]
    fn the_nan_guard_did_not_change_finite_behaviour() {
        let info = ParameterInfo::new(1, "Gain").with_plain_range(-96.0, 6.0, 0.0);

        assert_eq!(info.to_plain(0.0), -96.0);
        assert_eq!(info.to_plain(1.0), 6.0);
        assert_eq!(info.to_plain(0.5), -45.0);
        for n in [0.0, 0.25, 0.5, 0.75, 1.0] {
            let plain = info.to_plain(n);
            assert!((-96.0..=6.0).contains(&plain));
            assert!((info.to_normalized(plain) - n).abs() < 1e-12);
        }
    }

    #[test]
    fn test_to_range_values_preserved() {
        let info = ParameterInfo::new(7, "Volume").with_plain_range(-96.0, 6.0, -12.0);
        let range = info.to_range();
        assert_eq!(range.min, -96.0);
        assert_eq!(range.max, 6.0);
        assert_eq!(range.default, -12.0);
    }
}
