//! Carry a [`UnitParam`] through fundsp's lock-free [`Setting`] channel.
//!
//! The [`UnitParam`] vocabulary itself is pure and lives in `tutti-types`. This
//! module is the fundsp-coupled half: building a `Setting` that addresses a
//! param by its stable id, and reading one back inside a leaf unit's
//! `AudioUnit::set`. It lives here because it joins two crates neither of which
//! may name the other: [`UnitParam`] is `tutti-types`', while [`Setting`] /
//! [`Parameter`] / [`Address`] are `tutti-node`'s (this crate re-exports them),
//! and [`node_setting`] additionally needs [`NodeId`](crate::net::NodeId),
//! which is this crate's alone. `tutti-core` re-exports these functions so
//! consumers reach `UnitParam` and its `Setting` glue together
//! (`tutti_core::unit_param::{setting, from_setting}`).
//!
//! The addressing rides fundsp's `Net::set`, which is **lock-free** when a
//! realtime backend is attached (the setting is enqueued to the audio thread),
//! so it is the RT-correct param path — unlike a `downcast_mut` + direct field
//! write.

use crate::setting::{Address, Parameter, Setting};
use tutti_types::UnitParam;

/// Build a [`Setting`] carrying `(param, value)` addressed at a leaf unit,
/// for delivery through `AudioUnit::set` directly.
///
/// - `Parameter::Value(v)` carries the value,
/// - the `Address::Index(id)` level (unused by leaf units, which have no inner
///   nodes to descend into) carries the param selector.
///
/// To address a node *inside a [`Net`](crate::net::Net)*, use
/// [`node_setting`] — the address order is load-bearing and easy to get
/// backwards by hand.
pub fn setting(param: UnitParam, value: f32) -> Setting {
    Setting::value(value).index(u16::from(param) as usize)
}

/// Build a [`Setting`] that addresses `param` on `node` within a
/// [`Net`](crate::net::Net).
///
/// The address is `[Node, Index]`, and the order is not cosmetic: `Net::set`
/// matches on `direction()` — the *first* address level — to find the node,
/// then `peel()`s it so the leaf's own `set` sees `Index` at the front. Build
/// the two levels the other way round and `direction()` yields `Index`, the
/// node lookup never matches, and the setting is dropped in silence: the fader
/// moves on screen and not in the sound.
///
/// That is why this exists as a function rather than a documented call-site
/// idiom. `setting(param, v).node(id)` reads correctly and appends in exactly
/// the wrong order.
pub fn node_setting(node: crate::net::NodeId, param: UnitParam, value: f32) -> Setting {
    Setting::value(value)
        .node(node)
        .index(u16::from(param) as usize)
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
    use crate::setting::NodeAddr;

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

    /// A node-addressed setting must present `Node` first, so `Net::set` finds
    /// the node, and `Index` after peeling, so the leaf decodes the param.
    ///
    /// The test above round-trips a *leaf* setting, which is why it never
    /// caught the ordering: peel one level off the wrong build order and the
    /// address is empty, so a leaf sees nothing at all. The failure mode is
    /// silence — `Net::set` drops an unmatched address without complaint —
    /// which is exactly what a test has to stand in for.
    #[test]
    fn a_node_setting_addresses_the_node_then_the_param() {
        let node = crate::net::NodeId::new();
        let s = node_setting(node, UnitParam::Drive, 4.0);

        assert!(
            matches!(s.direction(), Address::Node(addr) if addr == NodeAddr::from(node)),
            "Net::set matches on the first level; it must be the node"
        );

        let (param, value) = from_setting(&s.peel()).expect("leaf decodes after peel");
        assert_eq!(param, UnitParam::Drive);
        assert!((value - 4.0).abs() < 1e-9);
    }
}
