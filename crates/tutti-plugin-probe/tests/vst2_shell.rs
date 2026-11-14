//! **A VST2 shell file is every plugin it names.** The reference shell
//! (`tutti-vst2-shell-plugin`) answers `audioMasterCurrentId` with what the host says: with
//! `0` it is the shell (`kPlugCategShell`) and walks its two sub-plugins at
//! `effShellGetNextPlugin`; with a sub-plugin's `uniqueID` it is that plugin.
//!
//! The file is the plugin's copy under a `.vst` name, so the probe routes it as VST2 whatever
//! the platform's library extension. The oracle is the spec's protocol and the ids the
//! shell's own crate declares, not what the host found.

#![cfg(feature = "vst2")]

use std::path::PathBuf;
use tutti_plugin_probe::describe;
use tutti_plugin_types::{InProcess, Load, PluginRole};
use tutti_vst2_host::Vst2Loaded;
use tutti_vst2_shell_plugin::{ALPHA_ID, ALPHA_NAME, BETA_ID, BETA_NAME, SHELL_ID, SHELL_NAME};

fn shell(dir: &tempfile::TempDir) -> PathBuf {
    let real = tutti_fixtures::cdylib("tutti_vst2_shell_plugin", "tutti-vst2-shell-plugin");
    let copy = dir.path().join("shell.vst");
    std::fs::copy(real, &copy).expect("copy the plugin");
    copy
}

fn id(unique: i32) -> String {
    format!("vst2.{unique}")
}

/// The probe lists each sub-plugin of the shell, in the shell's order, described as that
/// plugin and not as the shell: its own id, name and category.
#[test]
fn the_probe_lists_every_sub_plugin_of_a_shell() {
    let dir = tempfile::tempdir().unwrap();
    let main = tutti_fixtures::main_thread();
    let plugins = describe(&main, &shell(&dir)).expect("probes");

    let ids: Vec<_> = plugins.iter().map(|p| p.id.clone()).collect();
    assert_eq!(ids, [id(ALPHA_ID), id(BETA_ID)], "{plugins:?}");
    let names: Vec<_> = plugins.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, [ALPHA_NAME, BETA_NAME]);
    assert_eq!(plugins[0].class.role(), PluginRole::Effect);
    assert_eq!(plugins[1].class.role(), PluginRole::Instrument);
    assert!(
        plugins.iter().all(|p| p.vendor == "Tutti"),
        "each is described from its own load: {plugins:?}"
    );
}

/// A load that names no plugin is the shell itself (the host answered 0), and a load that
/// names a sub-plugin is that plugin, with the choice kept for a reopen.
#[test]
fn a_load_by_id_is_that_sub_plugin() {
    let dir = tempfile::tempdir().unwrap();
    let path = shell(&dir);
    let main = tutti_fixtures::main_thread();

    let first =
        Vst2Loaded::load_in_process(&main, &tutti_fixtures::detached_work(), &path, &Load::new())
            .expect("the shell");
    assert_eq!(first.metadata().id, id(SHELL_ID));
    assert_eq!(first.metadata().name, SHELL_NAME);
    assert_eq!(first.origin().load().chosen(), None);
    drop(first);

    for (unique, name) in [(BETA_ID, BETA_NAME), (ALPHA_ID, ALPHA_NAME)] {
        let load = Load::new().plugin(id(unique));
        let loaded =
            Vst2Loaded::load_in_process(&main, &tutti_fixtures::detached_work(), &path, &load)
                .expect("a sub-plugin");
        assert_eq!(loaded.metadata().id, id(unique));
        assert_eq!(loaded.metadata().name, name);
        assert_eq!(
            loaded.origin().load(),
            &load,
            "a reopen loads the same sub-plugin"
        );
    }
}

/// A shell that creates something else for an id (it does not know it) is refused: the
/// plugin that came up is not the one asked for. So is a choice that is no VST2 id.
#[test]
fn an_id_the_shell_does_not_have_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = shell(&dir);
    let main = tutti_fixtures::main_thread();

    let unknown = Load::new().plugin(id(0x1234_5678));
    let refused =
        Vst2Loaded::load_in_process(&main, &tutti_fixtures::detached_work(), &path, &unknown);
    assert!(
        refused.is_err_and(|e| e.to_string().contains("no plugin")),
        "the shell's default is not the plugin asked for"
    );
    let malformed = Load::new().plugin("clap.some.plugin");
    assert!(
        Vst2Loaded::load_in_process(&main, &tutti_fixtures::detached_work(), &path, &malformed)
            .is_err()
    );
}
