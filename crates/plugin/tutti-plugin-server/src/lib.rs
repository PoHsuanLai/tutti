//! Subprocess-side of the plugin bridge.
//!
//! `tutti-plugin-server` is the implementation behind the `plugin-server`
//! binary that [`tutti_plugin`] spawns once per loaded plugin. This crate
//! hosts the plugin in isolation, speaks the [`tutti_plugin::server`]
//! wire protocol over a Unix-socket / named-pipe, and drives audio via a
//! shared-memory slab.
//!
//! # Using the library
//!
//! Most callers want the `plugin-server` binary, not this library. Library
//! users have exactly one entry point. `no_run`: `run` binds a socket and
//! blocks for the lifetime of the session.
//!
//! ```no_run
//! use tutti_plugin_server::{BridgeConfig, PluginServer};
//!
//! // The host chooses the rendezvous path and passes it in — a subprocess
//! // deriving its own could not meet the host that spawned it. Everything else
//! // defaults; `max_buffer_size` is denominated in FRAMES and sizes the slab,
//! // so a later block may not exceed it.
//! let config = BridgeConfig {
//!     socket_path: std::env::args().nth(1).expect("socket path").into(),
//!     ..Default::default()
//! };
//!
//! // One server serves one host, then returns. A crash here takes the plugin
//! // down and leaves the host running — which is the point of the split.
//! PluginServer::new(config)
//!     .expect("record parent pid")
//!     .run()
//!     .expect("session");
//! ```
//!
//! The `socket_path` above is the one field with no safe default:
//! [`BridgeConfig::default`] derives a *unique* path per call precisely so a
//! `..Default::default()` cannot become a latent collision, in which the second
//! bridge to bind unlinks the first's live socket.
//!
//! # The wire
//!
//! Framing is a u32 big-endian length prefix plus a bincode payload. The message
//! shapes and the version constant are [`tutti_plugin`]'s — this crate imports
//! `PROTOCOL_VERSION` and never restates its history. Both phases open by
//! sending it, and a host that does not recognise the version refuses.
//!
//! This is an **IPC boundary, so the unit newtypes stop here**, as they do at
//! the C ABIs of the hosted plugin formats. A raw `f64` sample rate crossing the
//! wire or entering `AudioUnitSetParameter` is correct, not an omission.
//!
//! # Internal layout
//!
//! - `server` — outer shell ([`PluginServer`]); orchestrates the two-phase
//!   connection dance and drives a `Session` over a `Transport`.
//! - `session` — pure message-to-reaction dispatch. Owns plugin + shm +
//!   pipeline + editor state. Unit-testable without sockets.
//! - `audio_pipeline` — per-block audio machinery (scratch buffers,
//!   shared-memory I/O, plugin invocation).
//! - `plugin` — format-polymorphic plugin wrapper; hides VST2/VST3/CLAP/AU
//!   cfg-gating behind a single `Plugin` enum.
//! - `editor` — editor window state.
//! - `transport` — IPC framing; trait seam for testability.
//! - `loaders::{vst2, vst3, clap, au}` — per-format `PluginInstance` adapters.

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
#[cfg(test)]
pub(crate) mod test_utils {
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, MutexGuard, OnceLock};
    pub static PLUGIN_LOAD_LOCK: Mutex<()> = Mutex::new(());

    /// `;`-separated candidate paths for the reference CLAP plugin, emitted by
    /// `build.rs`.
    const CLAP_PROBE_CANDIDATES: &str = env!("TUTTI_CLAP_TEST_PLUGIN_CANDIDATES");

    /// The reference VST3 bundle's parent directory, forwarded from
    /// `tutti-vst3-host` by `build.rs`. Empty when that crate built no probe.
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
    /// This used to return `Option` and let callers skip, because the probe
    /// needed an external `VST3_SDK_DIR` checkout that most machines lacked.
    /// The SDK is a submodule now, so the skip has nothing left to describe.
    ///
    /// `TUTTI_TEST_VST3_PLUGIN` still overrides, for running these tests
    /// against a real third-party plugin.
    ///
    /// # Panics
    ///
    /// If the probe is absent — see above. The message names the likely cause.
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

                // The bundle layout is the plugin format's, not cargo's: one of
                // these arch dirs holds the binary. Probe all of them rather
                // than deriving one from `cfg!`, so a cross-build lands in the
                // slower branch instead of a wrong answer.
                let bundle = Path::new(VST3_PROBE_DIR).join("audio-probe.vst3");
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
                            return path.to_string_lossy().into_owned();
                        }
                    }
                }
                panic!(
                    "audio-probe not found under {}. `tutti-vst3-host`'s build \
                     script builds it from the in-repo SDK submodules, so this is \
                     a build failure. If the SDK submodules are empty, run: \
                     git submodule update --init --recursive",
                    bundle.display()
                )
            })
            .as_str()
    }

    /// The reference plugin under a `.clap` extension.
    ///
    /// [`PluginHost::load`](crate::plugin) dispatches on the file extension,
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
    pub fn clap_probe_path_dot_clap() -> &'static str {
        static LINKED: OnceLock<String> = OnceLock::new();
        LINKED
            .get_or_init(|| {
                let real = Path::new(clap_probe_path());
                let link = real.with_extension("clap");
                // Recreate unconditionally: a link left by an earlier run may
                // point at an artifact that has since been rebuilt elsewhere.
                let _ = std::fs::remove_file(&link);
                #[cfg(unix)]
                std::os::unix::fs::symlink(real, &link).expect("symlink the reference plugin");
                #[cfg(windows)]
                std::fs::copy(real, &link).expect("copy the reference plugin");
                link.to_string_lossy().into_owned()
            })
            .as_str()
    }

    /// [`RenderMode::TagOnly`] on the reference plugin: every output sample is
    /// a nonzero per-`(port, channel)` tag, independent of input.
    ///
    /// Mirrors `tutti_clap_test_plugin::RenderMode`; kept as a bare constant
    /// because the value crosses a `dlopen` boundary as a `u32`.
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
    pub fn plugin_load_lock() -> MutexGuard<'static, ()> {
        PLUGIN_LOAD_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
