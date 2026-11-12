//! The plugin probe, a separate process: `tutti_plugin::catalog::probe_record`
//! spawns `plugin-probe`, which loads the file just far enough to read the
//! descriptor of every plugin it holds and writes them to a result file.
//!
//! One test per format reads the shared plugin's descriptor, and one test each
//! reads a file of several plugins (a CLAP factory of two, a VST3 factory of
//! three audio classes, a VST2 shell of two sub-plugins) and loads each plugin by
//! the `PluginRef` its entry gives. The CLAP plugin's
//! `init` also misbehaves by file name (`clap.rs`, `InitMode`): a probe sends
//! no state and a plugin may be configured by no environment variable, so the
//! name is the one lever. Those cases are CLAP's; the other formats have none
//! to pull.
//!
//! Needs `cargo build -p tutti-plugin-probe` first.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tutti_plugin::catalog::{
    DEFAULT_PROBE_TIMEOUT, Locator, PluginDescriptor, PluginRef, probe_failure, probe_record_with,
};
use tutti_plugin_contract::{bundle_of, clap_path, library, vst3_cpp_module, vst3_module};
use tutti_plugin_db::PluginDb;
use tutti_plugin_ipc::{BridgeConfig as IpcConfig, Ipc, Launched};
use tutti_plugin_types::{
    BridgeError, BridgeResult, Control, Editor, InProcess, Load, LoadStage, MainThread, SampleRate,
};
use tutti_vst2_host::Vst2Loaded;

/// The one descriptor `probe_record` found at `path`.
fn probe(path: &Path) -> Result<PluginDescriptor, BridgeError> {
    probe_with_timeout(path, DEFAULT_PROBE_TIMEOUT)
}

/// [`probe`] under a deadline of the test's choosing.
fn probe_with_timeout(path: &Path, timeout: Duration) -> Result<PluginDescriptor, BridgeError> {
    let found = probe_all_with_timeout(path, timeout)?;
    assert_eq!(found.len(), 1, "one plugin in {}", path.display());
    Ok(found.into_iter().next().unwrap())
}

/// Every plugin `probe_record` found at `path`, as the catalog holds them: the
/// db a probe's delta was applied to.
fn probe_db(path: &Path) -> Result<PluginDb, BridgeError> {
    probe_db_with_timeout(path, DEFAULT_PROBE_TIMEOUT)
}

/// The probe is the one built in this profile, named through [`Locator::at`], so
/// nothing writes the process-global environment.
fn probe_db_with_timeout(path: &Path, timeout: Duration) -> Result<PluginDb, BridgeError> {
    let locator = Locator::at(tutti_fixtures::plugin_probe());
    let delta = probe_record_with(path, &locator, timeout)?;
    let mut db = PluginDb::new();
    db.apply(delta);
    Ok(db)
}

fn probe_all_with_timeout(
    path: &Path,
    timeout: Duration,
) -> Result<Vec<PluginDescriptor>, BridgeError> {
    let db = probe_db_with_timeout(path, timeout)?;
    Ok(db.plugins().map(|p| p.descriptor.clone()).collect())
}

/// The shared plugin as the spec describes it, whatever the format.
fn assert_shared_plugin(d: &PluginDescriptor, format: &str) {
    assert_named(d, format, "Tutti Shared Test Plugin");
}

fn assert_named(d: &PluginDescriptor, format: &str, name: &str) {
    assert_eq!(d.name, name);
    assert_eq!(d.vendor, "Tutti");
    assert_eq!(d.class.format_name(), format, "{d:?}");
}

#[test]
fn clap_is_probed_through_the_binary() {
    let d = probe(&clap_path()).expect("probes");
    assert_shared_plugin(&d, "clap");
    assert_eq!(d.id, "tutti.shared-test-plugin");
}

/// The shared plugin under a `.vst` name in a directory of its own, so the probe
/// routes it as VST2 whatever the platform's library extension (`.dylib` on macOS
/// names no plugin format).
fn vst2_named(dir: &tempfile::TempDir, file: &str, real: PathBuf) -> PathBuf {
    let path = dir.path().join(file);
    std::fs::copy(real, &path).expect("copy the plugin");
    path
}

#[test]
fn vst2_is_probed_through_the_binary() {
    let dir = tempfile::tempdir().unwrap();
    let d = probe(&vst2_named(&dir, "shared.vst", library())).expect("probes");
    assert_shared_plugin(&d, "vst2");
}

