//! The bridge thread — connects to the plugin-server's RT socket and
//! pumps [`Command`]s through [`handle`]. Runs entirely on one OS
//! thread; all IPC calls are blocking.

use super::channels::Channels;
use super::dispatch::handle;
use super::lifecycle::Lifecycle;
use super::messages::{AudioResponse, Command};
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

    let Ok(mut stream) = ipc::connect(&socket_path) else {
        return;
    };
    // Consume the server's Ready handshake for connection 2.
    if ipc::recv(&mut stream).is_err() {
        return;
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

        let result = handle(cmd, stream, channels, payloads);
        drain_unsolicited(channels, listener);

        if result.is_err() {
            lifecycle.mark_crashed();
            // Connection-level: the stream is gone, so this ends every
            // in-flight and queued block, not just one. `None` matches
            // whichever request the audio thread is waiting on.
            channels.push_audio_response(AudioResponse::Error { buffer_id: None });
            drain_with_errors(channels);
            return;
        }
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
        channels.push_audio_response(AudioResponse::Error { buffer_id: None });
    }
}
