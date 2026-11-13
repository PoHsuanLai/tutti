//! `Hosting::{Auto, InProcess, OutOfProcess}` through `Plugin::open`: the
//! reference CLAP plugin loaded each way (feature `clap`; the out-of-process
//! ones need `cargo build -p tutti-plugin-server` first, as `CLAUDE.md` says),
//! and a VST2 file refused for `OutOfProcess` with no server looked for.

use tutti_plugin::catalog::{Hosting, Plugin, PluginRef};
use tutti_plugin_types::{BridgeError, SampleRate};

const RATE: f64 = 48_000.0;

/// **A VST2 plugin asked to be served is refused at the load, typed,** whether
/// or not the file exists and whether or not a server can be found: the answer
/// is the format's (its `AEffect` fuses editor and audio), so it is given
/// before anything is looked for, and the request is not silently changed to
/// in process.
#[test]
fn a_vst2_plugin_asked_for_out_of_process_is_refused_at_load() {
    let mut env = tutti_fixtures::ScopedEnv::new();
    env.set("TUTTI_PLUGIN_SERVER", "/nonexistent/plugin-server");
    for name in ["Synth.vst", "Legacy.dll", "Old.so"] {
        let plugin = PluginRef::file(name)
            .expect("a plugin file")
            .with_hosting(Hosting::OutOfProcess);
        assert_eq!(plugin.hosting(), Hosting::OutOfProcess);
        match Plugin::open(None, &plugin, SampleRate::of(RATE)) {
            Err(BridgeError::NotOutOfProcess { format, .. }) => assert_eq!(format, "VST2"),
            Err(other) => panic!("{name}: expected NotOutOfProcess, got {other:?}"),
            Ok(_) => panic!("{name}: a VST2 file must not be served"),
        }
    }
}

#[cfg(feature = "clap")]
mod clap {
    use super::*;
    use tutti_plugin_types::MainThread;

    fn clap(hosting: Hosting) -> PluginRef {
        PluginRef::file(tutti_fixtures::clap_probe())
            .expect("a plugin file")
            .with_hosting(hosting)
    }

    fn with_server() -> tutti_fixtures::ScopedEnv {
        let mut env = tutti_fixtures::ScopedEnv::new();
        env.set("TUTTI_PLUGIN_SERVER", tutti_fixtures::plugin_server());
        env
    }

    /// **`Auto` serves a CLAP plugin**: the format's default is out of process
    /// (design 021 Q3), so it runs in a `plugin-server` process, which the
    /// handle names.
    #[test]
    fn auto_serves_a_clap_plugin() {
        let _env = with_server();
        let plugin = Plugin::open(None, &clap(Hosting::Auto), SampleRate::of(RATE))
            .expect("the reference plugin loads through a server");
        assert_eq!(plugin.hosting(), Hosting::OutOfProcess);
        assert!(plugin.handle().server_pid().is_some());
    }

    /// **`OutOfProcess` serves it too**, from any thread (no main-thread proof).
    #[test]
    fn out_of_process_serves_a_clap_plugin() {
        let _env = with_server();
        let plugin = Plugin::open(None, &clap(Hosting::OutOfProcess), SampleRate::of(RATE))
            .expect("the reference plugin loads through a server");
        assert_eq!(plugin.hosting(), Hosting::OutOfProcess);
        assert!(plugin.handle().server_pid().is_some());
    }

    /// **`InProcess` loads the same plugin in the host's process**: no server
    /// is looked for (none can be found here), the plugin is the one asked for
    /// and the load needs the main thread, which a caller without it is told.
    #[test]
    fn in_process_hosts_a_clap_plugin_here() {
        let mut env = tutti_fixtures::ScopedEnv::new();
        env.set("TUTTI_PLUGIN_SERVER", "/nonexistent/plugin-server");
        let asked = clap(Hosting::InProcess);

        match Plugin::open(None, &asked, SampleRate::of(RATE)) {
            Err(BridgeError::LoadFailed { reason, .. }) => {
                assert!(reason.contains("main thread"), "{reason}");
            }
            other => panic!("an in-process load without the main thread: {other:?}"),
        }

        let main = MainThread::claim().expect("one test, one process");
        let plugin = Plugin::open(Some(&main), &asked, SampleRate::of(RATE))
            .expect("the reference plugin loads in this process");
        assert_eq!(plugin.hosting(), Hosting::InProcess);
        assert_eq!(plugin.handle().server_pid(), None);
        assert_eq!(
            plugin.descriptor().id,
            tutti_clap_test_plugin::PLUGIN_ID.to_str().expect("utf-8")
        );
    }
}

/// **`Auto` hosts a VST2 plugin in process**, the only way it can run.
#[cfg(feature = "vst2")]
#[test]
fn auto_hosts_a_vst2_plugin_in_process() {
    // The probe is a plain cdylib; a host meets VST2 under a `.vst` name.
    let staged = std::env::temp_dir().join("tutti-hosting-auto.vst");
    std::fs::copy(tutti_fixtures::vst2_probe(), &staged).expect("stage the probe as a .vst");
    let main = tutti_plugin_types::MainThread::claim().expect("one test, one process");
    let plugin = Plugin::open(
        Some(&main),
        &PluginRef::file(&staged).expect("a plugin file"),
        SampleRate::of(RATE),
    )
    .expect("the reference VST2 plugin loads in this process");
    assert_eq!(plugin.hosting(), Hosting::InProcess);
}
