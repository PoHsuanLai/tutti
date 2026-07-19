//! Uniform parameter addressing for tutti units.
//!
//! Every tutti `*Node` historically exposed bespoke typed setters
//! (`set_frequency`, `set_q`, `set_mix`, …) that callers reached by
//! downcasting to the concrete type (`graph.downcast_mut::<StereoSvfFilterNode>()`).
//! That forced the ECS reconcile layer to hardcode one system per effect type.
//!
//! [`UnitParam`] is the small, stable vocabulary of *every* scalar a built-in
//! unit can expose. A caller sets one through fundsp's existing [`Setting`]
//! channel — [`UnitParam::setting`] builds `Setting::value(v).index(id)`:
//!
//! - `Parameter::Value(v)` carries the value,
//! - the `Address::Index(id)` level (unused by leaf units, which have no inner
//!   nodes to descend into) carries the param selector.
//!
//! A unit's `AudioUnit::set` reads it back with [`UnitParam::from_setting`] and
//! stores the matching `Param` atomic. Unknown ids are ignored, so the scheme
//! is forward-compatible: a newer host can address a param an older unit lacks
//! and it simply no-ops.
//!
//! This rides fundsp's `Net::set`, which is **lock-free** when a realtime
//! backend is attached (the setting is enqueued to the audio thread), so it is
//! the RT-correct param path — unlike a `downcast_mut` + direct field write.

use crate::dsp::{Address, Parameter, Setting};

/// The full set of scalar parameters any built-in tutti unit may expose.
///
/// Discriminants are **stable** — they ride through [`Setting`] as an address
/// index and (potentially) persist in tooling, so existing values must never be
/// renumbered. Append new params at the end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum UnitParam {
    /// Filter cutoff / centre frequency (Hz).
    Cutoff = 0,
    /// Filter resonance / Q.
    Q = 1,
    /// Filter / EQ gain (dB).
    GainDb = 2,
    /// Wet/dry mix (0..1).
    Wet = 3,
    /// Feedback amount (0..1).
    Feedback = 4,
    /// Delay time (beats or seconds — unit-defined).
    DelayTime = 5,
    /// Modulation rate (Hz).
    Rate = 6,
    /// Modulation depth (0..1).
    Depth = 7,
    /// Reverb room size (0..1).
    RoomSize = 8,
    /// Reverb damping (0..1).
    Damping = 9,
    /// Dynamics threshold (dB).
    Threshold = 10,
    /// Compressor ratio (≥1).
    Ratio = 11,
    /// Envelope attack (seconds).
    Attack = 12,
    /// Envelope release (seconds).
    Release = 13,
    /// Limiter ceiling (dB).
    Ceiling = 14,
    /// Drive / saturation amount.
    Drive = 15,
    /// Compressor make-up gain (dB).
    Makeup = 16,
}

impl UnitParam {
    /// Reconstruct from the stable `u16` discriminant. `None` for unknown ids.
    pub fn from_u16(id: u16) -> Option<Self> {
        use UnitParam::*;
        Some(match id {
            0 => Cutoff,
            1 => Q,
            2 => GainDb,
            3 => Wet,
            4 => Feedback,
            5 => DelayTime,
            6 => Rate,
            7 => Depth,
            8 => RoomSize,
            9 => Damping,
            10 => Threshold,
            11 => Ratio,
            12 => Attack,
            13 => Release,
            14 => Ceiling,
            15 => Drive,
            16 => Makeup,
            _ => return None,
        })
    }

    /// Build a [`Setting`] carrying `(self, value)` for delivery through
    /// `Net::set` / `AudioUnit::set`. Address the target node with `.node(id)`
    /// at the call site: `param.setting(v).node(node_id)`.
    pub fn setting(self, value: f32) -> Setting {
        Setting::value(value).index(self as usize)
    }

    /// Read `(UnitParam, value)` from a [`Setting`] as seen by a leaf unit's
    /// `set()` (i.e. after `Net` has peeled the node address). Returns `None`
    /// unless the setting is a `Value` carrying an `Index` selector that maps to
    /// a known param.
    pub fn from_setting(setting: &Setting) -> Option<(Self, f32)> {
        let value = match setting.parameter() {
            Parameter::Value(v) => *v,
            _ => return None,
        };
        match setting.direction() {
            Address::Index(i) => Self::from_u16(u16::try_from(i).ok()?).map(|p| (p, value)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_setting() {
        for id in 0..=16u16 {
            let p = UnitParam::from_u16(id).expect("known id");
            let s = p.setting(1.23);
            let (got, v) = UnitParam::from_setting(&s).expect("decodes");
            assert_eq!(got, p);
            assert!((v - 1.23).abs() < 1e-9);
        }
    }

    #[test]
    fn unknown_id_is_none() {
        assert_eq!(UnitParam::from_u16(9999), None);
    }

    #[test]
    fn non_value_setting_is_none() {
        // A bare center setting (no Value/Index) is not a UnitParam.
        assert_eq!(UnitParam::from_setting(&Setting::center(440.0)), None);
    }
}
