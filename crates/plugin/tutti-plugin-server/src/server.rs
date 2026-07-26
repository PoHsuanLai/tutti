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
/// # Why the subprocess has to ask and the host does not
///
/// On the host side the audio callback runs on a thread the OS audio backend
/// created — CoreAudio, WASAPI and JACK all give that thread realtime priority
/// themselves, because it is driving a live device. A plugin subprocess gets no
/// such treatment: it is an ordinary `Command::spawn`ed process at default
/// priority, competing with every background task on the machine, and yet it
/// must answer within the same block period.
///
/// That asymmetry is measurable. Driving eight real plugins at 64 frames /
/// 48 kHz from an unprivileged harness, the number of blocks whose reply
/// arrived in time swung between 29 and 492 out of 500 across identical runs —
/// entirely at the scheduler's discretion.
///
/// # Why a late reply is not a correctness problem
///
/// It is a *quality* problem. A block whose reply misses its window produces
/// silence, never wrong audio: the host reads a slot only when the slab's
/// sequence number says the server published it (see
/// `tutti_plugin::util::transport::shm::header`). So this call raises the
/// proportion of blocks that carry audio; it is not load-bearing for the
/// pipeline being sound.
///
/// # Why failure is ignored
///
/// Elevation needs privileges that are not always available — a hardened
/// sandbox, a container without `CAP_SYS_NICE`, an unprivileged CI runner.
/// Refusing to run there would turn a degraded-quality situation into a
/// non-functional one. `ThreadPriority::Max` matches
/// `tutti-sampler`'s disk butler, the engine's other thread with a deadline.
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
