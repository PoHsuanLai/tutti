//! Full launch sequence for a plugin-server subprocess:
//! spawn → handshake → load plugin → setup shared memory → return
//! a [`LaunchedServer`] ready for the RT bridge thread to connect.

use super::Reaped;
use crate::config::BridgeConfig;
use crate::error::unexpected_message;
use crate::protocol::{
    BridgeMessage, HostMessage, LoadedPlugin, PluginDescriptor, SampleFormat, StagedState,
};
use crate::transport::control::{self as ipc, ControlStream};
use crate::transport::shm::{AudioSlab, SlabLayout, SlabWriter};
use std::path::Path;
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};
use tutti_plugin_types::MAX_CHUNK;
use tutti_plugin_types::{BridgeError, BridgeResult};
use tutti_plugin_types::{Helper, Locator};
use tutti_types::SampleRate;

/// How long to keep retrying the connect while the plugin-server binds.
///
/// This replaced a flat 500 ms `sleep`. The server binds its socket as its
/// first real act — `main` reads one argv entry and calls `PluginServer::run`,
/// which binds before anything else — so the wait was never for setup work, it
/// was padding for process spawn. Paying it unconditionally cost half a second
/// on *every* load, including the ones that were ready in five milliseconds,
/// and a project with twenty plugins spent ten seconds sleeping.
///
/// A retry loop pays only what the spawn actually takes. The bound is generous
/// rather than tight because the failure it guards is a slow machine under
/// load, and the cost of being generous is only paid when the server never
/// arrives at all — which is a failed load either way.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long to wait between connect attempts.
///
/// Short enough that a fast spawn is not rounded up to something a user
/// notices, long enough that a slow one does not spin the CPU. At this interval
/// the common case returns in single-digit milliseconds.
const CONNECT_RETRY_INTERVAL: Duration = Duration::from_millis(2);

/// How long the server may take to restore a staged state once it has said it
/// began ([`BridgeMessage::RestoringState`]). A restore is one opaque plugin call
/// with nothing inside it to measure progress by, so this is one flat, generous
/// window, not the bridge's 5 s: a large state restoring slowly is legal, and a
/// plugin that has not answered in a minute is hung.
pub(crate) const STATE_RESTORE_TIMEOUT: Duration = Duration::from_secs(60);

/// A plugin-server that has loaded its plugin and waits for the bridge
/// thread to connect ([`launch`]).
pub struct LaunchedServer {
    /// The server process, killed and reaped when this drops (doc 027 D11).
    pub process: Reaped,
    /// Catalog identity (name, vendor, class, editor).
    pub descriptor: PluginDescriptor,
    /// Engine-wiring data from instantiation (bus widths, latency, f64).
    pub loaded: LoadedPlugin,
    /// The sample format the blocks travel in.
    pub format: SampleFormat,
    /// The shared-memory slab the server mapped, as the writer of its inputs:
    /// the one in this process (`SlabWriter`). Readers share
    /// [`SlabWriter::slab`].
    pub slab: SlabWriter,
    /// What became of the state the launch was given: `None` when it had none,
    /// else the plugin's refusal as text (`Ok` when it took it).
    pub state_restore: Option<std::result::Result<(), String>>,
}

impl std::fmt::Debug for LaunchedServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LaunchedServer")
            .field("process", &self.process)
            .field("descriptor", &self.descriptor)
            .field("loaded", &self.loaded)
            .field("format", &self.format)
            .field("state_restore", &self.state_restore)
            .finish_non_exhaustive()
    }
}

/// Spawn a plugin-server and bring it to the point where the RT bridge thread
/// can connect.
///
/// **Every failure after the spawn must reap the child.** `process` is a bare
/// [`Child`], and `Child::drop` neither kills nor waits — it explicitly
/// documents that the child keeps running. Ownership only transfers to
/// `ProcessGuard` much later, in `PluginClient::new`, so a bare `?` anywhere in
/// between leaves a `plugin-server` alive with nothing holding it: it survives
/// the host, keeps its socket bound, and holds the plugin's audio device claim.
/// A user retrying a failing plugin a few times accumulates them.
///
/// So the fallible work is done in one closure and the result captured, with
/// teardown unconditional afterwards — the shape `probe::probe_plugin` already
/// uses. Writing it as a sequence of `?` statements is what leaked; making the
/// teardown structural rather than remembered is the point.
///
/// # Errors
///
/// The server cannot be found or spawned, it does not connect or answer the
/// handshake in time, or it fails to load the plugin.
pub fn launch(
    config: &BridgeConfig,
    plugin_path: &Path,
    sample_rate: SampleRate,
    state: Option<&[u8]>,
) -> BridgeResult<LaunchedServer> {
    launch_with_state(
        &Locator::Search,
        config,
        plugin_path,
        sample_rate,
        state,
        STATE_RESTORE_TIMEOUT,
    )
}

/// [`launch`], with the server binary named explicitly rather than searched
/// for.
///
/// Exists so a test can substitute a stand-in server **without touching
/// process-global state**. The alternative — `std::env::set_var` around the
/// call — makes the choice of binary visible to every other thread in the test
/// binary, so under a parallel runner one test's stand-in is another's
/// production lookup. A save/restore guard does not close that window; it *is*
/// the window.
#[cfg(all(test, unix))]
pub(super) fn launch_with(
    locator: &Locator,
    config: &BridgeConfig,
    plugin_path: &Path,
    sample_rate: SampleRate,
) -> BridgeResult<LaunchedServer> {
    launch_with_state(
        locator,
        config,
        plugin_path,
        sample_rate,
        None,
        STATE_RESTORE_TIMEOUT,
    )
}

