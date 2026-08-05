//! Do presets listed through the *handle* load the program they name?
//!
//! `PluginHandle` is the only surface a host keeps: `into_parts` consumes the
//! `Plugin` at load, so a preset browser holds a handle and nothing else. These
//! tests drive that route end to end — `handle.presets()` → `handle.load_preset`
//! → the program the plugin actually switched to — rather than the
//! `Vst2Instance` methods underneath, which `tutti-vst2-host`'s own suite
//! already covers.
//!
//! # What makes the round trip meaningful
//!
//! A `PresetId` is opaque: the caller never builds one, it hands back what the
//! list gave it. For VST2 that id happens to be a dense index, but the point of
//! the round trip is that nothing here *relies* on that — the same code path
//! carries AU's sparse selectors and VST3's `(list, index)` pairs. A test that
//! passed `PresetId::Number(1)` literally would pin the format's numbering
//! rather than the surface's contract, so every load below uses an id the
//! listing produced.

#![cfg(feature = "vst2")]

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use tutti_plugin::handles::PluginHandle;
use tutti_plugin::{in_process_vst2_client, FeatureReport, Features, PresetId};

#[path = "support/probe_path.rs"]
mod probe_path;

const SAMPLE_RATE: f64 = 48_000.0;

/// Serializes against the probe's process-global switches and environment.
static PROBE_LOCK: Mutex<()> = Mutex::new(());

fn lock_probe() -> MutexGuard<'static, ()> {
    PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// Every `TUTTI_VST2_PROBE_*` key this file sets, cleared between loads so a
/// leaked variable cannot reshape an unrelated test's plugin.
const PROBE_ENV_KEYS: &[&str] = &[
    "TUTTI_VST2_PROBE_PROGRAMS",
    "TUTTI_VST2_PROBE_SERVICED_PROGRAMS",
];

fn clear_probe_env() {
    for key in PROBE_ENV_KEYS {
        // SAFETY: callers hold `PROBE_LOCK`, so no other test thread is
        // touching the environment concurrently.
        unsafe { std::env::remove_var(key) };
    }
}

/// Load the probe through the in-process VST2 path and keep only the handle,
/// which is what a real host is left holding.
fn load_handle(env: &[(&str, &str)]) -> PluginHandle {
    let path: PathBuf = probe_path::probe_path().clone();
    clear_probe_env();
    for (k, v) in env {
        // SAFETY: as above.
        unsafe { std::env::set_var(k, v) };
    }
    let (_client, handle) = in_process_vst2_client(&path, SAMPLE_RATE)
        .unwrap_or_else(|e| panic!("in-process VST2 load failed for {path:?}: {e:?}"));
    // The AEffect is built; clearing now keeps the variable from outliving it.
    clear_probe_env();
    handle
}

/// A preset listed through the handle loads the program it names.
///
/// The whole surface in one path: list, pick, load, confirm. `current_preset`
/// is the confirmation rather than the return value alone, because
/// `load_preset` returning `true` only says the switch was dispatched — VST2's
/// `effProgramChange` reports nothing back.
#[test]
fn a_listed_preset_loads_the_program_it_names() {
    let _guard = lock_probe();
    let handle = load_handle(&[("TUTTI_VST2_PROBE_PROGRAMS", "3")]);

    let presets = handle.presets().expect("the VST2 path carries presets");
    assert_eq!(presets.len(), 3, "the probe declared 3 programs");

    // Deliberately not `PresetId::Number(2)`: the id is opaque, and taking it
    // from the listing is what a caller does.
    let wanted = presets[2].id.clone();
    assert!(
        handle.load_preset(&wanted),
        "loading a preset the plugin listed must be accepted"
    );
    assert_eq!(
        handle.current_preset(),
        Some(wanted),
        "the plugin must report the program the handle just loaded"
    );
}

/// An id from another format is refused rather than coerced.
///
/// `Program` and `Location` name nothing in VST2's index space. Refusing is the
/// point: coercing either into an index — by taking the `index` field, say —
/// would load a real program that the caller never asked for, which is silent
/// and worse than a `false`.
#[test]
fn an_id_from_another_format_is_refused() {
    let _guard = lock_probe();
    let handle = load_handle(&[("TUTTI_VST2_PROBE_PROGRAMS", "3")]);

    let before = handle.current_preset();

    assert!(
        !handle.load_preset(&PresetId::Program {
            list_id: 0,
            index: 1
        }),
        "a VST3 program id addresses no VST2 program"
    );
    assert!(
        !handle.load_preset(&PresetId::Location(PathBuf::from("/x.clap-preset"))),
        "a CLAP preset path addresses no VST2 program"
    );

    assert_eq!(
        handle.current_preset(),
        before,
        "a refused load must not move the program"
    );
}

/// A plugin declaring no programs answers the preset bits `false`, not silence.
///
/// The distinction `FeatureReport` exists for. VST2 answers both bits from
/// `numPrograms`, so zero programs is a *declination* — the plugin was asked
/// and said no. Reporting `None` here would claim nobody asked, and a browser
/// could not tell "this plugin has no presets" from "this host cannot check".
#[test]
fn a_plugin_with_no_programs_declines_rather_than_going_silent() {
    let _guard = lock_probe();
    let handle = load_handle(&[("TUTTI_VST2_PROBE_PROGRAMS", "0")]);

    let loaded = handle.loaded();
    let report = FeatureReport::new(loaded.probed, loaded.features);
    assert_eq!(
        report.get(Features::PRESET_LIST),
        Some(false),
        "a plugin with no programs declines the list; it was asked"
    );
    assert_eq!(
        report.get(Features::PRESET_LOAD),
        Some(false),
        "and declines the load, from the same number"
    );

    assert_eq!(
        handle.presets(),
        Some(Vec::new()),
        "the route exists and listed nothing — not None, which would mean no route"
    );
}

/// A plugin with programs sets both bits.
///
/// The positive half: without it, a loader that cleared the bits
/// unconditionally would satisfy the test above.
#[test]
fn a_plugin_with_programs_answers_both_preset_bits() {
    let _guard = lock_probe();
    let handle = load_handle(&[("TUTTI_VST2_PROBE_PROGRAMS", "2")]);

    let loaded = handle.loaded();
    let report = FeatureReport::new(loaded.probed, loaded.features);
    assert_eq!(report.get(Features::PRESET_LIST), Some(true));
    assert_eq!(report.get(Features::PRESET_LOAD), Some(true));
}
