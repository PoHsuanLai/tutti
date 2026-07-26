//! `PluginServer` — the outer shell. Owns a [`Session`] and drives it
//! over a [`Transport`] across the two-phase connection dance the host
//! expects: first a handshake connection for plugin load / format
//! negotiation / shared-memory setup, then the audio connection that
//! carries per-block traffic for the rest of the session.

use crate::session::{Reaction, Session};
use crate::transport::{Transport, TransportListener};
use tutti_plugin::server::{BridgeConfig, BridgeMessage, PROTOCOL_VERSION};
use tutti_plugin::Result;

/// Ask the OS to schedule this thread as realtime, now that it is about to do
/// nothing but per-block audio work.
///
/// The host side never needs this: CoreAudio, WASAPI and JACK all create the
/// audio callback's thread at realtime priority themselves. A plugin subprocess
/// gets no such treatment — it is an ordinary `Command::spawn` at default
/// priority — yet it must answer within the same block period. Measured with
/// eight real plugins at 64/48k, that asymmetry swung the number of on-time
/// replies between 29 and 492 out of 500 across identical runs.
///
/// This is a *quality* knob, not a correctness one: a late reply yields silence,
/// never wrong audio, because the host reads a slot only when the slab's
/// sequence number says the server published it (see
/// `tutti_plugin::util::transport::shm::header`).
///
/// Failure is logged and ignored — elevation needs privileges a sandbox or CI
/// runner may not grant, and degraded quality beats refusing to run.
fn raise_to_realtime() {
    match thread_priority::set_current_thread_priority(thread_priority::ThreadPriority::Max) {
        Ok(()) => tracing::debug!("plugin-server audio phase running at realtime priority"),
        Err(e) => tracing::info!(
            "could not raise the audio phase to realtime priority ({e:?}); \
             continuing at normal priority — expect more dropped blocks under load"
        ),
    }
}

pub struct PluginServer {
    config: BridgeConfig,
    session: Session,
}

impl PluginServer {
    pub fn new(config: BridgeConfig) -> Result<Self> {
        Ok(Self {
            config,
            session: Session::new(),
        })
    }

    pub fn run(mut self) -> Result<()> {
        let listener = TransportListener::bind(&self.config.socket_path)?;

        // Phase 1: handshake.
        let mut handshake = listener.accept()?;
        handshake.send(&BridgeMessage::Ready {
            protocol_version: PROTOCOL_VERSION,
        })?;
        if self.handshake_phase(&mut handshake)? {
            return Ok(());
        }
        drop(handshake);

        // Phase 2: audio.
        let mut audio = listener.accept()?;
        audio.send(&BridgeMessage::Ready {
            protocol_version: PROTOCOL_VERSION,
        })?;
        raise_to_realtime();
        self.audio_phase(&mut audio)
    }

    fn handshake_phase(&mut self, transport: &mut dyn Transport) -> Result<bool> {
        loop {
            let msg = match transport.recv() {
                Ok(m) => m,
                Err(_) => return Ok(false),
            };
            match self.session.handle(msg)? {
                Reaction::Shutdown => return Ok(true),
                Reaction::Reply(m) => {
                    if transport.send(&m).is_err() {
                        return Ok(false);
                    }
                }
                Reaction::None => {}
            }
        }
    }

    fn audio_phase(&mut self, transport: &mut dyn Transport) -> Result<()> {
        loop {
            let msg = transport.recv()?;
            match self.session.handle(msg)? {
                Reaction::Shutdown => break,
                Reaction::Reply(m) => transport.send(&m)?,
                Reaction::None => {}
            }
            for event in self.session.drain_async_events() {
                transport.send(&event)?;
            }
        }
        Ok(())
    }
}

impl Drop for PluginServer {
    fn drop(&mut self) {
        self.session.close_editor_on_drop();
        let _ = std::fs::remove_file(&self.config.socket_path);
    }
}
