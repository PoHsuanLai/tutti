//! `PluginServer` — the outer shell. Owns a [`Session`] and drives it
//! over a [`Transport`] across the two-phase connection dance the host
//! expects: first a handshake connection for plugin load / format
//! negotiation / shared-memory setup, then the audio connection that
//! carries per-block traffic for the rest of the session.

use crate::session::{Reaction, Session};
use crate::transport::{Transport, TransportListener};
use tutti_plugin_ipc::{BridgeConfig, BridgeMessage, PROTOCOL_VERSION};
use tutti_plugin_types::MainThread;
use tutti_plugin_types::{BridgeError, BridgeResult};

/// Ask the OS for the highest priority it grants this thread, now that it is
/// about to do nothing but per-block audio work, and log what it got once.
/// That is not real time: on Linux it is nice -20 at best, and a refusal
/// (`EPERM`) is common (`tutti_rt::request_priority`).
///
/// The host side needs this less: CoreAudio, WASAPI and JACK create the
/// audio callback's thread at an elevated priority themselves (cpal's ALSA
/// backend does not: its callback thread is an ordinary one). A plugin subprocess
/// gets no such treatment — it is an ordinary `Command::spawn` at default
/// priority — yet it must answer within the same block period. Measured with
/// eight real plugins at 64/48k, that asymmetry swung the number of on-time
/// replies between 29 and 492 out of 500 across identical runs.
///
/// This is a *quality* knob, not a correctness one: a late reply yields silence,
/// never wrong audio, because the host reads a slot only when the slab's
/// sequence number says the server published it (see
/// `tutti-plugin-ipc`'s `transport::shm::header`).
///
/// Failure is logged and ignored — elevation needs privileges a sandbox or CI
/// runner may not grant, and degraded quality beats refusing to run.
pub(crate) fn request_audio_priority(what: &str) {
    let outcome = tutti_rt::request_priority(tutti_rt::PriorityRequest::Highest);
    if outcome.is_granted() {
        tracing::debug!("plugin-server: {what}: {outcome}");
    } else {
        tracing::info!("plugin-server: {what}: {outcome}; expect more dropped blocks under load");
    }
}

/// The subprocess-side server: one hosted plugin, one host connection.
///
/// Construct with [`PluginServer::new`] and hand control to
/// [`PluginServer::run`], which does not return until the host disconnects or
/// asks for shutdown. Dropping it closes any open editor window and unlinks the
/// socket.
pub struct PluginServer {
    config: BridgeConfig,
    session: Session,
    /// The thread that built the server: the one the plugin's main-thread
    /// calls run on, claimed once at startup.
    main: MainThread,
}

impl std::fmt::Debug for PluginServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginServer")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl PluginServer {
    /// Creates a server for `config`, recording the spawning host's PID.
    ///
    /// The socket is not created until [`PluginServer::run`]. Call this early:
    /// the recorded PID is what orphan detection compares against.
    ///
    /// # Errors
    ///
    /// Currently infallible; the `BridgeResult` is part of the constructor's contract
    /// so setup that can fail may be added without a breaking change.
    pub fn new(config: BridgeConfig) -> BridgeResult<Self> {
        // Before anything can block: the orphan check compares against the PID
        // recorded here, and recording it later would sample a reaper instead
        // of the real spawner. See `transport::record_parent_pid`.
        crate::transport::record_parent_pid();
        let main = MainThread::claim().map_err(|e| BridgeError::ProtocolError(e.to_string()))?;
        Ok(Self {
            config,
            session: Session::new(),
            main,
        })
    }

    /// Serves one host, then returns. Blocks for the lifetime of the session.
    ///
    /// # The two-phase connection dance
    ///
    /// The host connects twice, and the split is what lets the second phase run
    /// at the highest priority the OS grants (not real time) without the
    /// load-time work behind it:
    ///
    /// 1. **Handshake** — plugin load, format negotiation, shared-memory setup.
    ///    Returns early if the host asks for shutdown before any audio flows.
    /// 2. **Audio** — per-block traffic for the rest of the session. The thread
    ///    asks for the highest priority on entry (`request_audio_priority`), and
    ///    async plugin events are
    ///    drained to the host after each block.
    ///
    /// Each phase opens with a [`BridgeMessage::Ready`] carrying
    /// `PROTOCOL_VERSION`; the host refuses a version it does not know.
    ///
    /// # Errors
    ///
    /// Returns an error if the socket cannot be bound, or if the audio phase's
    /// transport fails. A transport failure during the *handshake* is treated as
    /// the host having gone away and returns `Ok(())` — there is no session to
    /// report an error to.
    pub fn run(mut self) -> BridgeResult<()> {
        let listener = TransportListener::bind(&self.config.socket_path)?;

        // First connection: handshake.
        let mut handshake = listener.accept()?;
        handshake.send(&BridgeMessage::Ready {
            protocol_version: PROTOCOL_VERSION,
        })?;
        if self.handshake_phase(&mut handshake)? {
            return Ok(());
        }
        drop(handshake);

        // Second connection: audio.
        let mut audio = listener.accept()?;
        audio.send(&BridgeMessage::Ready {
            protocol_version: PROTOCOL_VERSION,
        })?;
        request_audio_priority("the audio phase");
        self.audio_phase(&mut audio)
    }

    fn handshake_phase(&mut self, transport: &mut dyn Transport) -> BridgeResult<bool> {
        loop {
            let msg = match transport.recv() {
                Ok(m) => m,
                Err(_) => return Ok(false),
            };
            // A load says it is restoring a state before it does (the host's wait
            // changes with it); the transport is the interim sink.
            let reaction = self.session.handle_with(&self.main, msg, &mut |m| {
                let _ = transport.send(&m);
            })?;
            match reaction {
                Reaction::Shutdown => return Ok(true),
                Reaction::Reply(m) => {
                    if transport.send(&m).is_err() {
                        return Ok(false);
                    }
                }
                Reaction::ReplyMany(ms) => {
                    for m in &ms {
                        if transport.send(m).is_err() {
                            return Ok(false);
                        }
                    }
                }
                Reaction::None => {}
            }
        }
    }

    fn audio_phase(&mut self, transport: &mut dyn Transport) -> BridgeResult<()> {
        let main = self.main;
        loop {
            let msg = transport.recv()?;
            match self.session.handle(&main, msg)? {
                Reaction::Shutdown => break,
                Reaction::Reply(m) => transport.send(&m)?,
                Reaction::ReplyMany(ms) => {
                    for m in &ms {
                        transport.send(m)?;
                    }
                }
                Reaction::None => {}
            }
            for event in self.session.drain_async_events(&main) {
                transport.send(&event)?;
            }
        }
        Ok(())
    }
}

impl Drop for PluginServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.config.socket_path);
    }
}
