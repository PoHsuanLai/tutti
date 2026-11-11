#![forbid(unsafe_code)]
#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod audio_pipeline;
mod loaders;
mod plugin;
mod server;
mod session;
mod transport;

pub use server::PluginServer;
pub use tutti_plugin::server::BridgeConfig;

// Test-only global allocator for the audio-pipeline RT-safety regression
// test (`audio_pipeline::tests::process_is_alloc_free`). Active only in the
// test build; normal builds use the system allocator.
#[cfg(test)]
#[global_allocator]
static RT_NO_ALLOC_HARNESS: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

/// Plugin-loading tests across the crate serialize on this lock: format
/// modules keep process globals of their own, and plugins' `main` may too.
///
/// Gated on a loader feature as well as `test`, for the same reason
/// `loaders::common` is: every consumer lives in `loaders::{clap,vst3}` or
/// `plugin`, all of which are feature-gated, so a test build with no loader on
/// compiles this module with nothing to serialize.
///
/// The lock is the only member shared by every loader. Everything else is
/// gated to the *specific* loader it serves — a `vst2`-only build has no CLAP
/// probe and no VST3 bundle — and the `env!` constants especially, since those
/// read build-script variables that only exist once that loader has built its
/// reference plugin.
#[cfg(all(
    test,
    any(
        all(feature = "au", target_os = "macos"),
        feature = "clap",
        feature = "vst2",
        feature = "vst3"
    )
))]
pub(crate) mod test_utils {
    // Every import here belongs to the probe-path helpers, which are gated to
    // the loader whose reference plugin they find.
    // `Path`/`PathBuf` belong to the VST3 bundle walk; VST2 and CLAP only
    // resolve a path string.
    #[cfg(any(feature = "clap", feature = "vst3"))]
    use std::path::Path;
    #[cfg(feature = "vst3")]
    use std::path::PathBuf;
    #[cfg(any(feature = "clap", feature = "vst3", feature = "vst2"))]
    use std::sync::OnceLock;
    #[cfg(any(
        all(feature = "au", target_os = "macos"),
        feature = "clap",
        feature = "vst3",
        feature = "vst2"
    ))]
    use std::sync::{Mutex, MutexGuard};
    #[cfg(feature = "clap")]
    use tutti_fixtures::{ClapProbe, ClapSwitch};

    /// Taken by every loader test that loads a real plugin — CLAP, VST3, VST2
    /// and, on macOS, AU. A plugin's entry point may touch process globals, so
    /// loads are kept one at a time in tests.
    ///
    /// AU is gated on the target as well as the feature, matching the module's
    /// own `cfg`: the AU tests are `all(feature = "au", target_os = "macos")`,
    /// so an `au`-only build on macOS must still find this lock.
    #[cfg(any(
        all(feature = "au", target_os = "macos"),
        feature = "clap",
        feature = "vst3",
        feature = "vst2"
    ))]
    pub static PLUGIN_LOAD_LOCK: Mutex<()> = Mutex::new(());

    /// The reference VST3 bundle's parent directory, forwarded from
    /// `tutti-vst3-host` by `build.rs`. Empty when that crate built no probe.
    #[cfg(feature = "vst3")]
    const VST3_PROBE_DIR: &str = env!("TUTTI_VST3_PROBE_DIR");

    /// Absolute path to `audio-probe`, the reference VST3 binary the loader
    /// tests drive.
    ///
    /// The counterpart to [`clap_probe_path`], and it panics for the same
    /// reason. `audio-probe` is built by `tutti-vst3-host`'s build script
    /// against the in-repo SDK submodules, as a dev-dependency of this crate,
    /// during this same `cargo test` — so its absence is a build failure, not a
    /// property of the machine.
    ///
    /// `TUTTI_TEST_VST3_PLUGIN` overrides, for running these tests
    /// against a real third-party plugin.
    ///
    /// # Panics
    ///
    /// If the probe is absent — see above. The message names the likely cause.
    #[cfg(feature = "vst3")]
    pub fn vst3_probe_path() -> &'static str {
        static RESOLVED: OnceLock<String> = OnceLock::new();
        RESOLVED
            .get_or_init(|| {
                if let Some(p) = std::env::var_os("TUTTI_TEST_VST3_PLUGIN") {
                    let p = PathBuf::from(p);
                    assert!(
                        p.exists(),
                        "TUTTI_TEST_VST3_PLUGIN={} does not exist",
                        p.display()
                    );
                    return p.to_string_lossy().into_owned();
                }

                assert!(
                    !VST3_PROBE_DIR.is_empty(),
                    "no VST3 probe directory: `tutti-vst3-host` did not export one. \
                     It is a dev-dependency of this crate with `features = \
                     [\"conformance\"]`, so this means its build script did not run \
                     or did not build the probe."
                );

                let bundle = Path::new(VST3_PROBE_DIR).join("audio-probe.vst3");
                binary_in_bundle(&bundle).unwrap_or_else(|| {
                    panic!(
                        "audio-probe not found under {}. `tutti-vst3-host`'s build \
                         script builds it from the in-repo SDK submodules, so this is \
                         a build failure. If the SDK submodules are empty, run: \
                         git submodule update --init --recursive",
                        bundle.display()
                    )
                })
            })
            .as_str()
    }

    /// Absolute path to the SDK's `multiple_programchanges` sample, built beside
    /// the probe.
    ///
    /// The one fixture that can witness a VST3 **program list id**: it declares
    /// 16 lists whose ids are `kProgramStartId + i`, so an id is provably not a
    /// position. `audio-probe` publishes no program lists, so it cannot stand in.
    ///
    /// # Panics
    ///
    /// If absent. It is built from the in-repo SDK submodule by the same
    /// `cargo test`, so that is a build failure. The three tests using it used
    /// to skip on a missing `VST3_SAMPLE_PLUGIN_DIR`, which was every machine.
    #[cfg(feature = "vst3")]
    pub fn vst3_program_sample_path() -> &'static str {
        static RESOLVED: OnceLock<String> = OnceLock::new();
        RESOLVED
            .get_or_init(|| {
                assert!(
                    !VST3_PROBE_DIR.is_empty(),
                    "no VST3 probe directory: `tutti-vst3-host` did not export one."
                );
                let bundle = Path::new(VST3_PROBE_DIR).join("multiple-program-changes.vst3");
                binary_in_bundle(&bundle).unwrap_or_else(|| {
                    panic!(
                        "the multiple-program-changes sample was not found under {}. \
                         `tutti-vst3-host`'s build script builds it from the in-repo \
                         SDK submodule, so this is a build failure. If the submodules \
                         are empty, run: git submodule update --init --recursive",
                        bundle.display()
                    )
                })
            })
            .as_str()
    }

    /// The single binary inside a `.vst3` bundle.
    ///
    /// The layout is the plugin format's, not cargo's: one of these arch dirs
    /// holds it. All are probed rather than deriving one from `cfg!`, so a
    /// cross-build lands in the slower branch instead of a wrong answer.
    #[cfg(feature = "vst3")]
    fn binary_in_bundle(bundle: &Path) -> Option<String> {
        // `any_*`: the corpus may hold a cross-built fixture, and finding it is
        // better than skipping it. The arch list and the linker-by-product skip
        // both live in `tutti_plugin_types::bundle` now.
        tutti_plugin_types::bundle::any_module_in_bundle(
            bundle,
            tutti_plugin_types::bundle::ModuleKind::Vst3,
        )
        .map(|p| p.to_string_lossy().into_owned())
    }

    /// The reference plugin under a `.clap` extension.
    ///
    /// `Plugin::load` (the `crate::plugin` enum) dispatches on the file extension,
    /// and the cdylib cargo builds is `libtutti_clap_test_plugin.so` — which
    /// that table maps to **VST2**, so loading it through the format-detecting
    /// path fails with `NotAPlugin`. Tests that go through `HostMessage::
    /// LoadPlugin` (rather than calling `ClapInstance::load` directly) need a
    /// path that ends in `.clap`.
    ///
    /// A symlink beside the artifact rather than a copy, so it cannot go
    /// stale against a rebuilt plugin. The link path is shared by every test
    /// process; `tutti_fixtures::publish_with_extension` says how it is
    /// published without a reader ever missing it.
    #[cfg(feature = "clap")]
    pub fn clap_probe_path_dot_clap() -> &'static str {
        static LINKED: OnceLock<String> = OnceLock::new();
        LINKED
            .get_or_init(|| {
                tutti_fixtures::publish_with_extension(Path::new(clap_probe_path()), "clap")
                    .to_string_lossy()
                    .into_owned()
            })
            .as_str()
    }

    /// [`RenderMode::TagOnly`] on the reference plugin: every output sample is
    /// a nonzero per-`(port, channel)` tag, independent of input.
    ///
    /// Mirrors `tutti_clap_test_plugin::RenderMode`; kept as a bare constant
    /// because the value crosses a `dlopen` boundary as a `u32`.
    #[cfg(feature = "clap")]
    pub const CLAP_PROBE_RENDER_TAG_ONLY: u32 = 2;

    /// Set the reference plugin's render mode.
    ///
    /// The probe defaults to `Inert` — it writes nothing — so a test that
    /// asserts audio *came out* has to opt in. It is an effect, not a synth:
    /// there is no note-to-audio path to exercise, and `TagOnly` is what makes
    /// "the host collected this plugin's output at all" observable.
    ///
    /// Reaches the plugin through its already-loaded `dlopen` handle rather
    /// than the `rlib`: the running instance's `RENDER_MODE` static is the one
    /// in the *dynamic library*, and a value stored through the rlib's copy
    /// would be a different static that `process` never reads.
    #[cfg(feature = "clap")]
    pub fn set_clap_probe_render_mode(mode: u32) {
        ClapProbe::get().set(ClapSwitch::RenderMode, mode);
    }

    /// [`set_clap_probe_render_mode`] until the returned guard drops, which
    /// puts the plugin back to `Inert`.
    #[cfg(feature = "clap")]
    #[must_use = "the plugin goes back to `Inert` when the guard drops"]
    pub fn clap_probe_render_mode(mode: u32) -> ClapProbeRenderMode {
        set_clap_probe_render_mode(mode);
        ClapProbeRenderMode
    }

    /// Select the reference plugin's `process` status
    /// (`tutti_clap_test_plugin::StatusMode`) until the returned guard drops,
    /// which restores `Continue`. `Error` makes every block fail inside the
    /// plugin, a failure of the plugin's own and not of the block's shape.
    #[cfg(feature = "clap")]
    #[must_use = "the plugin goes back to `Continue` when the guard drops"]
    pub fn clap_probe_status_mode(mode: tutti_clap_test_plugin::StatusMode) -> ClapProbeStatusMode {
        ClapProbe::get().set(ClapSwitch::StatusMode, mode as u32);
        ClapProbeStatusMode
    }

    /// Restores the reference plugin's `Continue` status on drop; see
    /// [`clap_probe_status_mode`].
    #[cfg(feature = "clap")]
    pub struct ClapProbeStatusMode;

    #[cfg(feature = "clap")]
    impl Drop for ClapProbeStatusMode {
        fn drop(&mut self) {
            ClapProbe::get().set(
                ClapSwitch::StatusMode,
                tutti_clap_test_plugin::StatusMode::Continue as u32,
            );
        }
    }

    /// Restores the reference plugin's `Inert` render mode on drop; see
    /// [`clap_probe_render_mode`].
    #[cfg(feature = "clap")]
    pub struct ClapProbeRenderMode;

    #[cfg(feature = "clap")]
    impl Drop for ClapProbeRenderMode {
        fn drop(&mut self) {
            set_clap_probe_render_mode(tutti_clap_test_plugin::RenderMode::Inert as u32);
        }
    }

    /// Select the reference plugin's audio-port layout
    /// (`tutti_clap_test_plugin::PortLayoutMode`) until the returned guard
    /// drops, which restores `SymmetricStereo`. Call before loading: the host
    /// reads the layout once, at load.
    ///
    /// Reached through the `dlopen` handle for the same reason as
    /// [`set_clap_probe_render_mode`].
    #[cfg(feature = "clap")]
    #[must_use = "the plugin goes back to `SymmetricStereo` when the guard drops"]
    pub fn clap_probe_port_layout(mode: u32) -> ClapProbePortLayout {
        set_clap_probe_port_layout(mode);
        ClapProbePortLayout
    }

    /// Restores the reference plugin's `SymmetricStereo` layout on drop; see
    /// [`clap_probe_port_layout`].
    #[cfg(feature = "clap")]
    pub struct ClapProbePortLayout;

    #[cfg(feature = "clap")]
    impl Drop for ClapProbePortLayout {
        fn drop(&mut self) {
            set_clap_probe_port_layout(
                tutti_clap_test_plugin::PortLayoutMode::SymmetricStereo as u32,
            );
        }
    }

    #[cfg(feature = "clap")]
    fn set_clap_probe_port_layout(mode: u32) {
        ClapProbe::get().set(ClapSwitch::PortLayout, mode);
    }

    /// Make the reference plugin's ports on the sides in `mask` advertise
    /// `CLAP_AUDIO_PORT_SUPPORTS_64BITS` (`tutti_clap_test_plugin::PORTS_64BIT_*`)
    /// until the returned guard drops, which restores the f32-only default.
    /// Call before loading: the host reads the port flags once, at load.
    ///
    /// Reached through the `dlopen` handle for the same reason as
    /// [`set_clap_probe_render_mode`].
    #[cfg(feature = "clap")]
    #[must_use = "the plugin goes back to f32-only when the guard drops"]
    pub fn clap_probe_ports_64bit(mask: u32) -> ClapProbePorts64 {
        set_clap_probe_ports_64bit(mask);
        ClapProbePorts64
    }

    /// Restores the reference plugin's f32-only ports on drop; see
    /// [`clap_probe_ports_64bit`].
    #[cfg(feature = "clap")]
    pub struct ClapProbePorts64;

    #[cfg(feature = "clap")]
    impl Drop for ClapProbePorts64 {
        fn drop(&mut self) {
            set_clap_probe_ports_64bit(0);
        }
    }

    #[cfg(feature = "clap")]
    fn set_clap_probe_ports_64bit(mask: u32) {
        ClapProbe::get().set(ClapSwitch::Ports64Bit, mask);
    }

    /// Make the reference plugin call `audio-ports.rescan(flags)` at its next
    /// `on_main_thread` (see `tutti_test_plugin_request_ports_rescan`) until
    /// the returned guard drops, which clears a call not yet made.
    #[cfg(feature = "clap")]
    #[must_use = "a call not yet made is cleared when the guard drops"]
    pub fn clap_probe_request_ports_rescan(flags: u32) -> ClapProbePortsRescan {
        ClapProbe::get().set(ClapSwitch::RequestPortsRescan, flags);
        ClapProbePortsRescan
    }

    /// Clears a pending `audio-ports.rescan` call on drop; see
    /// [`clap_probe_request_ports_rescan`].
    #[cfg(feature = "clap")]
    pub struct ClapProbePortsRescan;

    #[cfg(feature = "clap")]
    impl Drop for ClapProbePortsRescan {
        fn drop(&mut self) {
            ClapProbe::get().set(ClapSwitch::RequestPortsRescan, 0);
        }
    }

    /// Make the reference plugin call `host.request_restart()` from its next
    /// `process`, until the returned guard drops (which clears a call not yet
    /// made).
    #[cfg(feature = "clap")]
    #[must_use = "a call not yet made is cleared when the guard drops"]
    pub fn clap_probe_request_restart() -> ClapProbeRestart {
        ClapProbe::get().thread_command(tutti_clap_test_plugin::CMD_REQUEST_RESTART);
        ClapProbeRestart
    }

    /// Clears a pending `request_restart` on drop; see
    /// [`clap_probe_request_restart`].
    #[cfg(feature = "clap")]
    pub struct ClapProbeRestart;

    #[cfg(feature = "clap")]
    impl Drop for ClapProbeRestart {
        fn drop(&mut self) {
            ClapProbe::get().thread_command(tutti_clap_test_plugin::CMD_NONE);
        }
    }

    /// Make the reference plugin run `cmd` (`tutti_clap_test_plugin::CMD_*`)
    /// at its next legal call site until the returned guard drops, which
    /// clears one not yet run: `on_main_thread` for the timer commands,
    /// `process` for the restart ones.
    #[cfg(feature = "clap")]
    #[must_use = "a command not yet run is cleared when the guard drops"]
    pub fn clap_probe_thread_command(cmd: u32) -> ClapProbeRestart {
        ClapProbe::get().thread_command(cmd);
        ClapProbeRestart
    }

    /// Make the reference plugin call `host.params.rescan` as
    /// `tutti_clap_test_plugin::PARAM_CMD_*` says at its next `on_main_thread`,
    /// until the returned guard drops, which clears one not yet made.
    #[cfg(feature = "clap")]
    #[must_use = "a call not yet made is cleared when the guard drops"]
    pub fn clap_probe_param_command(cmd: u32) -> ClapProbeParamCommand {
        ClapProbe::get().param_command(cmd);
        ClapProbeParamCommand
    }

    /// Clears a pending params command on drop; see [`clap_probe_param_command`].
    #[cfg(feature = "clap")]
    pub struct ClapProbeParamCommand;

    #[cfg(feature = "clap")]
    impl Drop for ClapProbeParamCommand {
        fn drop(&mut self) {
            ClapProbe::get().param_command(tutti_clap_test_plugin::PARAM_CMD_NONE);
        }
    }

    /// Make the reference plugin's latency `extra` frames longer from its next
    /// activation on (see `tutti_test_plugin_set_latency_at_activation`), until
    /// the returned guard drops. The plugin does not call `changed()`.
    #[cfg(feature = "clap")]
    #[must_use = "the latency goes back when the guard drops"]
    pub fn clap_probe_latency_at_activation(extra: u32) -> ClapProbeLatency {
        ClapProbe::get().set(ClapSwitch::LatencyAtActivation, extra);
        ClapProbeLatency
    }

    /// Restores the reference plugin's latency on drop; see
    /// [`clap_probe_latency_at_activation`].
    #[cfg(feature = "clap")]
    pub struct ClapProbeLatency;

    #[cfg(feature = "clap")]
    impl Drop for ClapProbeLatency {
        fn drop(&mut self) {
            ClapProbe::get().set(ClapSwitch::LatencyAtActivation, 0);
        }
    }

    /// Make the reference plugin call `latency.changed()` from inside every
    /// `activate`, until the returned guard drops.
    #[cfg(feature = "clap")]
    #[must_use = "the plugin goes back to silence when the guard drops"]
    pub fn clap_probe_latency_changed_in_activate() -> ClapProbeChangedInActivate {
        ClapProbe::get().set(ClapSwitch::LatencyChangedInActivate, 1);
        ClapProbeChangedInActivate
    }

    /// Stops the announcement on drop; see [`clap_probe_latency_changed_in_activate`].
    #[cfg(feature = "clap")]
    pub struct ClapProbeChangedInActivate;

    #[cfg(feature = "clap")]
    impl Drop for ClapProbeChangedInActivate {
        fn drop(&mut self) {
            ClapProbe::get().set(ClapSwitch::LatencyChangedInActivate, 0);
        }
    }

    /// What the reference plugin recorded of its threading and timer calls
    /// since the last reset.
    #[cfg(feature = "clap")]
    pub fn clap_probe_thread_capture() -> tutti_clap_test_plugin::ThreadCapture {
        ClapProbe::get().thread_capture()
    }

    /// Zeroes [`clap_probe_thread_capture`].
    #[cfg(feature = "clap")]
    pub fn clap_probe_thread_reset() {
        ClapProbe::get().thread_reset();
    }

    /// How many times the reference plugin accepted `activate` since
    /// [`clap_probe_reset_activation_count`]: one per activation, so a
    /// re-activation shows as one more.
    #[cfg(feature = "clap")]
    pub fn clap_probe_activation_count() -> u32 {
        ClapProbe::get().activation_count()
    }

    /// Zeroes [`clap_probe_activation_count`].
    #[cfg(feature = "clap")]
    pub fn clap_probe_reset_activation_count() {
        ClapProbe::get().reset_activation();
    }

    /// Make the reference plugin's `activate` refuse `rate` until the returned
    /// guard drops, which lifts the refusal.
    #[cfg(feature = "clap")]
    #[must_use = "the refusal is lifted when the guard drops"]
    pub fn clap_probe_refuse_rate(rate: f64) -> ClapProbeRefusal {
        set_clap_probe_activate_refusal(rate.to_bits());
        ClapProbeRefusal
    }

    /// Lifts the refusal on drop; see [`clap_probe_refuse_rate`].
    #[cfg(feature = "clap")]
    pub struct ClapProbeRefusal;

    #[cfg(feature = "clap")]
    impl Drop for ClapProbeRefusal {
        fn drop(&mut self) {
            set_clap_probe_activate_refusal(u64::MAX);
        }
    }

    #[cfg(feature = "clap")]
    fn set_clap_probe_activate_refusal(rate_bits: u64) {
        ClapProbe::get().set_activate_refusal(rate_bits);
    }

    /// The rate of the reference plugin's most recent accepted `activate`
    /// since [`clap_probe_reset_activation_count`], if any.
    #[cfg(feature = "clap")]
    pub fn clap_probe_last_accepted_rate() -> Option<f64> {
        ClapProbe::get().last_accepted_rate()
    }

    /// Make the reference plugin withhold `clap.tail` until the returned guard
    /// drops, which restores it. Call before loading: the host asks for the
    /// plugin's extensions once, at load.
    #[cfg(feature = "clap")]
    #[must_use = "the plugin offers clap.tail again when the guard drops"]
    pub fn clap_probe_without_tail() -> ClapProbeWithoutTail {
        ClapProbe::get().set(ClapSwitch::TailExtension, 0);
        ClapProbeWithoutTail
    }

    /// Restores the reference plugin's `clap.tail` on drop; see
    /// [`clap_probe_without_tail`].
    #[cfg(feature = "clap")]
    pub struct ClapProbeWithoutTail;

    #[cfg(feature = "clap")]
    impl Drop for ClapProbeWithoutTail {
        fn drop(&mut self) {
            ClapProbe::get().set(ClapSwitch::TailExtension, 1);
        }
    }

    /// Absolute path to `tutti-clap-test-plugin`, the reference CLAP cdylib the
    /// loader tests drive.
    ///
    /// This replaced a hard-coded `/Library/Audio/Plug-Ins/CLAP/…` path, which
    /// made the whole CLAP suite macOS-only *and* dependent on a third-party
    /// plugin being installed. The reference plugin is a dev-dependency built
    /// by the same `cargo test` run, so the suite works on any machine.
    ///
    /// Resolution (newest candidate wins) and the panic-on-absence rule are
    /// `tutti_fixtures`'s; see that crate for why each matters.
    /// Absolute path to the reference VST2 cdylib the loader tests drive.
    ///
    /// The VST2 counterpart to [`clap_probe_path`], resolved the same way:
    /// `tutti-vst2-test-plugin` is a dev-dependency, so cargo builds the
    /// cdylib during this same `cargo test`, and the resolver finds it beside the
    /// test binary. Its behaviour is steered per-test with the
    /// `TUTTI_VST2_PROBE_*` env vars rather than by building variants.
    ///
    /// # Panics
    ///
    /// If no candidate exists — that is a build failure, not a property of the
    /// machine, since the cdylib is a dev-dependency of this crate.
    #[cfg(feature = "vst2")]
    pub fn vst2_probe_path() -> &'static str {
        static RESOLVED: OnceLock<String> = OnceLock::new();
        RESOLVED
            .get_or_init(|| {
                tutti_fixtures::cdylib("tutti_vst2_test_plugin", "tutti-vst2-test-plugin")
                    .to_string_lossy()
                    .into_owned()
            })
            .as_str()
    }

    /// The reference VST2 plugin under a `.vst` name, for a test that goes
    /// through [`crate::plugin::Plugin::open`], which routes on the extension.
    ///
    /// The cdylib is a `.so`, `.dll` or `.dylib` by platform, and `.dylib` is no
    /// VST2 extension, so the image is copied once per process.
    ///
    /// # Panics
    ///
    /// If the copy fails.
    #[cfg(feature = "vst2")]
    pub fn vst2_probe_staged_path() -> &'static std::path::Path {
        static STAGED: OnceLock<std::path::PathBuf> = OnceLock::new();
        STAGED.get_or_init(|| {
            let staged = std::env::temp_dir().join(format!(
                "tutti-server-vst2-probe-{}.vst",
                std::process::id()
            ));
            std::fs::copy(vst2_probe_path(), &staged)
                .unwrap_or_else(|e| panic!("staging the VST2 probe at {staged:?} failed: {e}"));
            staged
        })
    }

    #[cfg(feature = "clap")]
    pub fn clap_probe_path() -> &'static str {
        static RESOLVED: OnceLock<String> = OnceLock::new();
        RESOLVED
            .get_or_init(|| {
                tutti_fixtures::cdylib("tutti_clap_test_plugin", "tutti-clap-test-plugin")
                    .to_string_lossy()
                    .into_owned()
            })
            .as_str()
    }

    /// Acquire the plugin-load lock, recovering from poisoning.
    ///
    /// These tests load real third-party plugins, and a single failing
    /// assertion (e.g. a plugin that doesn't advertise an expected
    /// capability) panics while holding the lock — poisoning it. Without
    /// recovery, every *other* plugin test then fails with `PoisonError`,
    /// turning one real failure into dozens of misleading cascade failures.
    /// The guarded data is `()`, so there's no invariant a poisoned lock
    /// could violate; recovering is safe and keeps failures isolated.
    #[cfg(any(
        all(feature = "au", target_os = "macos"),
        feature = "clap",
        feature = "vst3",
        feature = "vst2"
    ))]
    pub fn plugin_load_lock() -> MutexGuard<'static, ()> {
        PLUGIN_LOAD_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
