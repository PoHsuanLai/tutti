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

    /// Absolute path to `tutti-clap-test-plugin`, the reference CLAP cdylib the
    /// loader tests drive.
    ///
    /// This replaced a hard-coded `/Library/Audio/Plug-Ins/CLAP/…` path, which
    /// made the whole CLAP suite macOS-only *and* dependent on a third-party
    /// plugin being installed. The reference plugin is a dev-dependency built
    /// by the same `cargo test` run, so the suite works on any machine.
    ///
    /// Picks the **newest** existing candidate, not the first. Cargo builds
    /// into `<profile>/deps/` and hardlinks up to `<profile>/` without always
    /// refreshing it, so the two can hold different builds of the same plugin.
    /// Loading the older one is worse than loading none: the suite runs green
    /// against a plugin whose behaviour no longer matches what the test sets.
    ///
    /// # Panics
    ///
    /// If no candidate exists. Deliberate: the plugin is built by this same
    /// `cargo test` invocation, so its absence is a build failure, not a
    /// property of the machine. A skip here would be indistinguishable from a
    /// pass — the exact shape that once let 55 integration tests report `ok`
    /// having executed nothing.
    /// A loadable VST3 binary, or `None` when this machine has none.
    ///
    /// Unlike CLAP, there is no VST3 plugin this workspace can build from its
    /// own sources: `tutti-vst3-host`'s `audio-probe` needs a **Steinberg SDK
    /// checkout** (`VST3_SDK_DIR` plus the `conformance` feature) to compile,
    /// so it cannot be a plain dev-dependency the way the CLAP probe is. Until
    /// that changes, VST3 coverage here is opt-in:
    ///
    /// - `TUTTI_TEST_VST3_PLUGIN` — an explicit path to any VST3 binary.
    /// - `VST3_PROBE_DIR` — the directory `tutti-vst3-host`'s `build.rs`
    ///   exports when built with `--features conformance` and `VST3_SDK_DIR`
    ///   set; the probe bundle is found inside it.
    ///
    /// Returning `None` is what lets a caller **skip** rather than fail. That
    /// is a deliberate exception to this workspace's "absence is a hard
    /// failure" rule, and it is narrow: the CLAP probe is built by the same
    /// `cargo test` run, so its absence really is a build failure, whereas a
    /// VST3 binary is a property of the machine. The rule's actual hazard — a
    /// silent skip reading as a pass — is handled by making every caller print
    /// why it skipped.
    pub fn vst3_plugin_path() -> Option<&'static str> {
        static RESOLVED: OnceLock<Option<String>> = OnceLock::new();
        RESOLVED
            .get_or_init(|| {
                if let Some(p) = std::env::var_os("TUTTI_TEST_VST3_PLUGIN") {
                    let p = PathBuf::from(p);
                    if p.exists() {
                        return Some(p.to_string_lossy().into_owned());
                    }
                }
                let dir = std::env::var("VST3_PROBE_DIR").ok()?;
                if dir.is_empty() {
                    return None;
                }
                let bundle = Path::new(&dir).join("audio-probe.vst3");
                for sub in [
                    "Contents/x86_64-linux",
                    "Contents/aarch64-linux",
                    "Contents/MacOS",
                    "Contents/x86_64-win",
                ] {
                    let dir = bundle.join(sub);
                    let Ok(entries) = std::fs::read_dir(&dir) else {
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
            })
            .as_deref()
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

    pub fn clap_probe_path() -> &'static str {
        static RESOLVED: OnceLock<String> = OnceLock::new();
        RESOLVED
            .get_or_init(|| {
                let newest = CLAP_PROBE_CANDIDATES
                    .split(';')
                    .filter(|s| !s.is_empty())
                    .filter(|c| Path::new(c).is_file())
                    .filter_map(|c| {
                        let mtime = std::fs::metadata(c).and_then(|m| m.modified()).ok()?;
                        Some((mtime, c))
                    })
                    .max_by_key(|(mtime, _)| *mtime);
                match newest {
                    Some((_, path)) => path.to_string(),
                    None => panic!(
                        "reference plugin `tutti-clap-test-plugin` not found.\n\
                         Searched: {CLAP_PROBE_CANDIDATES}\n\
                         It is a dev-dependency of this crate, so `cargo test` \
                         should have built it. If this fires, the cdylib landed \
                         somewhere build.rs does not name."
                    ),
                }
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
