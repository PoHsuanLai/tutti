//! Driving the reference CLAP plugin through the **real out-of-process host**.
//!
//! `tutti-clap-host`'s own suites load the probe in-process, so they assert what
//! the CLAP loader does. These suites assert what the *host node* does: a real
//! `plugin-server` subprocess, a real socket, a real shared-memory slab, and
//! `PluginClient` as fundsp sees it. Everything between the graph and the
//! plugin's `process()` is under test, which is the half the in-process suites
//! structurally cannot reach.
//!
//! # The two artifacts, and why each is found rather than built
//!
//! `build.rs` names candidate paths for both; see there for why a nested
//! `cargo build` is not an option.
//!
//! # The dlopen seam
//!
//! The probe's switches are process-global statics **inside the cdylib**. This
//! process links the same crate as an rlib, and an rlib is a separate image with
//! separate statics — so `tutti_clap_test_plugin::set_render_mode` called
//! directly would write a static nothing ever reads. Worse here than in the
//! in-process suites: the plugin is resident in *another process entirely*, so
//! the switch has to be flipped in that process's copy.
//!
//! [`ProbeSwitches`] is therefore not usable across the subprocess boundary and
//! deliberately does not exist. Switches that the out-of-process tests need are
//! set through `TUTTI_CLAP_PROBE_*` **environment variables**, which the
//! subprocess inherits at spawn and the plugin reads on load. See
//! [`ProbeEnv`].

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use tutti_plugin::handles::{PluginClient, PluginHandle};
use tutti_plugin::BridgeConfig;

/// `;`-separated candidate paths for the reference CLAP cdylib, from `build.rs`.
const CLAP_PROBE_CANDIDATES: &str = env!("TUTTI_CLAP_TEST_PLUGIN_CANDIDATES");

/// `;`-separated candidate paths for the `plugin-server` binary, from `build.rs`.
const PLUGIN_SERVER_CANDIDATES: &str = env!("TUTTI_PLUGIN_SERVER_CANDIDATES");

/// These tests must not run concurrently, and a `Mutex` cannot enforce it.
///
/// Two things need serializing, and they need it at different scopes:
///
/// - **Wall clock.** Each test spawns a `plugin-server` subprocess and paces
///   itself to the real block period, because the pipeline submits block N and
///   collects block N-1 without waiting — the subprocess needs real time between
///   blocks or nothing is ever published and every block reads back silence.
///   Two tests running at once halve the time each subprocess gets. This is the
///   same constraint `real_plugin_pressure.rs` documents.
/// - **Process-global environment.** The probe's behaviour is selected by
///   environment variables, and `set_var` is process-wide.
///
/// **A `static Mutex` serializes neither under `cargo nextest`**, which is this
/// repo's runner: nextest gives every test its own *process*, so a
/// process-local lock is uncontended in each one and the tests run fully
/// parallel anyway. That is not a hypothetical — this suite was written with a
/// `Mutex` first and failed 2 runs in 3, always in whichever test lost the race
/// for wall clock rather than in the one that took it.
///
/// So the gate is a **lock directory**: `create_dir` is atomic and fails with
/// `AlreadyExists` on every OS this builds for, which is the one primitive that
/// works across processes without a new dependency. A directory rather than a
/// file because `File::create` truncates an existing file and succeeds, which
/// would hand the lock to everyone.
mod cross_process_lock {
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    /// Where the lock lives. Under the system temp dir rather than the target
    /// dir: every test process must agree on the path, and this one needs no
    /// build-time plumbing to compute.
    fn lock_path() -> PathBuf {
        std::env::temp_dir().join("tutti-plugin-clap-probe.lock")
    }

    /// How long to wait for the lock before assuming its holder died.
    ///
    /// A crashed test process leaves the directory behind — there is no OS
    /// cleanup for this the way there is for a real `flock` — so the wait has to
    /// end in a *steal* rather than a hang. Generous enough that a slow but
    /// live test is never robbed: the suites here run in well under a second
    /// each, so ten seconds is two orders of magnitude of headroom.
    const STALE_AFTER: Duration = Duration::from_secs(10);