/// **A VST3 factory is every audio class it lists**, one entry per class id, in the
/// factory's order (`class_at` in the shared plugin: the plugin, then a copy that
/// refuses `initialize`, then one that refuses processing). The C++ plugin's factory
/// has one audio class (and a controller class, which is no plugin).
#[test]
fn vst3_is_probed_through_the_binary() {
    let db = probe_db(&bundle_of(&vst3_module())).expect("probes");
    let found: Vec<_> = db.plugins().map(|p| p.descriptor.clone()).collect();
    let names: Vec<_> = found.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "Tutti Shared Test Plugin",
            "Tutti Shared Test Plugin (initialize refused)",
            "Tutti Shared Test Plugin (processing refused)",
        ]
    );
    for d in &found {
        assert_named(d, "vst3", &d.name);
        assert!(d.id.starts_with("vst3."), "{}", d.id);
    }
    let mut ids: Vec<_> = found.iter().map(|d| d.id.clone()).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 3, "one id per class: {found:?}");
}

#[test]
fn vst3_cpp_is_probed_through_the_binary() {
    let d = probe(&bundle_of(&vst3_cpp_module())).expect("probes");
    assert_named(&d, "vst3", "Tutti Shared Test Plugin (C++)");
}

/// No editor in these tests: the served plugin is driven through `Ipc` alone.
fn no_editor(_: &MainThread, path: &Path, _: &Load) -> BridgeResult<Box<dyn Editor>> {
    Err(BridgeError::LoadFailed {
        path: path.to_path_buf(),
        stage: LoadStage::Opening,
        reason: "these tests load no editor".into(),
    })
}

/// The id of the plugin a `plugin-server` loads for `plugin`: what a served load of
/// the ref picks.
fn served_id(plugin: &PluginRef) -> String {
    let mut env = tutti_fixtures::ScopedEnv::new();
    env.set("TUTTI_PLUGIN_SERVER", tutti_fixtures::plugin_server());
    let Launched { ipc, .. } = Ipc::launch(
        IpcConfig::default(),
        plugin.path().to_path_buf(),
        plugin.load(),
        SampleRate::new(48_000.0),
        None,
        no_editor,
    )
    .unwrap_or_else(|e| panic!("{plugin:?}: the server loads it: {e:?}"));
    Control::info(&ipc).id.clone()
}

/// **A CLAP factory of two plugins is two entries**, the factory's order, and a served
/// load of each entry's ref loads that plugin, not the file's first.
#[test]
fn a_clap_factory_of_two_plugins_is_probed_as_two() {
    // A copy under its own name: `clap_probe()` is a link, which a catalog row keys by
    // what it names (a `.so`, which no server routes as CLAP).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("two.clap");
    std::fs::copy(
        tutti_fixtures::cdylib("tutti_clap_test_plugin", "tutti-clap-test-plugin"),
        &path,
    )
    .expect("copy the plugin");
    let db = probe_db(&path).expect("probes");
    let ids: Vec<_> = db.plugins().map(|p| p.descriptor.id.clone()).collect();
    // The factory's two descriptors, by the ids the reference plugin declares (the
    // crate is a dev-dependency to be built, not linked: its entry point would clash
    // with the shared plugin's).
    assert_eq!(
        ids,
        ["tutti.conformance-probe", "tutti.conformance-probe.second"]
    );

    for entry in db.plugins() {
        let plugin = PluginRef::from(&entry);
        assert_eq!(served_id(&plugin), entry.descriptor.id, "{ids:?}");
    }
}

/// A served load of a VST3 class's ref loads that class: the third of the shared
/// plugin's three, which a load of the file's first would not be.
#[test]
fn a_served_vst3_load_picks_the_class_its_ref_names() {
    let db = probe_db(&bundle_of(&vst3_module())).expect("probes");
    let entry = db.plugins().nth(2).expect("a third class");
    assert_eq!(
        served_id(&PluginRef::from(&entry)),
        entry.descriptor.id,
        "{:?}",
        entry.descriptor
    );
}

