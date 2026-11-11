//! `AuLoaded::load` resolves a bundle path to the unit the bundle declares, by
//! exact component description, and refuses what is not a bundle.
//!
//! The bundle is `CoreAudio.component`, which declares every Apple unit (and a
//! non-unit `afil` first), so these need nothing installed beyond macOS.

#![cfg(target_os = "macos")]

use std::path::Path;

use tutti_au_host::AuLoaded;
use tutti_fixtures::main_thread;
use tutti_plugin_types::{Control, Load, LoadStage, PluginError};

const CORE_AUDIO: &str = "/System/Library/Components/CoreAudio.component";

fn load_id(id: &str) -> Result<AuLoaded, PluginError> {
    AuLoaded::load(
        &main_thread(),
        Path::new(CORE_AUDIO),
        &Load::new().plugin(id),
    )
}

fn scanning_reason(result: Result<AuLoaded, PluginError>) -> String {
    match result {
        Err(PluginError::Load {
            stage: LoadStage::Scanning,
            reason,
        }) => reason,
        Err(other) => panic!("expected a Scanning refusal, got {other:?}"),
        Ok(_) => panic!("expected a Scanning refusal, but it loaded"),
    }
}

/// Two ids of one bundle load two different units, each the unit with that
/// type, subtype and manufacturer (the loaded unit's own id is asserted, and
/// `dely` and `nbeq` are not the first entry, so neither is a default).
/// Failing the substring match: `Delay` also names AUSampleDelay.
#[test]
fn each_declared_id_loads_the_unit_with_that_triple() {
    for id in [
        "au.aufx.dely.appl",
        "au.aufx.nbeq.appl",
        "au.aumu.dls .appl",
    ] {
        let loaded = load_id(id).unwrap_or_else(|e| panic!("{id}: {e:?}"));
        assert_eq!(loaded.info().id, id, "the loaded unit is not the one named");
    }
}

/// No choice loads the bundle's first Audio Unit, and the `afil` entry that
/// precedes it in `CoreAudio.component` is never chosen.
#[test]
fn no_choice_loads_the_first_audio_unit_and_never_an_afil_entry() {
    let loaded = AuLoaded::load(&main_thread(), Path::new(CORE_AUDIO), &Load::new())
        .expect("the first unit loads");
    // CoreAudio.component's array opens with `afil MooV appl`, then
    // `aufx nbeq appl` (measured, macOS 15.6).
    assert_eq!(loaded.info().id, "au.aufx.nbeq.appl");
    assert!(!loaded.info().id.starts_with("au.afil"));
}

/// An `afil` entry is declared in the bundle but is not an Audio Unit, so
/// naming it is refused as undeclared.
#[test]
fn an_afil_entry_cannot_be_chosen() {
    let reason = scanning_reason(load_id("au.afil.MooV.appl"));
    let (_, declared) = reason.split_once("it declares: ").expect(&reason);
    assert!(!declared.contains("au.afil"), "{declared}");
    assert!(declared.contains("au.aufx.nbeq.appl"), "{declared}");
}

/// An id the bundle does not declare is refused at `Scanning`, naming the ids
/// it does declare.
#[test]
fn an_unknown_id_is_refused_naming_the_declared_ids() {
    let reason = scanning_reason(load_id("au.aufx.zzzz.appl"));
    assert!(reason.contains("au.aufx.zzzz.appl"), "{reason}");
    assert!(reason.contains("au.aufx.dely.appl"), "{reason}");
    assert!(reason.contains("au.aumu.samp.appl"), "{reason}");
}

/// The type is part of the id: `tmpt` is declared as both an effect and a
/// converter, and each id loads its own.
#[test]
fn an_effect_and_a_converter_of_one_subtype_are_two_ids() {
    for id in ["au.aufx.tmpt.appl", "au.aufc.tmpt.appl"] {
        assert_eq!(load_id(id).expect(id).info().id, id);
    }
}

/// A bare name is not a bundle, and neither is a plain file or a missing path.
#[test]
fn a_path_that_is_not_a_bundle_is_refused() {
    let file = tutti_fixtures::repo_root().join("Cargo.toml");
    for path in [
        Path::new("AUDelay"),
        Path::new("/nonexistent/x.component"),
        &file,
    ] {
        let reason = scanning_reason(AuLoaded::load(&main_thread(), path, &Load::new()));
        assert!(reason.contains("not an Audio Unit bundle"), "{reason}");
    }
}