    /// Held for the duration of one test; released on drop, including on the
    /// unwind of a failing assertion.
    pub struct Guard;

    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir(lock_path());
        }
    }

    /// Take the lock, waiting for whoever holds it.
    pub fn acquire() -> Guard {
        let path = lock_path();
        let deadline = Instant::now() + STALE_AFTER;
        loop {
            match std::fs::create_dir(&path) {
                Ok(()) => return Guard,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if Instant::now() >= deadline {
                        // The holder is gone or wedged. Steal rather than hang:
                        // a hung suite reports as a harness timeout with no clue
                        // which test is at fault, while a stolen lock at worst
                        // reproduces the contention this exists to prevent.
                        let _ = std::fs::remove_dir(&path);
                        continue;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                // An unusable lock path (a read-only temp dir, say) must not
                // silently disable the serialization the suites depend on, but
                // it must not fail them either — running unserialized is what
                // they did before this existed.
                Err(_) => return Guard,
            }
        }
    }
}

/// Take the machine, across processes. See [`cross_process_lock`].
pub fn exclusive() -> cross_process_lock::Guard {
    cross_process_lock::acquire()
}

/// Absolute path to the freshest built copy of the reference CLAP plugin,
/// published under a `.clap` extension.
///
/// The extension matters: the server's format dispatch is by file extension,
/// and the cdylib cargo builds is `libtutti_clap_test_plugin.so` — which that
/// table reads as **VST2**, so the CLAP loader is never reached and the load
/// fails with `NotAPlugin`.
///
/// Published by **atomic rename**, because the link path is shared between test
/// processes. `OnceLock` serializes within one process; under `cargo nextest`
/// every test process runs this against the same absolute path, since the
/// candidate list is baked in at build time. A `remove_file` + `symlink` pair
/// races there — one process unlinks while another has already created, and the
/// loser dies with `EEXIST`. `rename` over an existing path is atomic on POSIX,
/// so a concurrent reader sees the old link or the new one and never a gap.
///
/// A symlink rather than a copy so it cannot go stale against a rebuilt plugin.
///
/// # Panics
///
/// If no candidate exists. The plugin is a dev-dependency built by this same
/// `cargo test`, so that is a build failure and not a property of the machine.
pub fn clap_probe_path() -> &'static Path {
    static LINKED: OnceLock<PathBuf> = OnceLock::new();
    LINKED.get_or_init(|| {
        let real = PathBuf::from(tutti_fixture_resolve::resolve_or_panic(
            CLAP_PROBE_CANDIDATES,
            "tutti-clap-test-plugin",
        ));
        let link = real.with_extension("clap");

        // Stage under a name no other process can pick, then swap it in.
        let staging = link.with_extension(format!("clap.tmp{}", std::process::id()));
        let _ = std::fs::remove_file(&staging);

        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &staging).expect("stage the reference plugin symlink");
        #[cfg(windows)]
        std::fs::copy(&real, &staging).expect("stage the reference plugin copy");

        if let Err(e) = std::fs::rename(&staging, &link) {
            // Losing the swap is not a failure: whoever won published a link to
            // the same artifact. Only a missing result is fatal.
            let _ = std::fs::remove_file(&staging);
            assert!(
                link.exists(),
                "publish the reference plugin at {}: {e}",
                link.display()
            );
        }
        link
    })
}

/// Absolute path to the `plugin-server` binary these tests spawn.
///
/// # Panics
///
/// If it has not been built. Unlike the cdylibs this is **not** a
/// dev-dependency — `tutti-plugin-server` depends on `tutti-plugin`, so the edge
/// would be a cycle — so cargo does not build it as a side effect of running
/// these tests, and the message says so rather than blaming the machine.
pub fn plugin_server_path() -> &'static str {
    static RESOLVED: OnceLock<String> = OnceLock::new();
    RESOLVED
        .get_or_init(|| {
            let searched = PLUGIN_SERVER_CANDIDATES
                .split(';')
                .filter(|s| !s.is_empty())
                .map(|s| format!("\n  - {s}"))
                .collect::<String>();
            tutti_fixture_resolve::newest_existing(PLUGIN_SERVER_CANDIDATES.split(';'))
                .unwrap_or_else(|| {
                    panic!(
                        "`plugin-server` not found.\nSearched:{searched}\n\
                         It is NOT a dev-dependency of this crate (that would be a \
                         dependency cycle), so `cargo test -p tutti-plugin` does not \
                         build it. Build it first:\n  \
                         cargo build -p tutti-plugin-server"
                    )
                })
                .to_string()
        })
        .as_str()
}