/// **A VST2 shell is its sub-plugins**, each an entry of its own, never the shell:
/// the probe lists them in the shell's order, and a load of each entry's ref (in this
/// process, which is where VST2 runs) creates that sub-plugin. A served VST2 load is
/// refused by the format (`NotOutOfProcess`), so the served pick is CLAP's and VST3's
/// above.
#[test]
fn a_vst2_shell_is_probed_as_its_sub_plugins() {
    // The shell's sub-plugins: `uniqueID`s spelt as four-character codes, and names, as
    // `tutti-vst2-shell-plugin` declares them (a dev-dependency to be built, not linked:
    // its entry point would clash with the shared plugin's).
    let alpha = i32::from_be_bytes(*b"SHAL");
    let beta = i32::from_be_bytes(*b"SHBE");
    let dir = tempfile::tempdir().unwrap();
    let real = tutti_fixtures::cdylib("tutti_vst2_shell_plugin", "tutti-vst2-shell-plugin");
    let path = vst2_named(&dir, "shell.vst", real);
    let db = probe_db(&path).expect("probes");

    let found: Vec<_> = db
        .plugins()
        .map(|p| (p.descriptor.id.clone(), p.descriptor.name.clone()))
        .collect();
    assert_eq!(
        found,
        [
            (format!("vst2.{alpha}"), "Shell Alpha".to_string()),
            (format!("vst2.{beta}"), "Shell Beta".to_string()),
        ]
    );

    let main = tutti_fixtures::main_thread();
    for entry in db.plugins() {
        let plugin = PluginRef::from(&entry);
        let loaded = Vst2Loaded::load_in_process(&main, plugin.path(), &plugin.load())
            .expect("the sub-plugin loads");
        assert_eq!(loaded.metadata().id, entry.descriptor.id);
    }
}

/// The shared plugin under a file name its `init` reads, in a directory of its
/// own.
fn clap_named(dir: &tempfile::TempDir, name: &str) -> PathBuf {
    let path = dir.path().join(format!("{name}.clap"));
    std::fs::copy(library(), &path).expect("copy the plugin");
    path
}

/// **A plugin that aborts in its init is a crash, and a blacklistable one**: no
/// result file, a signal for an exit.
#[test]
fn a_plugin_that_crashes_in_init_is_a_crash() {
    let dir = tempfile::tempdir().unwrap();
    let err = probe(&clap_named(&dir, "init-crash")).expect_err("must not probe");
    assert!(matches!(err, BridgeError::ProcessCrashed), "{err:?}");
    let failure = probe_failure(err);
    assert!(failure.blacklistable, "{failure:?}");
    assert!(failure.reason.contains("crashed"), "{failure:?}");
}

/// **A plugin that never returns from its init is a timeout**, and the probe is
/// killed rather than waited for. The deadline is the caller's: two seconds here.
#[test]
fn a_plugin_that_hangs_in_init_times_out() {
    let dir = tempfile::tempdir().unwrap();
    let started = Instant::now();
    let err = probe_with_timeout(&clap_named(&dir, "init-hang"), Duration::from_secs(2))
        .expect_err("must not probe");
    assert!(matches!(err, BridgeError::Timeout { .. }), "{err:?}");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    let failure = probe_failure(err);
    assert!(failure.blacklistable, "{failure:?}");
    assert!(failure.reason.contains("timed out"), "{failure:?}");
}

/// A plugin whose init refuses is a result: the probe finished, with the
/// plugin's reason, and the scan records it.
#[test]
fn a_plugin_that_refuses_to_load_is_a_typed_failure() {
    let dir = tempfile::tempdir().unwrap();
    let err = probe(&clap_named(&dir, "init-refuse")).expect_err("must not probe");
    assert!(
        matches!(&err, BridgeError::LoadFailed { reason, .. }
        if reason.contains("Entry init failed")),
        "{err:?}"
    );
    assert!(probe_failure(err).blacklistable);
}

/// **A plugin that prints from its init still probes**: stdout and stderr carry
/// nothing the host reads, so two thousand lines of each corrupt nothing and
/// fill no pipe the probe would block on.
#[test]
fn a_plugin_that_prints_during_init_still_probes() {
    let dir = tempfile::tempdir().unwrap();
    let path = clap_named(&dir, "init-print");

    // The plugin does print: run the probe by hand with its stdout captured.
    let out = std::process::Command::new(tutti_fixtures::plugin_probe())
        .arg(&path)
        .arg(dir.path().join("direct.result"))
        .output()
        .expect("spawn");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("shared plugin init: stdout line 1999"),
        "the plugin printed nothing, so this tests nothing"
    );

    let d = probe(&path).expect("probes");
    assert_shared_plugin(&d, "clap");
}
