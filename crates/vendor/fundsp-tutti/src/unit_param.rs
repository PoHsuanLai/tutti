//! Carry a [`UnitParam`] through fundsp's lock-free [`Setting`] channel.
//!
//! The [`UnitParam`] vocabulary itself is pure and lives in `tutti-types`. This
//! module is the fundsp-coupled half: building a `Setting` that addresses a
//! param by its stable id, and reading one back inside a leaf unit's
//! `AudioUnit::set`. It lives here because `fundsp-tutti` owns [`Setting`] /
//! [`Parameter`] / [`Address`]; `tutti-core` re-exports these functions so
//! consumers reach `UnitParam` and its `Setting` glue together
//! (`tutti_core::unit_param::{setting, from_setting}`).
//!
//! The addressing rides fundsp's `Net::set`, which is **lock-free** when a
//! realtime backend is attached (the setting is enqueued to the audio thread),
//! so it is the RT-correct param path — unlike a `downcast_mut` + direct field
//! write.

use crate::setting::{Address, Parameter, Setting};
use tutti_types::UnitParam;

/// Build a [`Setting`] carrying `(param, value)` for delivery through
/// `Net::set` / `AudioUnit::set`. Address the target node with `.node(id)` at
/// the call site: `unit_param::setting(param, v).node(node_id)`.
///
/// - `Parameter::Value(v)` carries the value,
/// - the `Address::Index(id)` level (unused by leaf units, which have no inner
///   nodes to descend into) carries the param selector.
pub fn setting(param: UnitParam, value: f32) -> Setting {
    Setting::value(value).index(u16::from(param) as usize)
}

/// Read `(UnitParam, value)` from a [`Setting`] as seen by a leaf unit's `set()`
/// (i.e. after `Net` has peeled the node address). Returns `None` unless the
/// setting is a `Value` carrying an `Index` selector that maps to a known param.
///
/// Unknown ids yield `None`, so the scheme is forward-compatible: a newer host
/// can address a param an older unit lacks and it simply no-ops.
pub fn from_setting(setting: &Setting) -> Option<(UnitParam, f32)> {
    let value = match setting.parameter() {
        Parameter::Value(v) => *v,
        _ => return None,
    };
    match setting.direction() {
        Address::Index(i) => UnitParam::try_from(u16::try_from(i).ok()?)
            .ok()
            .map(|p| (p, value)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_setting() {
        for id in 0..=16u16 {
            let p = UnitParam::try_from(id).expect("known id");
            let s = setting(p, 1.23);
            let (got, v) = from_setting(&s).expect("decodes");
            assert_eq!(got, p);
            assert!((v - 1.23).abs() < 1e-9);
        }
    }

    #[test]
    fn non_value_setting_is_none() {
        // A bare center setting (no Value/Index) is not a UnitParam.
        assert_eq!(from_setting(&Setting::center(440.0)), None);
    }
}
