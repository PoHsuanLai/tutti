//! The mapping contract on the controller of a real plugin: the shared test
//! plugin's `level` (identity) and `tone` (a decreasing exponential, plain
//! `20000 * 1000^(-n)`), through `Vst3Loaded::mapping`.

use tutti_fixtures::main_thread;
use tutti_plugin_types::{Normalized, ParamId, ParamMapping, contract};
use tutti_shared_plugin::core::{LEVEL, TONE, tone_plain};
use tutti_vst3_host::{LoadOptions, Vst3Loaded};

fn load() -> Vst3Loaded {
    let path = tutti_fixtures::cdylib("tutti_shared_plugin", "tutti-shared-plugin");
    Vst3Loaded::load(
        main_thread(),
        &tutti_fixtures::detached_work(),
        &path,
        LoadOptions::new(),
    )
    .expect("the plugin loads")
}

/// **The controller's own maps hold the mapping laws**, whatever their shape:
/// finite plain values, positions clamped onto `0..=1`, a stable round trip from
/// the plain side and a monotonic map, for the identity `level` and for the
/// decreasing exponential `tone`. One test, because the main-thread token is
/// claimed once per process.
#[test]
fn the_controllers_map_holds_the_mapping_laws() {
    let main = main_thread();
    let loaded = load();
    for id in [LEVEL, TONE] {
        let map = loaded
            .mapping(main, ParamId::new(id))
            .expect("the plugin lists the parameter");
        contract::check(&map, &[]).unwrap_or_else(|e| panic!("parameter {id}: {e}"));
    }
}

/// The controller's map is the plugin's, not a line between its ends: `tone`'s
/// middle is the geometric mean, and the map runs backwards.
#[test]
fn the_controllers_map_is_the_plugins_own_not_a_line() {
    let main = main_thread();
    let loaded = load();
    let map = loaded
        .mapping(main, ParamId::new(TONE))
        .expect("the plugin lists tone");
    let half = map.plain(Normalized::of(0.5));
    assert!((half - tone_plain(0.5)).abs() < 1e-6 * half, "{half}");
    assert!(half < 0.5 * (map.plain(Normalized::of(0.0)) + map.plain(Normalized::of(1.0))));
    assert!(map.plain(Normalized::of(1.0)) < map.plain(Normalized::of(0.0)));
}

/// An id the plugin does not list has no mapping.
#[test]
fn an_unlisted_id_has_no_mapping() {
    let main = main_thread();
    let loaded = load();
    assert!(loaded.mapping(main, ParamId::new(9_999)).is_none());
}