/// `launch_with`, restoring `state` into the plugin before it is activated:
/// staged ahead of the load (`HostMessage::StageStateChunk`), applied by the
/// server between opening the plugin and activating it, so the activation, and
/// the reply's buses and latency, are the state's.
pub(super) fn launch_with_state(
    locator: &Locator,
    config: &BridgeConfig,
    plugin_path: &Path,
    sample_rate: SampleRate,
    state: Option<&[u8]>,
    restore_timeout: Duration,
) -> BridgeResult<LaunchedServer> {
    let mut process = Reaped::new(spawn_process(locator, config)?);

    // `&mut process` rather than a move: the connect retry polls the child so a
    // server that died on startup is reported at once, and the teardown below
    // still needs to reap it.
    let result = (|process: &mut Child| {
        let mut stream = handshake(config, process)?;
        let shm_name = next_shm_name();
        // A state the server would not take (over the limit, a broken sequence)
        // is a refusal like any other: the plugin loads at its defaults. A stage
        // that got no answer is not: the stream may have a late reply in flight,
        // so the launch ends.
        let staged = match state {
            Some(state) => Some(stage_state(&mut stream, config, state)?),
            None => None,
        };
        let declared = match (&staged, state) {
            (Some(Ok(())), Some(state)) => Some(StagedState {
                len: state.len() as u64,
            }),
            _ => None,
        };
        let (descriptor, loaded, format, loaded_error) = load_plugin(
            &mut stream,
            config,
            plugin_path,
            sample_rate,
            &shm_name,
            declared,
            restore_timeout,
        )?;
        let state_restore = match staged {
            Some(Err(e)) => Some(Err(e)),
            Some(Ok(())) => Some(loaded_error.map_or(Ok(()), Err)),
            None => None,
        };
        let slab = setup_shm(&mut stream, config, &loaded, format, shm_name)?;
        Ok((descriptor, loaded, format, slab, state_restore))
    })(&mut process);

    match result {
        Ok((descriptor, loaded, format, slab, state_restore)) => Ok(LaunchedServer {
            process,
            descriptor: *descriptor,
            loaded,
            format,
            slab,
            state_restore,
        }),
        Err(e) => {
            // `process` drops here, killed and reaped. Best-effort and
            // deliberately un-propagated: the launch error is the one worth
            // reporting, and a failed clean-up must not mask it.
            drop(process);
            let _ = std::fs::remove_file(&config.socket_path);
            Err(e)
        }
    }
}

/// Environment variable through which this process states its own PID to the
/// server it is spawning.
///
/// The server's orphan watchdog needs to know *which* process is its host, and
/// it cannot ask the kernel: a host that dies during the child's startup is
/// reparented before the child's first instruction, so a `getppid()` sample in
/// the server names the reaper rather than the host. Under
/// `PR_SET_CHILD_SUBREAPER` — systemd user services, Docker `--init`, Flatpak,
/// Snap — that reaper is an ordinary live PID and the watchdog goes silent.
///
/// Stating it here fixes the value before the server exists, so no race can
/// disturb it. The constant is duplicated in `tutti_plugin_server::transport`
/// rather than shared: this is the wire between two crates, on the same terms
/// as the socket-path argument beside it.
const HOST_PID_ENV: &str = "TUTTI_PLUGIN_HOST_PID";

fn spawn_process(locator: &Locator, config: &BridgeConfig) -> BridgeResult<Child> {
    let server_path = locator
        .resolve(Helper::Server)
        .ok_or(BridgeError::ServerNotFound)?;
    tracing::debug!("spawning plugin-server: {}", server_path.display());
    Command::new(server_path)
        .arg(&config.socket_path)
        .env(HOST_PID_ENV, std::process::id().to_string())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .map_err(BridgeError::Io)
}

/// Connect as soon as the server is listening, rather than after a fixed wait.
///
/// `connect` fails immediately while nothing is bound, so retrying is how the
/// host learns the socket is up. The path is unique per launch
/// (`unique_socket_path`), so a successful connect can only be *this* server —
/// there is no stale socket from a previous run to attach to by mistake.
///
/// `process` is polled between attempts. A server that died on startup — a
/// missing dynamic library, an immediate panic — never binds, and without this
/// the host would retry for the full timeout before reporting a failure whose
/// cause was known within milliseconds.
fn connect_when_listening(socket: &Path, process: &mut Child) -> BridgeResult<ControlStream> {
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    loop {
        match ipc::connect(socket) {
            Ok(stream) => return Ok(stream),
            Err(e) => {
                if let Ok(Some(status)) = process.try_wait() {
                    return Err(BridgeError::ConnectionFailed(format!(
                        "plugin-server exited before binding its socket ({status})"
                    )));
                }
                if Instant::now() >= deadline {
                    return Err(e);
                }
                thread::sleep(CONNECT_RETRY_INTERVAL);
            }
        }
    }
}

fn handshake(config: &BridgeConfig, process: &mut Child) -> BridgeResult<ControlStream> {
    let mut stream = connect_when_listening(&config.socket_path, process)?;
    let timeout = Duration::from_millis(config.timeout_ms);
    // A server from before version 27 frames its `Ready` in bincode, which does
    // not decode as one message of ours: say which binary is stale rather than
    // report a bare codec error.
    let ready = ipc::recv_within(&mut stream, timeout).map_err(|e| match e {
        BridgeError::Serialization(why) => BridgeError::Serialization(format!(
            "the plugin-server's handshake does not decode ({why}): a server from \
             another build (protocol 26 or older?), rebuild the plugin-server"
        )),
        other => other,
    })?;
    match ready {
        BridgeMessage::Ready { protocol_version } => {
            crate::protocol::check_protocol_version(protocol_version)?;
            Ok(stream)
        }
        ref other => Err(unexpected_message("Ready", other)),
    }
}

