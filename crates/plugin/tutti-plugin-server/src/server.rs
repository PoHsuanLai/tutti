//! `PluginServer` — the outer shell. Owns a [`Session`] and drives it
//! over a [`Transport`] across the two-phase connection dance the host
//! expects: first a handshake connection for plugin load / format
//! negotiation / shared-memory setup, then the audio connection that
//! carries per-block traffic for the rest of the session.

use crate::session::{Reaction, Session};
use crate::transport::{Transport, TransportListener};
use tutti_plugin::server::{BridgeConfig, BridgeMessage, PROTOCOL_VERSION};
use tutti_plugin::Result;

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
