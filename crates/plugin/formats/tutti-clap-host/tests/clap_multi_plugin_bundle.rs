//! A `.clap` bundle is a factory, not a plugin — can this host reach past the
//! first entry in it?
//!
//! `clap_plugin_factory::get_plugin_count` exists because one file may ship a
//! synth plus companion effects. This host called
//! `get_plugin_descriptor(factory, 0)` and discarded the count, so everything
//! after the first plugin in a bundle was unreachable. It failed silently:
//! index 0 loads fine, so a multi-plugin bundle looked exactly like a
//! single-plugin one.
//!
//! These go through the real `AEffect`-equivalent FFI — the in-repo probe,
//! which ships **two** descriptors for exactly this reason. A single-descriptor
//! fixture cannot distinguish a host that enumerates the factory from one that
//! hard-codes index 0, because both load the same plugin and both pass.

use std::path::Path;

#[path = "support/probe_path.rs"]
mod probe_path;
use probe_path::probe_path;

use tutti_clap_host::ClapLoaded;
use tutti_clap_test_plugin::{PLUGIN_ID, SECOND_PLUGIN_ID};

const RATE: f64 = 48_000.0;
const BLOCK: u32 = 512;

fn bundle() -> &'static Path {
    // Bare dylib: passed as both bundle and library so the host dlopens it
    // directly, no `.clap` bundle structure needed.
    Path::new(probe_path())
}

/// `probe_all` reports every plugin the factory advertises.
///
/// The count is the assertion that matters: `probe` alone answers for the first
/// descriptor, which is indistinguishable from "the bundle has one plugin".
#[test]
fn probe_all_reports_every_plugin_in_the_bundle() {
    let all = ClapLoaded::probe_all(bundle(), Some(bundle())).expect("probe_all");

    let ids: Vec<&str> = all.iter().map(|i| i.id.as_str()).collect();
    assert_eq!(
        ids.len(),
        2,
        "the probe factory advertises two plugins, found: {ids:?}"
    );
    assert!(
        ids.contains(&PLUGIN_ID.to_str().unwrap()),
        "first plugin missing from {ids:?}"
    );
    assert!(
        ids.contains(&SECOND_PLUGIN_ID.to_str().unwrap()),
        "second plugin missing from {ids:?} — the factory was not enumerated \
         past index 0"
    );
}

/// The plain `probe` still answers for the bundle's first plugin.
///
/// Enumeration must not have changed what a single-plugin caller sees; every
/// existing scan path goes through here.
#[test]
fn probe_still_answers_for_the_first_plugin() {
    let info = ClapLoaded::probe(bundle(), Some(bundle())).expect("probe");
    assert_eq!(info.id, PLUGIN_ID.to_str().unwrap());
}

/// A plugin that is not first in the bundle can actually be loaded.
///
/// This is the finding end to end: not just enumerated, but instantiated. The
/// id reaches `create_plugin`, so a host asking for the companion effect gets
/// the companion effect.
#[test]
fn a_plugin_past_index_zero_can_be_loaded() {
    let loaded = ClapLoaded::load_plugin(bundle(), SECOND_PLUGIN_ID.to_str().unwrap(), RATE, BLOCK)
        .expect("the second plugin in the bundle must be loadable");

    assert_eq!(
        loaded.info().id,
        SECOND_PLUGIN_ID.to_str().unwrap(),
        "loaded a different plugin than the one named"
    );
}

/// Loading without naming an id gets the bundle's first plugin.
///
/// The compatibility half: every existing caller passes no id, and must keep
/// getting exactly what it got before ids were selectable.
#[test]
fn loading_without_an_id_gets_the_first_plugin() {
    let loaded = ClapLoaded::load(bundle(), RATE, BLOCK).expect("default load");
    assert_eq!(loaded.info().id, PLUGIN_ID.to_str().unwrap());
}

/// Naming an id the bundle does not contain fails instead of loading something
/// else.
///
/// Falling back to the first plugin would be worse than erroring: a session
/// restoring "the compressor" would come back with the synth, silently, and the
/// user would hear the wrong plugin rather than see a message.
#[test]
fn an_unknown_id_fails_rather_than_falling_back() {
    // Matched rather than `expect_err`, because `ClapLoaded` is not `Debug` —
    // and a loaded plugin is exactly what must not come back here.
    let msg = match ClapLoaded::load_plugin(bundle(), "tutti.no-such-plugin", RATE, BLOCK) {
        Ok(loaded) => panic!(
            "an unknown id must not silently load a plugin; got '{}'",
            loaded.info().id
        ),
        Err(e) => e.to_string(),
    };
    assert!(
        msg.contains("tutti.no-such-plugin"),
        "the error should name the id that was asked for: {msg}"
    );
}
