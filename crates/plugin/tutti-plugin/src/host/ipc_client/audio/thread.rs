//! The bridge thread — connects to the plugin-server's RT socket and
//! pumps [`Command`]s through [`handle`]. Runs entirely on one OS
//! thread; all IPC calls are blocking.

use super::channels::Channels;
use super::dispatch::{handle, Owed};
use super::lifecycle::Lifecycle;
use super::messages::{AudioResponse, BridgeEvent, Command};
use super::payload_pool::PayloadPool;
use super::ListenerSlot;
use crate::util::transport::control::{self as ipc, ControlStream};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

/// Upper bound on how long the bridge thread stays parked with no command. In
/// practice `push_command` unparks it immediately; this is only the backstop
/// that guarantees `lifecycle.is_running()` is re-checked (shutdown) even if an
/// unpark were ever missed.
const IDLE_PARK_TIMEOUT: Duration = Duration::from_millis(1);

pub struct BridgeThread {
    channels: Channels,
    lifecycle: Lifecycle,
    handle: Option<thread::JoinHandle<()>>,
}

impl BridgeThread {
    pub(super) fn spawn(
        channels: Channels,
        payloads: PayloadPool,
        lifecycle: Lifecycle,
        listener: ListenerSlot,
        socket_path: PathBuf,
    ) -> Self {
        let thread_channels = channels.clone();
        let thread_payloads = payloads.clone();
        let thread_lifecycle = lifecycle.clone();
        let handle = thread::Builder::new()
            .name("plugin-bridge".to_string())
            .spawn(move || {
                run_thread(
                    thread_channels,
                    thread_payloads,
                    thread_lifecycle,
                    listener,
                    socket_path,
                )
            })
            .expect("failed to spawn bridge thread");

        Self {
            channels,
            lifecycle,
            handle: Some(handle),
        }
    }

    pub fn shutdown(&mut self) {
        self.lifecycle.request_shutdown();
        self.channels.push_command(Command::Shutdown);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for BridgeThread {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run_thread(
    channels: Channels,
    payloads: PayloadPool,
    lifecycle: Lifecycle,
    listener: ListenerSlot,
    socket_path: PathBuf,
) {
    // Publish our handle before the first poll so `push_command` can unpark us.
    channels.register_worker(thread::current());

    // A bridge that never connects is as dead as one whose socket drops later,
    // and the audio thread tells them apart only through `is_crashed`. Both
    // exits below used to return silently, leaving the flag false forever — so
    // the batcher's crash check could never fire, and the output was silent
    // only because the slab sequence happened never to match. That made correct
    // behaviour a coincidence of the numbering rather than the decision the
    // check exists to make.
    let stream = ipc::connect(&socket_path);
    let mut stream = match stream {
        Ok(s) => s,
        Err(e) => {
            crash(
                &lifecycle,
                &listener,
                format!("could not connect to plugin-server: {e}"),
            );
            return;
        }
    };
    // Consume the server's Ready handshake for connection 2, and check its
    // version rather than discarding it. `launch.rs` already gated the same
    // server on connection 1, so a mismatch here is not reachable today — but
    // this is a wire boundary, the check is one comparison off the audio path,
    // and a silently-ignored version field is how a skew becomes a mis-parse
    // instead of an error.
    match ipc::recv(&mut stream) {
        Ok(crate::protocol::BridgeMessage::Ready { protocol_version })
            if crate::protocol::check_protocol_version(protocol_version).is_ok() => {}
        // The reason is spelled out per case rather than as one string: a
        // version skew and a peer that never said `Ready` are different
        // problems for whoever reads the message, and the first is actionable.
        other => {
            let why = match other {
                Ok(crate::protocol::BridgeMessage::Ready { protocol_version }) => format!(
                    "plugin-server speaks protocol v{protocol_version}, this host speaks v{}",
                    crate::protocol::PROTOCOL_VERSION
                ),
                Ok(_) => "plugin-server did not send Ready as its first message".to_string(),
                Err(e) => format!("plugin-server handshake failed: {e}"),
            };
            crash(&lifecycle, &listener, why);
            return;
        }
    }
    pump(&channels, &payloads, &lifecycle, &listener, &mut stream);
}

fn pump(
    channels: &Channels,
    payloads: &PayloadPool,
    lifecycle: &Lifecycle,
    listener: &ListenerSlot,
    stream: &mut ControlStream,
) {
    // Replies for blocks this thread stopped waiting on. Lives for the length of
    // the connection, because that is the scope over which the stream's pairing
    // has to stay straight; see `dispatch::Owed`.
    let mut owed = Owed::default();
    while lifecycle.is_running() {
        let Some(cmd) = channels.pop_command() else {
            // Park rather than sleep: `push_command` unparks us the instant a
            // block arrives, so the socket round-trip starts immediately
            // instead of after a fixed poll interval. That latency used to sit
            // inside the audio thread's wait budget for the reply.
            //
            // `park_timeout` may also return spuriously — harmless, the loop
            // just re-polls. An unpark racing with this re-poll is likewise
            // safe: `park_timeout` consumes the pending token and returns at
            // once, so no command is ever left sitting in the queue.
            thread::park_timeout(IDLE_PARK_TIMEOUT);
            continue;
        };

        let result = handle(cmd, stream, channels, payloads, &mut owed);
        drain_unsolicited(channels, listener);

        if let Err(e) = result {
            // The error is stringified here, at the only place it exists.
            // `BridgeError` is not `Clone`, and the crash notification outlives
            // this frame, so the alternative is the hardcoded placeholder the
            // host used to report for every death alike.
            crash(lifecycle, listener, format!("{e}"));
            // Connection-level: the stream is gone, so this ends every
            // in-flight and queued block, not just one. `None` matches
            // whichever request the audio thread is waiting on.
            channels.push_audio_response(AudioResponse::Error { seq: None });
            drain_with_errors(channels);
            return;
        }
    }
}

/// Declare the bridge dead: latch the cause, then tell whoever is listening.
///
/// One helper rather than two calls at each site, because the latch and the
/// notification must not drift apart — a crash that fired an event without
/// latching would be invisible to anything that asked later, and one that
/// latched without firing is the polling design this replaces.
///
/// The listener may legitimately be absent. `PluginBridge::new` spawns this
/// thread and `set_listener` runs afterwards, so the connect- and
/// handshake-failure sites usually have no subscriber yet. That is exactly why
/// the latch comes first and is authoritative: `Lifecycle::crash_cause` answers
/// for a crash nobody heard.
fn crash(lifecycle: &Lifecycle, listener: &ListenerSlot, cause: String) {
    lifecycle.mark_crashed(cause.clone());
    let cb = listener.lock().clone();
    if let Some(f) = cb.as_ref() {
        f(BridgeEvent::Crashed { cause });
    }
}

fn drain_unsolicited(channels: &Channels, listener: &ListenerSlot) {
    let cb = listener.lock().clone();
    while let Some(ev) = channels.pop_unsolicited() {
        if let Some(f) = cb.as_ref() {
            f(ev);
        }
    }
}

fn drain_with_errors(channels: &Channels) {
    while channels.pop_command().is_some() {
        channels.push_audio_response(AudioResponse::Error { seq: None });
    }
}
