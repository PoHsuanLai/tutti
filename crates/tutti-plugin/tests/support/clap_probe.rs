//! Driving the reference CLAP plugin through the **real out-of-process host**.
//!
//! `tutti-clap-host`'s own suites load the probe in-process, so they assert what
//! the CLAP loader does. These suites assert what the *host node* does: a real
//! `plugin-server` subprocess, a real socket, a real shared-memory slab, and
//! `PluginClient` as the graph's executor sees it. Everything between the graph and the
//! plugin's `process()` is under test, which is the half the in-process suites
//! structurally cannot reach.
//!
//! # The two artifacts, and why each is found rather than built
//!
//! Both are looked up beside the test binary (`tutti-fixtures`); a nested
//! `cargo build` from a test would deadlock on the target-directory lock.
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

use std::sync::OnceLock;
use std::time::Duration;

use tutti_graph::NodeKey;
use tutti_graph::contract::Latent;
use tutti_graph::{GraphBuilder, Prepare, Renderer, Unforkable};
use tutti_plugin::BridgeConfig;
use tutti_plugin::handles::{Bound, PluginClient};
use tutti_plugin_host::{PluginControls, PluginHandle};
use tutti_types::{ChannelLayout, Latency, SampleRate, Samples};

/// A bound plugin as the only node of a graph: the global inputs feed
/// its inputs in order, its outputs feed the global outputs. The executor
/// hands the node its `Env` and its whole block, as the engine does.
pub struct Rig {
    renderer: Renderer,
    key: NodeKey,
    /// What inserting the plugin handed back.
    pub controls: PluginControls,
    inputs: usize,
    outputs: usize,
}

impl Rig {
    /// `client` in a graph prepared for `rate` and blocks of `block` frames.
    pub fn new(client: PluginClient<Bound>, rate: f64, block: usize) -> Self {
        Self::prepared(client, Prepare::new(SampleRate(rate), Samples(block)))
    }

    /// `client` in a graph prepared with `prepare` (a device quantum, say).
    pub fn prepared(client: PluginClient<Bound>, prepare: Prepare) -> Self {
        Self::prepared_with(client, prepare, Vec::new())
    }

    /// As [`prepared`](Self::prepared), with `lanes` (foreign control
    /// lanes, one per parameter) feeding the plugin's event input.
    pub fn prepared_with(
        client: PluginClient<Bound>,
        prepare: Prepare,
        lanes: Vec<tutti_nodes::automation::ControlLaneNode>,
    ) -> Self {
        Self::build(client, prepare, lanes, false).0
    }

    /// As [`prepared_with`](Self::prepared_with), with a keyboard: a
    /// `MidiQueueNode` feeding the plugin's event input too, and its sender.
    #[allow(dead_code)]
    pub fn prepared_with_keys(
        client: PluginClient<Bound>,
        prepare: Prepare,
        lanes: Vec<tutti_nodes::automation::ControlLaneNode>,
    ) -> (Self, tutti_midi_runtime::MidiSender) {
        let (rig, keys) = Self::build(client, prepare, lanes, true);
        (rig, keys.expect("asked for keys"))
    }

    fn build(
        client: PluginClient<Bound>,
        prepare: Prepare,
        lanes: Vec<tutti_nodes::automation::ControlLaneNode>,
        keys: bool,
    ) -> (Self, Option<tutti_midi_runtime::MidiSender>) {
        let (inputs, outputs) = (client.inputs(), client.outputs());
        let layout = |n: usize| ChannelLayout::from_count(u16::try_from(n).expect("a few ports"));
        let mut g = GraphBuilder::new(layout(inputs), layout(outputs));
        let (key, controls) = g.add_with_controls(client);
        g.pipe_input(key).pipe_output(key);
        for lane in lanes {
            let (from, _lane) = g.add_with_controls(lane);
            g.connect_events(from, 0, key, 0);
        }
        let sender = keys.then(|| {
            let (from, sender) = g.add_with_controls(tutti_midi_runtime::MidiQueueNode::new());
            g.connect_events(from, 0, key, 0);
            sender
        });
        let renderer = g.renderer(prepare).expect("a one-plugin graph compiles");
        (
            Self {
                renderer,
                key,
                controls,
                inputs,
                outputs,
            },
            sender,
        )
    }