fn load_plugin(
    stream: &mut ControlStream,
    config: &BridgeConfig,
    plugin_path: &Path,
    sample_rate: SampleRate,
    shm_name: &str,
    state: Option<StagedState>,
    restore_timeout: Duration,
) -> BridgeResult<(
    Box<PluginDescriptor>,
    LoadedPlugin,
    SampleFormat,
    Option<String>,
)> {
    ipc::send(
        stream,
        &HostMessage::LoadPlugin {
            path: plugin_path.to_path_buf(),
            sample_rate,
            block_size: config.max_buffer_size,
            preferred_format: config.preferred_format,
            shm_name: shm_name.to_string(),
            state,
        },
    )?;

    // The open (library, instance, init) runs under the bridge's timeout. When the
    // server says it has begun restoring the state, the wait changes: a restore
    // is one opaque plugin call with no signal inside it to measure progress by,
    // and a large state is legitimately slow to restore, so it gets a window of
    // its own. A failure from then on is the state's to answer for
    // ([`BridgeError::StateRestoreError`]); one before it is a load failure, and
    // blames nothing.
    let mut timeout = Duration::from_millis(config.timeout_ms);
    let mut restoring = false;
    loop {
        let reply = match ipc::recv_within(stream, timeout) {
            Ok(reply) => reply,
            Err(e) if restoring => {
                return Err(BridgeError::StateRestoreError(format!(
                    "the plugin did not finish restoring its state: {e}"
                )));
            }
            Err(e) => return Err(e),
        };
        match reply {
            BridgeMessage::RestoringState => {
                restoring = true;
                timeout = restore_timeout;
            }
            BridgeMessage::PluginLoaded {
                descriptor,
                loaded,
                negotiated_format,
                state_error,
            } => return Ok((descriptor, loaded, negotiated_format, state_error)),
            BridgeMessage::Error { message } => {
                return Err(BridgeError::load_from_server(plugin_path, message));
            }
            ref other => return Err(unexpected_message("PluginLoaded", other)),
        }
    }
}

/// Hands the server `state` for the plugin its next load opens. The inner `Err`
/// is the server's refusal (too large, a broken sequence): the plugin then loads
/// without it. The outer `Err` is a stage that got no answer, and ends the launch.
fn stage_state(
    stream: &mut ControlStream,
    config: &BridgeConfig,
    state: &[u8],
) -> BridgeResult<std::result::Result<(), String>> {
    if state.len() > crate::protocol::MAX_STATE_BYTES {
        return Ok(Err(format!(
            "state is {} bytes, over the {}-byte limit",
            state.len(),
            crate::protocol::MAX_STATE_BYTES
        )));
    }
    for (seq, last, bytes) in crate::transport::state_chunk::split(state) {
        ipc::send(
            stream,
            &HostMessage::StageStateChunk {
                seq,
                last,
                bytes: bytes.to_vec(),
            },
        )?;
    }
    let timeout = Duration::from_millis(config.timeout_ms);
    match ipc::recv_within(stream, timeout)? {
        BridgeMessage::StateLoaded { error: None } => Ok(Ok(())),
        BridgeMessage::StateLoaded { error: Some(e) } => Ok(Err(e)),
        other => Err(unexpected_message("StateLoaded", &other)),
    }
}

/// The slab shape for a freshly loaded plugin: how many flat channels, how they
/// partition into buses, and how many samples per channel cross the boundary.
///
/// Split out of [`setup_shm`] because it is the whole of the decision and none
/// of the I/O — every input is a plain value, so it is unit-testable, whereas
/// `setup_shm` needs a live subprocess on the other end of `stream`. That
/// matters more than it looks: with the decision fused to the I/O, the aliasing
/// choice below can only be checked by launching a real plugin.
fn slab_layout_for(
    loaded: &LoadedPlugin,
    format: SampleFormat,
    max_buffer_size: usize,
) -> SlabLayout {
    // Sized to the largest chunk the node ships (`MAX_CHUNK`: one device
    // callback, see the batcher), NOT to `config.max_buffer_size`. The two
    // are different quantities: `max_buffer_size` (8192 by default) is
    // what the *plugin* is told to size its own buffers for on
    // `LoadPlugin`, whereas this is what crosses the shared region per
    // chunk. `min` rather than a bare `MAX_CHUNK` so a host that
    // deliberately configures a *smaller* buffer still gets a slab it
    // cannot overrun (the node's chunk is capped at the same figure).
    // The per-bus widths are carried so both processes agree how each
    // direction's channels partition into buses.
    SlabLayout::new(
        max_buffer_size.min(MAX_CHUNK),
        format,
        loaded.input_widths(),
        loaded.output_widths(),
    )
}

fn setup_shm(
    stream: &mut ControlStream,
    config: &BridgeConfig,
    loaded: &LoadedPlugin,
    format: SampleFormat,
    shm_name: String,
) -> BridgeResult<SlabWriter> {
    let layout = slab_layout_for(loaded, format, config.max_buffer_size);
    let slab = AudioSlab::create(shm_name.clone(), layout.clone())?;

    ipc::send(stream, &HostMessage::SetupSharedMemory { shm_name, layout })?;

    let timeout = Duration::from_millis(config.timeout_ms);
    match ipc::recv_within(stream, timeout)? {
        BridgeMessage::SharedMemoryReady => Ok(slab),
        ref other => Err(unexpected_message("SharedMemoryReady", other)),
    }
}

