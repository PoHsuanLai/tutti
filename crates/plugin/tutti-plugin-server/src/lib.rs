#![doc = include_str!("../README.md")]

mod audio_pipeline;
mod editor;
mod loaders;
mod plugin;
mod server;
mod session;
mod transport;

pub use server::PluginServer;
pub use tutti_plugin::server::BridgeConfig;
pub use tutti_plugin::{BridgeError, Result};

// Test-only global allocator for the audio-pipeline RT-safety regression
// test (`audio_pipeline::tests::process_is_alloc_free`). Active only in the
// test build; normal builds use the system allocator.
#[cfg(test)]
#[global_allocator]
static RT_NO_ALLOC_HARNESS: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

/// VST2's `vst` crate uses a global `LOAD_POINTER` static during plugin
/// loading that is not thread-safe. All plugin-loading tests across the
/// crate must serialize on this lock.
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
    // resolve a candidate string.
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

    /// Taken by every loader test that loads a real plugin — CLAP, VST3, VST2
    /// and, on macOS, AU. VST2 is the reason the lock exists (the `vst` crate
    /// keeps a global `LOAD_POINTER` during load), and until `loaders::vst2`
    /// grew tests it was the one loader that never took it.
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

    /// `;`-separated candidate paths for the reference CLAP plugin, emitted by
    /// `build.rs`.
    #[cfg(feature = "clap")]
    const CLAP_PROBE_CANDIDATES: &str = env!("TUTTI_CLAP_TEST_PLUGIN_CANDIDATES");

    /// `;`-separated candidate paths for the reference VST2 plugin, emitted by
    /// `build.rs`.
    #[cfg(feature = "vst2")]
    const VST2_PROBE_CANDIDATES: &str = env!("TUTTI_VST2_TEST_PLUGIN_CANDIDATES");

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
        for sub in [
            "Contents/x86_64-linux",
            "Contents/aarch64-linux",
            "Contents/MacOS",
            "Contents/x86_64-win",
        ] {
            let Ok(entries) = std::fs::read_dir(bundle.join(sub)) else {
                continue;
            };
            for e in entries.flatten() {
                let path = e.path();
                if path.is_file() {
                    return Some(path.to_string_lossy().into_owned());
                }
            }
        }
        None
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
    /// Creates a symlink beside the artifact on first use. A symlink rather
    /// than a copy so it cannot go stale against a rebuilt plugin — the
    /// staleness hazard `clap_probe_path` already guards against by taking the
    /// newest candidate.
    ///
    /// **Published by atomic rename, because the link path is shared between
    /// processes.** `OnceLock` serializes this within one process, which is all
    /// `cargo test` needs — one binary, one process. Under a per-test-process
    /// runner (`cargo nextest`) every test process runs this against the *same*
    /// absolute path, since the candidate list is baked in at build time. The
    /// previous `remove_file` + `symlink` pair raced there: one process unlinked
    /// while another had already created, and the loser died with `EEXIST` —
    /// or worse, a third saw the path briefly absent. Failures moved around the
    /// suite run to run, which is what a shared-path race looks like from the
    /// outside.
    ///
    /// `rename` over an existing path is atomic on POSIX and replaces silently,
    /// so a concurrent reader sees either the old link or the new one and never
    /// a gap. The unique staging name keeps two creators from colliding on the
    /// temp file itself. This still recreates unconditionally, so the freshness
    /// guarantee above is unchanged.
    #[cfg(feature = "clap")]
    pub fn clap_probe_path_dot_clap() -> &'static str {
        static LINKED: OnceLock<String> = OnceLock::new();
        LINKED
            .get_or_init(|| {
                let real = Path::new(clap_probe_path());
                let link = real.with_extension("clap");

                // Stage under a name no other process can pick, then swap it in.
                let staging = link.with_extension(format!("clap.tmp{}", std::process::id()));
                let _ = std::fs::remove_file(&staging);

                #[cfg(unix)]
                std::os::unix::fs::symlink(real, &staging)
                    .expect("stage the reference plugin symlink");
                #[cfg(windows)]
                std::fs::copy(real, &staging).expect("stage the reference plugin copy");

                if let Err(e) = std::fs::rename(&staging, &link) {
                    // Losing the swap is not a failure: whoever won published a
                    // link to the same artifact. Only a missing result is fatal.
                    let _ = std::fs::remove_file(&staging);
                    assert!(
                        link.exists(),
                        "publish the reference plugin at {}: {e}",
                        link.display()
                    );
                }
                link.to_string_lossy().into_owned()
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
        // SAFETY: the symbol is `#[no_mangle] extern "C" fn(u32)` in
        // `tutti-clap-test-plugin`, and the library is already resident (the
        // instance under test holds it open), so this re-open is a refcount
        // bump rather than a fresh load.
        unsafe {
            let lib = libloading::Library::new(clap_probe_path())
                .expect("reference plugin opens for the render-mode symbol");
            let set: libloading::Symbol<unsafe extern "C" fn(u32)> = lib
                .get(b"tutti_test_plugin_set_render_mode\0")
                .expect("`tutti_test_plugin_set_render_mode` exported by the reference plugin");
            set(mode);
            // Leak the handle: dropping it would decrement the refcount on a
            // library the live instance is still using.
            std::mem::forget(lib);
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
    /// `tutti_fixture_resolve`'s; see that crate for why each matters.
    /// Absolute path to the reference VST2 cdylib the loader tests drive.
    ///
    /// The VST2 counterpart to [`clap_probe_path`], resolved the same way:
    /// `tutti-vst2-test-plugin` is a dev-dependency, so cargo builds the
    /// cdylib during this same `cargo test` and `build.rs` only reports where
    /// it landed. Its behaviour is steered per-test with the
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
                tutti_fixture_resolve::resolve_or_panic(
                    VST2_PROBE_CANDIDATES,
                    "tutti-vst2-test-plugin",
                )
            })
            .as_str()
    }

    #[cfg(feature = "clap")]
    pub fn clap_probe_path() -> &'static str {
        static RESOLVED: OnceLock<String> = OnceLock::new();
        RESOLVED
            .get_or_init(|| {
                tutti_fixture_resolve::resolve_or_panic(
                    CLAP_PROBE_CANDIDATES,
                    "tutti-clap-test-plugin",
                )
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
