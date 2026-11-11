//! The plugin probe, a separate process: `tutti_plugin::catalog::probe_record`
//! spawns `plugin-probe`, which loads the plugin just far enough to read its
//! descriptor and writes it to a result file.
//!
//! One test per format reads the shared plugin's descriptor. The CLAP plugin's
//! `init` also misbehaves by file name (`clap.rs`, `InitMode`): a probe sends
//! no state and a plugin may be configured by no environment variable, so the
//! name is the one lever. Those cases are CLAP's; the other formats have none
//! to pull.
//!
//! Needs `cargo build -p tutti-plugin-probe` first.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tutti_plugin::catalog::{
    DEFAULT_PROBE_TIMEOUT, Locator, PluginDescriptor, probe_failure, probe_record_with,
};
use tutti_plugin_contract::{bundle_of, clap_path, library, vst3_cpp_module, vst3_module};
use tutti_plugin_db::PluginDb;
use tutti_plugin_types::BridgeError;

/// The one descriptor `probe_record` found at `path`.
fn probe(path: &Path) -> Result<PluginDescriptor, BridgeError> {
    probe_with_timeout(path, DEFAULT_PROBE_TIMEOUT)
}

/// [`probe`] under a deadline of the test's choosing. The probe is the one built
/// in this profile, named through [`Locator::at`], so nothing writes the
/// process-global environment.
fn probe_with_timeout(path: &Path, timeout: Duration) -> Result<PluginDescriptor, BridgeError> {
    let locator = Locator::at(tutti_fixtures::plugin_probe());
    let delta = probe_record_with(path, &locator, timeout)?;
    let mut db = PluginDb::new();
    db.apply(delta);
    let found: Vec<_> = db.plugins().map(|p| p.descriptor.clone()).collect();
    assert_eq!(found.len(), 1, "one plugin in {}", path.display());
    Ok(found.into_iter().next().unwrap())
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

// Mutation (run): `descriptor_of` in the probe binary swapped for
// `PluginDescriptor::default()` fails the format's test (each asserts the name).
#[test]
fn clap_is_probed_through_the_binary() {
    let d = probe(&clap_path()).expect("probes");
    assert_shared_plugin(&d, "clap");
    assert_eq!(d.id, "tutti.shared-test-plugin");
}

#[test]
fn vst2_is_probed_through_the_binary() {
    let d = probe(&library()).expect("probes");
    assert_shared_plugin(&d, "vst2");
}

#[test]
fn vst3_is_probed_through_the_binary() {
    let d = probe(&bundle_of(&vst3_module())).expect("probes");
    assert_shared_plugin(&d, "vst3");
}

#[test]
fn vst3_cpp_is_probed_through_the_binary() {
    let d = probe(&bundle_of(&vst3_cpp_module())).expect("probes");
    assert_named(&d, "vst3", "Tutti Shared Test Plugin (C++)");
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
///
/// Mutation (run): the plugin's `InitMode::Crash` arm returning `true` makes the
/// probe succeed and fails this; the host mapping "no file, exited" to anything
/// but `ProcessCrashed` fails the match.
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
///
/// Mutation (run): the plugin's `InitMode::Hang` arm returning `true` makes the
/// probe succeed and fails this; the host not killing the child at the deadline
/// leaves the loop in `wait` blocked on the plugin forever, which the test
/// harness' timeout reports.
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
///
/// Mutation (run): the probe binary writing `Described(default)` for an error
/// fails the `LoadFailed` match; the host dropping the message fails the
/// reason assertion.
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
///
/// Mutation (run): the host spawning the probe with `Stdio::piped()` for stdout
/// and never reading it blocks the probe once the pipe fills, and the probe
/// times out; the plugin's `InitMode::Print` arm not printing leaves the test
/// passing for the wrong reason, which is why the first assertion reads the
/// plugin's own output from a direct run.
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