    /// `client` in a graph prepared with `prepare`, behind a latent path: a
    /// silent source declaring `latency` feeds every plugin input, so the
    /// plugin's compiled arrival latency is `latency`. The graph has no
    /// global inputs.
    ///
    /// # Panics
    ///
    /// If the plugin has no audio input (nothing to put a path in front of).
    pub fn behind(client: PluginClient<Bound>, prepare: Prepare, latency: Latency) -> Self {
        let (inputs, outputs) = (client.inputs(), client.outputs());
        assert!(inputs > 0, "the plugin has an audio input");
        let layout = |n: usize| ChannelLayout::from_count(u16::try_from(n).expect("a few ports"));
        let mut g = GraphBuilder::new(ChannelLayout::EMPTY, layout(outputs));
        let latent = g.add(Unforkable(Latent::new(latency)));
        let (key, controls) = g.add_with_controls(client);
        for port in 0..inputs {
            g.connect(latent, 0, key, port);
        }
        g.pipe_output(key);
        let renderer = g.renderer(prepare).expect("the graph compiles");
        Self {
            renderer,
            key,
            controls,
            inputs: 0,
            outputs,
        }
    }

    /// The graph's global audio inputs: the plugin's, unless it is
    /// [`behind`](Self::behind) a latent path.
    pub fn inputs(&self) -> usize {
        self.inputs
    }

    /// The plugin's audio outputs.
    pub fn outputs(&self) -> usize {
        self.outputs
    }

    /// Render `frames`, input channel `c` frame `i` being `input(c, i)`; one
    /// `Vec` per output channel.
    pub fn run(&mut self, frames: usize, input: impl Fn(usize, usize) -> f32) -> Vec<Vec<f32>> {
        if self.inputs == 0 {
            return self.renderer.render(frames);
        }
        let chans: Vec<Vec<f32>> = (0..self.inputs)
            .map(|c| (0..frames).map(|i| input(c, i)).collect())
            .collect();
        let refs: Vec<&[f32]> = chans.iter().map(Vec::as_slice).collect();
        self.renderer.render_input(&refs)
    }

    /// The node's latency as the graph compiles it: its declared `Shape`.
    pub fn latency(&self) -> Samples {
        self.renderer.editor().spec().topology.nodes[&self.key].latency
    }

    /// The renderer, to set the transport or edit the graph.
    pub fn renderer(&mut self) -> &mut Renderer {
        &mut self.renderer
    }

    /// The plugin's key in the graph.
    pub fn key(&self) -> NodeKey {
        self.key
    }
}

/// The rate the out-of-process suites run at.
pub const SAMPLE_RATE: f64 = 48_000.0;
/// The block the out-of-process suites run.
pub const BLOCK: usize = 64;

/// One block's worth of wall time: the audio callback's deadline.
pub const PERIOD: Duration = Duration::from_nanos((BLOCK as f64 / SAMPLE_RATE * 1e9) as u64);

/// The pace a live driving loop keeps. The pipeline never waits for a reply,
/// so an unpaced loop starves the subprocess and every block reads back
/// silence.
pub const PACE: Duration = PERIOD.saturating_mul(20);