/// The probe's `RenderMode` discriminants, which cross the process boundary as
/// bare `u32`s in an environment variable.
///
/// Mirrors `tutti_clap_test_plugin::RenderMode`. Pinned against the plugin's own
/// enum by `probe_render_modes_match_the_plugin`, so the duplication cannot
/// silently drift into selecting a different renderer than the caller asked for.
pub mod render {
    pub const INERT: u32 = 0;
    pub const TAG_PASSTHROUGH: u32 = 1;
    pub const TAG_ONLY: u32 = 2;
    pub const LATENCY: u32 = 3;
}

/// Environment the probe reads **in the subprocess**, on load.
///
/// The switches are `extern "C"` symbols in the cdylib, and the cdylib lives in
/// another process — so a test cannot call them. It sets them here instead, and
/// the values ride across on the spawn. `Drop` clears every variable it set, so
/// a failing assertion (which unwinds) cannot leave the next test's subprocess
/// configured for this one's behaviour.
///
/// Only valid while [`exclusive`] is held: `set_var` is process-global.
pub struct ProbeEnv {
    keys: Vec<&'static str>,
}

impl ProbeEnv {
    /// An empty environment — the probe's own defaults.
    pub fn new() -> Self {
        Self { keys: Vec::new() }
    }

    /// What the plugin writes into its outputs. See [`render`].
    pub fn render_mode(mut self, mode: u32) -> Self {
        self.set("TUTTI_CLAP_PROBE_RENDER_MODE", mode.to_string());
        self
    }

    /// Apply `kParamGain`-equivalent gain to the rendered output.
    pub fn gain_enabled(mut self, on: bool) -> Self {
        self.set("TUTTI_CLAP_PROBE_APPLY_GAIN", u8::from(on).to_string());
        self
    }

    /// Abort the process on the `n`th `process()` call (1-based). `0` disables.
    pub fn crash_on_block(mut self, n: u32) -> Self {
        self.set("TUTTI_CLAP_PROBE_CRASH_ON_BLOCK", n.to_string());
        self
    }

    /// Park `process()` on an atomic from the `n`th call (1-based) until
    /// released. `0` disables.
    pub fn block_from(mut self, n: u32) -> Self {
        self.set("TUTTI_CLAP_PROBE_BLOCK_FROM", n.to_string());
        self
    }

    fn set(&mut self, key: &'static str, value: String) {
        // SAFETY: `exclusive()` is held for the whole test, and every test in
        // these suites takes it before touching the environment — so no other
        // thread in this process is reading or writing the environment
        // concurrently. That is the precondition `set_var` documents.
        unsafe { std::env::set_var(key, value) };
        self.keys.push(key);
    }
}

impl Default for ProbeEnv {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ProbeEnv {
    fn drop(&mut self) {
        for key in &self.keys {
            // SAFETY: as in `set`.
            unsafe { std::env::remove_var(key) };
        }
    }
}

/// A loaded probe: the audio node, its control handle, and the guard whose drop
/// tears the subprocess down.
///
/// The handle must outlive the client (they share the subprocess guard), which
/// is why both are returned together rather than the caller keeping one.
pub struct LoadedProbe {
    pub client: PluginClient,
    pub handle: PluginHandle,
}

/// Spawn a `plugin-server`, load the reference CLAP plugin into it, and return
/// the audio node.
///
/// `TUTTI_PLUGIN_SERVER` rather than a `ServerLocator`: `PluginClient::new` takes
/// a `BridgeConfig`, which carries no locator field, so the environment is the
/// only lever a test has. It is set here rather than once per suite so the value
/// is visible in the one place that depends on it.
///
/// # Panics
///
/// If the load fails — these tests are about what a *loaded* plugin does, so a
/// failure to load is a broken fixture rather than an outcome worth asserting.
pub fn load_probe(sample_rate: f64) -> LoadedProbe {
    // SAFETY: `exclusive()` is held; see `ProbeEnv::set`.
    unsafe { std::env::set_var("TUTTI_PLUGIN_SERVER", plugin_server_path()) };

    let client = PluginClient::new(
        BridgeConfig::default(),
        clap_probe_path().to_path_buf(),
        sample_rate,
    )
    .expect("load the reference CLAP plugin through a real plugin-server");
    let handle = PluginHandle::from_client(&client);
    LoadedProbe { client, handle }
}