fn next_shm_name() -> String {
    // Unguessable for the same reason as the socket (`unique_socket_path`):
    // `/dev/shm` is shared, and `AudioSlab::create` refuses a name that
    // exists, so a predictable one is a launch another user can block.
    crate::config::unique_name("tutti_plugin")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ChannelLayout;
    use crate::transport::shm::RING_SLOTS;

    /// A failed launch must not leave the subprocess running.
    ///
    /// `process` is a bare [`Child`], whose `Drop` explicitly does *not* kill or
    /// reap — so any `?` between the spawn and `ProcessGuard`'s construction
    /// (which happens much later, in `PluginClient::new`) would strand a
    /// `plugin-server` with nothing holding it.
    /// It outlives the host, keeps its socket bound, and holds the plugin's
    /// device claim; a user retrying a failing plugin accumulates them.
    ///
    /// The stand-in has to satisfy three requirements at once: never speak the
    /// protocol, so `handshake` fails the way a hung server does; **stay alive
    /// while doing so**, or there is no leak left to detect; and leave the
    /// socket path alone, so the failure under test is the ordinary
    /// connect/handshake one.
    ///
    /// The middle requirement is easy to get wrong, because `spawn_process`
    /// hands the socket path to the stand-in as argv[1]. `sleep` reads it as a
    /// duration, rejects it, and exits. `cat` reads it as a filename and exits
    /// too, since nothing has created it. Both looked right and both made this
    /// test vacuous — caught only by restoring the leak and watching it still
    /// pass. Creating the path as a FIFO keeps `cat` alive but breaks the third
    /// requirement: `connect` then fails with ENOTSOCK long before the
    /// handshake, testing a different path than the one that matters.
    ///
    /// So the stand-in is a tiny shell script that ignores its arguments and
    /// sleeps. It survives, it never speaks, and it never touches the socket.
    ///
    /// # Keeping it deterministic under parallel load
    ///
    /// **The stand-in is chosen with a [`Locator`]** passed to
    /// `launch_with`, not through `TUTTI_PLUGIN_SERVER`: an env var is
    /// process-global, so every other thread in this binary would see it.
    ///
    /// **Liveness is not checked with a process-table scan** (`pgrep`): the
    /// stand-in's command line is identical in every test process, so a sibling
    /// test binary's healthy stand-in would count as this test's leak.
    ///
    /// Instead the stand-in writes **its own pid** to a path unique to this
    /// test, so the process observed is provably the one `launch` spawned and
    /// no scan is involved. `kill(pid, 0)` then asks
    /// about that process without signalling it, which distinguishes the two
    /// outcomes that matter: a still-running orphan answers `Ok`, while a
    /// killed-and-reaped child answers `ESRCH`. A zombie also answers `Ok`, so
    /// this catches a missing `wait` as well as a missing `kill`.
    ///
    /// **No wall-clock budget.** `launch` kills and waits *synchronously*
    /// before returning, so by the time it has returned the child is already
    /// reaped — there is nothing to wait for, and a retry loop would hide that
    /// certainty behind a deadline a loaded machine could exhaust. The pid is
    /// read once, and asserted once.
    #[test]
    #[cfg(unix)]
    fn a_failed_launch_does_not_strand_the_subprocess() {
        let dir = tempfile::tempdir().expect("a private temp dir");
        let pid_file = dir.path().join("standin.pid");
        let stand_in = dir.path().join("standin.sh");

        // `$$` is the script's own pid, and `exec` replaces that same process
        // image with `sleep` — so the recorded pid is the process that is still
        // alive afterwards, not a parent that has already exited.
        std::fs::write(
            &stand_in,
            format!(
                "#!/bin/sh\necho $$ > {}\nexec sleep 120\n",
                pid_file.display()
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stand_in, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let config = BridgeConfig {
            socket_path: crate::unique_socket_path(),
            timeout_ms: 200,
            ..Default::default()
        };

        let err = match launch_with(
            &Locator::at(&stand_in),
            &config,
            Path::new("/nonexistent.vst3"),
            SampleRate(48_000.0),
        ) {
            Err(e) => e,
            // Cannot happen (`sleep` never sends `Ready`), but reap rather than
            // leak if the premise ever changes.
            Ok(mut server) => {
                let _ = server.process.kill();
                let _ = server.process.wait();
                panic!("a stand-in server that never speaks the protocol completed a launch");
            }
        };

        // The stand-in records its pid before `exec`, and `launch` cannot return
        // until it has connected-or-timed-out against a server it spawned — so
        // an absent file means the stand-in was never run, which would make
        // every assertion below vacuous.
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap_or_else(|e| {
                panic!(
                    "the stand-in server never recorded its pid ({e}) — it was not \
                     spawned, so this test would prove nothing about cleanup. \
                     launch returned: {err}"
                )
            })
            .trim()
            .parse()
            .expect("the stand-in writes a bare pid");

        // No retry loop: `launch` kills AND waits before returning, so the child
        // is reaped by the time control is back here. A budget would only hide a
        // missing `wait` behind a deadline.
        assert!(
            !is_alive(pid),
            "a failed launch ({err}) left stand-in server pid {pid} running — a \
             bare `?` after the spawn strands the subprocess, because \
             `Child::drop` neither kills nor reaps"
        );
    }

    /// A launch that restores a staged state, against the reference plugin, with
    /// its switches set in this process's environment (the server inherits it;
    /// nextest runs each test in a process of its own).
    #[cfg(unix)]
    fn stateful_launch(
        env: &[(&str, &str)],
        timeout_ms: u64,
        restore_timeout: Duration,
    ) -> (BridgeResult<LaunchedServer>, Duration) {
        // Held for the launch: the server inherits the switches when it spawns.
        let mut switches = tutti_fixtures::ScopedEnv::new();
        for (k, v) in env {
            switches.set(k, v);
        }
        let config = BridgeConfig {
            socket_path: crate::unique_socket_path(),
            timeout_ms,
            ..Default::default()
        };
        let plugin = tutti_fixtures::clap_probe();
        let started = Instant::now();
        let launched = launch_with_state(
            &Locator::at(tutti_fixtures::plugin_server()),
            &config,
            plugin,
            SampleRate(48_000.0),
            Some(&[0u8; 16]),
            restore_timeout,
        );
        (launched, started.elapsed())
    }

    #[cfg(unix)]
    fn reap(server: BridgeResult<LaunchedServer>) {
        if let Ok(mut server) = server {
            let _ = server.process.kill();
            let _ = server.process.wait();
        }
    }

    /// **A slow restore that succeeds is a load.** The plugin takes 2.5 s over
    /// its state, longer than the bridge's 1 s timeout, and the launch waits for
    /// it: the server said it began restoring, and a restore has the window of its
    /// own.
    ///
    /// Needs `cargo build -p tutti-plugin-server` first.
    #[test]
    #[cfg(unix)]
    fn a_slow_restore_that_succeeds_is_a_load() {
        let (launched, took) = stateful_launch(
            &[("TUTTI_CLAP_PROBE_STATE_LOAD_SLEEP_MS", "2500")],
            1_000,
            Duration::from_secs(20),
        );
        assert!(took >= Duration::from_millis(2_500), "{took:?}");
        let server = launched.expect("a slow restore is not a failure");
        assert!(server.state_restore.is_some(), "a restore was attempted");
        reap(Ok(server));
    }

    /// **A restore that never returns is the state's to answer for**: past its
    /// window the launch fails as `StateRestoreError`.
    #[test]
    #[cfg(unix)]
    fn a_restore_that_outlasts_its_window_is_blamed_on_the_state() {
        let (launched, _) = stateful_launch(
            &[("TUTTI_CLAP_PROBE_STATE_LOAD_SLEEP_MS", "6000")],
            1_000,
            Duration::from_millis(1_500),
        );
        assert!(
            matches!(&launched, Err(BridgeError::StateRestoreError(_))),
            "{:?}",
            launched.as_ref().err()
        );
    }

    /// **A plugin that dies in its state restore is blamed on the state.**
    #[test]
    #[cfg(unix)]
    fn a_crash_in_the_restore_is_blamed_on_the_state() {
        let (launched, _) = stateful_launch(
            &[("TUTTI_CLAP_PROBE_CRASH_ON_STATE_LOAD", "1")],
            2_000,
            Duration::from_secs(20),
        );
        assert!(
            matches!(&launched, Err(BridgeError::StateRestoreError(_))),
            "{:?}",
            launched.as_ref().err()
        );
    }

    /// **A plugin that dies before the restore is not blamed on the state**: it
    /// dies initializing, which comes first, and the launch fails as any load does.
    #[test]
    #[cfg(unix)]
    fn a_crash_before_the_restore_is_not_blamed_on_the_state() {
        let (launched, _) = stateful_launch(
            &[("TUTTI_CLAP_PROBE_CRASH_ON_INIT", "1")],
            2_000,
            Duration::from_secs(20),
        );
        assert!(
            launched.is_err() && !matches!(&launched, Err(BridgeError::StateRestoreError(_))),
            "{:?}",
            launched.as_ref().err()
        );
    }

    /// A library that is not a plugin fails the launch with the loader's own
    /// reason. The server answers the load; it does not exit, which the host
    /// would report as `ProcessCrashed`.
    ///
    /// The reference CLAP plugin's cdylib, copied under a `.so` name, is a real
    /// library the server reads as VST2 and the VST2 loader refuses. The copy is
    /// what makes it VST2 on every unix: the cdylib is `.so` on Linux but
    /// `.dylib` on macOS, an extension the server refuses before any loader runs
    /// (`Unsupported plugin format`), which is the failure this test excludes.
    ///
    /// Needs `cargo build -p tutti-plugin-server` first (`CLAUDE.md`).
    #[test]
    #[cfg(unix)]
    fn a_library_that_is_not_a_plugin_fails_the_launch_with_the_loaders_reason() {
        let dir = tempfile::tempdir().expect("a private temp dir");
        let config = BridgeConfig {
            socket_path: crate::unique_socket_path(),
            ..Default::default()
        };
        // A copy in this test's own directory, not `publish_with_extension`:
        // that names the link `real.with_extension(..)`, which on Linux is the
        // cdylib's own path.
        let not_a_plugin = dir.path().join("not_a_plugin.so");
        std::fs::copy(
            tutti_fixtures::cdylib("tutti_clap_test_plugin", "tutti-clap-test-plugin"),
            &not_a_plugin,
        )
        .expect("copy the cdylib under a VST2 name");

        let err = match launch_with(
            &Locator::at(tutti_fixtures::plugin_server()),
            &config,
            &not_a_plugin,
            SampleRate(48_000.0),
        ) {
            Err(e) => e,
            Ok(mut server) => {
                let _ = server.process.kill();
                let _ = server.process.wait();
                panic!("a library with no plugin entry point launched");
            }
        };
        match err {
            BridgeError::LoadFailed { reason, .. } => {
                assert!(
                    !reason.contains("Unsupported plugin format"),
                    "reason: {reason}"
                );
                #[cfg(target_os = "linux")]
                assert!(reason.contains("NotAPlugin"), "reason: {reason}");
            }
            other => panic!("expected LoadFailed, got {other:?}"),
        }
    }

    /// Whether process `pid` still exists, asked without signalling it.
    ///
    /// `kill -0` is the POSIX existence probe: it performs permission checks and
    /// reports the result, but delivers no signal. Exit status 0 means the
    /// process is there (a zombie included, which is what makes a *missing*
    /// `wait` detectable), non-zero means ESRCH.
    ///
    /// Shelling out rather than calling `libc::kill` only because this crate has
    /// no `libc` dependency and one test does not earn it. The property that
    /// matters is unchanged either way: this asks about **one known pid**, so
    /// unlike the `pgrep` scan it replaced it cannot see any other test's
    /// processes.
    #[cfg(unix)]
    fn is_alive(pid: i32) -> bool {
        std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn loaded(inputs: &[ChannelLayout], outputs: &[ChannelLayout]) -> LoadedPlugin {
        let buses = |widths: &[ChannelLayout]| {
            widths
                .iter()
                .map(|&w| tutti_plugin_types::PluginBus::main(w))
                .collect()
        };
        LoadedPlugin {
            inputs: buses(inputs),
            outputs: buses(outputs),
            ..Default::default()
        }
    }

    /// The default `BridgeConfig::max_buffer_size`. Named here so the sizing
    /// tests below read as "the shipped default", not as a magic number.
    const DEFAULT_MAX_BUFFER: usize = 8192;

    /// A plain stereo-in/stereo-out plugin with one bus per direction is the
    /// *common* case, and the one most easily collapsed onto a single shared
    /// region: at `output_base == 0` the host's own input write lands where it
    /// later reads the plugin's output. The two directions must be separately
    /// sized and separately addressed.
    #[test]
    fn stereo_one_bus_each_direction_gets_disjoint_regions() {
        let layout = slab_layout_for(
            &loaded(&[ChannelLayout::STEREO], &[ChannelLayout::STEREO]),
            SampleFormat::Float32,
            DEFAULT_MAX_BUFFER,
        );

        assert_eq!(layout.input_channels(), 2);
        assert_eq!(layout.output_channels(), 2);
        assert_eq!(layout.input_ring_bytes(), layout.output_ring_bytes());
        // Both directions are counted. Half this total would mean the two
        // directions share one region, which is the aliasing.
        assert_eq!(
            layout.checked_byte_size_with_header(0),
            Some(layout.input_ring_bytes() * 2)
        );
    }

    /// A sidechain widens only the input direction.
    #[test]
    fn sidechain_input_widens_only_its_own_ring() {
        let layout = slab_layout_for(
            &loaded(
                &[ChannelLayout::STEREO, ChannelLayout::MONO],
                &[ChannelLayout::STEREO],
            ),
            SampleFormat::Float32,
            DEFAULT_MAX_BUFFER,
        );

        assert_eq!(layout.input_channels(), 3);
        assert_eq!(layout.output_channels(), 2);
        assert!(layout.input_ring_bytes() > layout.output_ring_bytes());
    }

    /// A direction with no buses (an instrument's inputs) is one bus of no
    /// channels at the slab, which refuses an empty list: it occupies no
    /// bytes, and the output ring is sized as before.
    #[test]
    fn a_direction_with_no_buses_is_one_bus_of_no_channels() {
        let layout = slab_layout_for(
            &loaded(&[], &[ChannelLayout::STEREO]),
            SampleFormat::Float32,
            DEFAULT_MAX_BUFFER,
        );
        assert_eq!(layout.inputs, vec![ChannelLayout::EMPTY]);
        assert_eq!(layout.input_ring_bytes(), 0);
        assert_eq!(layout.output_channels(), 2);
    }

    /// Every layout this function produces must be one the slab will accept.
    /// Cheaper to assert here than to discover at plugin-load time on a user's
    /// machine.
    #[test]
    fn every_produced_layout_is_addressable() {
        let cases = [
            loaded(&[], &[]),
            loaded(&[ChannelLayout::STEREO], &[ChannelLayout::STEREO]),
            loaded(&[ChannelLayout::MONO], &[ChannelLayout::STEREO]),
            loaded(
                &[ChannelLayout::STEREO, ChannelLayout::MONO],
                &[ChannelLayout::STEREO],
            ),
        ];
        for (i, l) in cases.iter().enumerate() {
            let layout = slab_layout_for(l, SampleFormat::Float32, DEFAULT_MAX_BUFFER);
            assert!(!layout.inputs.is_empty(), "case {i}: empty input buses");
            assert!(!layout.outputs.is_empty(), "case {i}: empty output buses");
            assert_eq!(layout.slots as usize, RING_SLOTS, "case {i}");
        }
    }

    /// The slab is sized to the largest chunk the node ships — one device
    /// callback, at most `MAX_CHUNK` — not to `max_buffer_size`, which is the
    /// plugin's own buffer bound. A live plugin gets a whole device callback
    /// per chunk, and the slab holds the largest one.
    #[test]
    fn slab_is_sized_to_the_largest_chunk_not_the_configured_maximum() {
        let l = loaded(&[ChannelLayout::STEREO], &[ChannelLayout::STEREO]);
        let layout = slab_layout_for(&l, SampleFormat::Float32, DEFAULT_MAX_BUFFER);
        assert_eq!(layout.samples_per_channel, MAX_CHUNK);
        const { assert!(MAX_CHUNK < DEFAULT_MAX_BUFFER) };
    }

    /// A host configured *below* the chunk ceiling gets a slab it cannot overrun —
    /// the reason this is a `min` and not a bare `MAX_CHUNK`.
    #[test]
    fn a_smaller_configured_buffer_wins() {
        let l = loaded(&[ChannelLayout::STEREO], &[ChannelLayout::STEREO]);
        let layout = slab_layout_for(&l, SampleFormat::Float32, 32);
        assert_eq!(layout.samples_per_channel, 32);
    }

    /// f64 negotiation doubles the region; the channel/bus decisions are
    /// unaffected by sample format.
    #[test]
    fn f64_doubles_the_byte_size_only() {
        let l = loaded(&[ChannelLayout::STEREO], &[ChannelLayout::STEREO]);
        let f32_layout = slab_layout_for(&l, SampleFormat::Float32, DEFAULT_MAX_BUFFER);
        let f64_layout = slab_layout_for(&l, SampleFormat::Float64, DEFAULT_MAX_BUFFER);

        assert_eq!(f64_layout.inputs, f32_layout.inputs);
        assert_eq!(
            f64_layout.samples_per_channel,
            f32_layout.samples_per_channel
        );
        assert_eq!(
            f64_layout.checked_byte_size_with_header(0),
            f32_layout.checked_byte_size_with_header(0).map(|n| n * 2)
        );
    }

    /// Drives the real [`handshake`] against a stand-in server whose first frame
    /// is `body`, and returns the error it must end in.
    fn handshake_against(body: Vec<u8>) -> BridgeError {
        use interprocess::local_socket::{ListenerOptions, traits::Listener as _};

        // Unique per run. A fixed name collides with a leftover socket from an
        // earlier run, and `connect` then fails with EINVAL *before* the version
        // is ever compared — an IO error that would masquerade as a working gate.
        let path = crate::unique_socket_path();
        let _ = std::fs::remove_file(&path);
        let name = crate::transport::control::socket_name(&path).unwrap();
        let listener = ListenerOptions::new().name(name).create_sync().unwrap();

        let server = std::thread::spawn(move || {
            use std::io::Write;
            let Ok(stream) = listener.accept() else {
                return;
            };
            let mut stream = &stream;
            let _ = stream.write_all(&(body.len() as u32).to_be_bytes());
            let _ = stream.write_all(&body);
        });

        let config = BridgeConfig {
            socket_path: path.clone(),
            timeout_ms: 2_000,
            ..Default::default()
        };
        // The listener above stands in for the server, so there is no child to
        // poll — but `handshake` takes one to notice a spawn that died. A
        // lingering stand-in stays alive for the whole test; it is killed below.
        let mut stand_in = spawn_stand_in(StandIn::Lingers);
        let err = handshake(&config, &mut stand_in)
            .expect_err("a stale server must not complete the handshake");
        let _ = stand_in.kill();
        let _ = stand_in.wait();
        let _ = server.join();
        let _ = std::fs::remove_file(&path);
        err
    }

    /// A leftover v3 `plugin-server` on `PATH` must be refused at the handshake
    /// with a named error, not crash and not proceed.
    ///
    /// This matters because the wire is not self-describing. A v3 server that got
    /// as far as `SetupSharedMemory` would read our `slots` field out of the
    /// bytes where its own build expects `channels`, get a plausible small
    /// integer, and map a wrong-sized region — in silence. The version gate is
    /// what stops that, so it has to fire before any payload is sent, and the
    /// failure has to name both versions or the operator has no idea which
    /// binary is stale.
    ///
    /// Drives the real [`handshake`] against a socket that speaks v3, rather
    /// than unit-testing `check_protocol_version` in isolation — the risk being
    /// covered is a handshake path that forgets to call the gate at all.
    #[test]
    fn a_stale_v3_server_is_refused_with_a_named_version_error() {
        use crate::protocol::PROTOCOL_VERSION;

        const STALE_VERSION: u32 = 3;
        assert_ne!(
            STALE_VERSION, PROTOCOL_VERSION,
            "this test is meaningless once the current version reaches 3"
        );

        let err = handshake_against(
            postcard::to_allocvec(&BridgeMessage::Ready {
                protocol_version: STALE_VERSION,
            })
            .unwrap(),
        );
        match err {
            BridgeError::ProtocolMismatch { expected, got } => {
                assert_eq!(expected, PROTOCOL_VERSION);
                assert_eq!(got, STALE_VERSION);
            }
            ref other => panic!("expected a ProtocolMismatch naming both versions, got: {other}"),
        }

        // The operator reads this line, so it must carry both numbers.
        let text = format!("{err}");
        assert!(
            text.contains(&STALE_VERSION.to_string())
                && text.contains(&PROTOCOL_VERSION.to_string()),
            "the message must name both versions, got: {text}"
        );
    }

    /// **A server from the bincode wire (protocol 26 and older) is refused at
    /// the handshake, and the error says to rebuild it.** Its `Ready` is a
    /// four-byte little-endian variant tag then the version as a four-byte
    /// little-endian `u32`, written out here from bincode 1's layout; under
    /// postcard those bytes do not decode as one message, so the version is
    /// never read and the codec error is what the operator would otherwise see.
    #[test]
    fn a_bincode_era_server_is_refused_with_a_rebuild_hint() {
        let ready = postcard::to_allocvec(&BridgeMessage::Ready {
            protocol_version: 0,
        })
        .unwrap()[0];
        let mut bincode_ready = vec![ready, 0, 0, 0];
        bincode_ready.extend_from_slice(&26u32.to_le_bytes());

        match handshake_against(bincode_ready) {
            BridgeError::Serialization(m) => {
                assert!(m.contains("rebuild the plugin-server"), "{m}");
            }
            other => panic!("expected a Serialization error naming a rebuild, got: {other}"),
        }
    }

    /// The connect waits for a socket that is not bound yet, instead of failing.
    ///
    /// This is the property that replaced the flat 500 ms sleep. The listener is
    /// deliberately created *after* `connect_when_listening` is already
    /// retrying, so a single attempt — which is what the code did before the
    /// sleep was introduced — cannot pass: the first `connect` is guaranteed to
    /// find nothing bound.
    #[test]
    fn the_connect_waits_for_a_socket_that_is_not_bound_yet() {
        use interprocess::local_socket::{ListenerOptions, prelude::*};

        let path = std::env::temp_dir().join(format!("tutti_late_bind_{}", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let listen_path = path.clone();
        let server = std::thread::spawn(move || {
            // Long enough that the first few connect attempts must fail.
            std::thread::sleep(Duration::from_millis(60));
            let name = crate::transport::control::socket_name(&listen_path).expect("socket name");
            let listener = ListenerOptions::new()
                .name(name)
                .create_sync()
                .expect("bind the late listener");
            // Hold the connection open briefly so the host's connect succeeds.
            let _ = listener.accept();
            std::thread::sleep(Duration::from_millis(50));
        });

        let mut stand_in = spawn_stand_in(StandIn::Lingers);
        let result = connect_when_listening(&path, &mut stand_in);
        let _ = stand_in.kill();
        let _ = stand_in.wait();

        assert!(
            result.is_ok(),
            "the connect must retry until the server binds, got: {:?}",
            result.err()
        );

        let _ = server.join();
        let _ = std::fs::remove_file(&path);
    }

    /// A server that dies before binding is reported at once, not after the
    /// full timeout.
    ///
    /// Without the `try_wait` poll this case is indistinguishable from a slow
    /// spawn, so the host would retry for the whole `CONNECT_TIMEOUT` before
    /// reporting a failure whose cause was known in milliseconds. The elapsed
    /// assertion is the point — a test that only checked for `Err` would pass
    /// on the slow path too.
    #[test]
    fn a_server_that_never_binds_is_reported_before_the_timeout() {
        let path = std::env::temp_dir().join(format!("tutti_never_bound_{}", std::process::id()));
        let _ = std::fs::remove_file(&path);

        // Exits immediately and binds nothing — the shape of a plugin-server
        // that dies on a missing dynamic library.
        let mut dead = spawn_stand_in(StandIn::Exits);

        let started = Instant::now();
        let err = connect_when_listening(&path, &mut dead)
            .expect_err("a server that never binds cannot connect");
        let elapsed = started.elapsed();

        assert!(
            elapsed < CONNECT_TIMEOUT / 2,
            "a dead server must be noticed promptly, took {elapsed:?} of {CONNECT_TIMEOUT:?}"
        );
        assert!(
            format!("{err}").contains("exited before binding"),
            "the error must say the server died rather than blaming the socket, got: {err}"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// Which throwaway child a connect test needs in place of a plugin-server.
    enum StandIn {
        /// Exits at once, binding nothing.
        Exits,
        /// Stays alive, binding nothing, until the test kills it.
        Lingers,
    }

    /// Set only in a [`StandIn::Lingers`] child, so that [`stand_in_lingers`]
    /// sleeps there and is an instant no-op in an ordinary test run.
    const STAND_IN_ENV: &str = "TUTTI_LAUNCH_TEST_STAND_IN";

    /// Spawns a stand-in by re-running **this test binary**, never a program
    /// looked up on `PATH`.
    ///
    /// These were `sleep 30` and `true`. Neither is a Windows program: there
    /// they resolve to whatever `PATH` offers (Git's MSYS `usr/bin` on CI) or to
    /// nothing. An MSYS `true` can take seconds to start and exit under load,
    /// and the never-binds test measures exactly that interval — on Windows CI
    /// it took 5.7 s against its 2.5 s budget and failed, having tested the
    /// runner's `PATH` rather than `connect_when_listening`. The test binary
    /// exists on every target by construction, and its harness starts and
    /// exits in milliseconds.
    ///
    /// - `Exits` asks the harness for an `--exact` name no test has: libtest
    ///   filters everything out and exits 0 without running anything.
    /// - `Lingers` runs [`stand_in_lingers`] alone, with [`STAND_IN_ENV`] set so
    ///   it sleeps. If that name ever stops matching, the child exits at once
    ///   instead and `the_connect_waits_for_a_socket_that_is_not_bound_yet`
    ///   fails with "exited before binding" — the drift cannot pass silently.
    fn spawn_stand_in(kind: StandIn) -> Child {
        let exe = std::env::current_exe().expect("the test binary's own path");
        let mut cmd = Command::new(exe);
        match kind {
            StandIn::Exits => {
                cmd.args(["--exact", "no test in this binary has this name"]);
            }
            StandIn::Lingers => {
                // libtest names a unit test by its path *within* the crate, so
                // drop the leading `tutti_plugin_ipc::` from `module_path!()`.
                let module = module_path!()
                    .split_once("::")
                    .map_or(module_path!(), |(_, rest)| rest);
                cmd.args(["--exact", &format!("{module}::stand_in_lingers")])
                    .env(STAND_IN_ENV, "1");
            }
        }
        cmd.stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("failed to re-run the test binary as a stand-in child")
    }

    /// The body of a [`StandIn::Lingers`] child; a no-op anywhere else.
    ///
    /// 30 s is only a ceiling — every caller kills its stand-in as soon as it is
    /// done — so a leaked one still goes away by itself.
    #[test]
    fn stand_in_lingers() {
        if std::env::var_os(STAND_IN_ENV).is_some() {
            thread::sleep(Duration::from_secs(30));
        }
    }
}