/// Absolute path to the `plugin-server` binary these tests spawn.
///
/// # Panics
///
/// If it has not been built; the message names the command that builds it.
/// `tutti-plugin-server` depends on `tutti-plugin`, so it is not a
/// dev-dependency and running these tests does not build it
/// (`CLAUDE.md`, "Build `plugin-server` first").
pub fn plugin_server_path() -> &'static str {
    static RESOLVED: OnceLock<String> = OnceLock::new();
    RESOLVED
        .get_or_init(|| {
            tutti_fixtures::plugin_server()
                .to_string_lossy()
                .into_owned()
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
    pub const NOTES: u32 = 4;
    pub const TRANSPORT: u32 = 5;
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

    /// Make `activate` refuse this sample rate (Hz), so a host that asks the
    /// plugin to run at it is refused, and any other rate is accepted.
    pub fn refuse_rate(mut self, hz: f64) -> Self {
        self.set("TUTTI_CLAP_PROBE_REFUSE_RATE", hz.to_string());
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

    /// Sleep `stall` at the start of every `process()` call: a live plugin
    /// that answers every block, late. A sleep rather than
    /// [`block_from`](Self::block_from)'s spin, so many stalled instances do
    /// not take the host's cores.
    pub fn stall_every_block(mut self, stall: std::time::Duration) -> Self {
        let us = u32::try_from(stall.as_micros()).expect("a stall under an hour");
        self.set("TUTTI_CLAP_PROBE_STALL_US", us.to_string());
        self
    }

    /// Make the plugin's `clap.state` save refuse.
    pub fn refuse_state_save(mut self, on: bool) -> Self {
        self.set(
            "TUTTI_CLAP_PROBE_REFUSE_STATE_SAVE",
            u8::from(on).to_string(),
        );
        self
    }

    /// Add `frames` to the plugin's latency (and its `Latency`-mode delay)
    /// while it renders offline, telling the host through `clap.latency`.
    pub fn offline_extra_latency(mut self, frames: u32) -> Self {
        self.set("TUTTI_CLAP_PROBE_OFFLINE_EXTRA_LATENCY", frames.to_string());
        self
    }

    /// Make the plugin's `clap.state` load refuse.
    pub fn refuse_state_load(mut self, on: bool) -> Self {
        self.set(
            "TUTTI_CLAP_PROBE_REFUSE_STATE_LOAD",
            u8::from(on).to_string(),
        );
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
/// `TUTTI_PLUGIN_SERVER` rather than a `tutti_plugin_types::Locator`: `PluginClient::new` takes
/// a `BridgeConfig`, which carries no locator field, so the environment is the
/// only lever a test has. It is set here rather than once per suite so the value
/// is visible in the one place that depends on it.
///
/// # Panics
///
/// If the load fails — these tests are about what a *loaded* plugin does, so a
/// failure to load is a broken fixture rather than an outcome worth asserting.
pub fn load_probe(sample_rate: f64) -> LoadedProbe {
    load_probe_with(BridgeConfig::default(), sample_rate)
}

/// [`load_probe`] with a bridge config of the caller's (a shorter
/// `timeout_ms`, say).
pub fn load_probe_with(config: BridgeConfig, sample_rate: f64) -> LoadedProbe {
    // SAFETY: `exclusive()` is held; see `ProbeEnv::set`.
    unsafe { std::env::set_var("TUTTI_PLUGIN_SERVER", plugin_server_path()) };

    let client = PluginClient::new(
        config,
        tutti_fixtures::clap_probe().to_path_buf(),
        SampleRate::new(sample_rate),
    )
    .expect("load the reference CLAP plugin through a real plugin-server");
    let handle = client.handle();
    LoadedProbe { client, handle }
}

/// Whether `pid` still names a process that has not been reaped.
///
/// Unix: `kill(pid, 0)` checks the pid without sending a signal. A running
/// orphan answers `Ok`. So does a zombie, which means a missing `wait` is
/// caught as well as a missing `kill`. A reaped child answers `ESRCH`.
///
/// Windows: there are no zombies. A process object lives exactly as long as
/// some handle to it is open, and the guard's `Child` holds one. After a
/// correct teardown, `OpenProcess` finds nothing. A leaked guard keeps the
/// server running, so its exit code reads `STILL_ACTIVE`.
///
/// Neither probe can rule out the OS reusing a reaped pid for an unrelated
/// process. That error goes the loud way (a false leak, never a false pass),
/// and it needs a pid to wrap round between a drop and this check.
#[cfg(unix)]
pub fn is_alive(pid: u32) -> bool {
    let pid = libc::pid_t::try_from(pid).expect("a pid fits pid_t");
    // SAFETY: signal 0 performs only the existence and permission check.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    // EPERM: the pid exists but belongs to someone else. It cannot be our
    // child, but it is not gone, so report it rather than guess.
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
pub fn is_alive(pid: u32) -> bool {
    use windows::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: plain Win32 calls on a handle this function opens and closes.
    unsafe {
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        let mut code = 0u32;
        let queried = GetExitCodeProcess(process, &mut code);
        let _ = CloseHandle(process);
        queried.is_ok() && code == STILL_ACTIVE.0 as u32
    }
}
