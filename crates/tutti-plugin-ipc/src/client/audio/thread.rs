//! The bridge thread — connects to the plugin-server's RT socket and
//! pumps [`Command`]s through [`handle`]. Runs entirely on one OS
//! thread; all IPC calls are blocking.

use super::ListenerSlot;
use super::channels::Channels;
use super::dispatch::{Lane, Lanes, Owed, handle};
use super::lifecycle::Lifecycle;
use super::messages::{AudioResponse, BridgeEvent, Command, Unsolicited};
use super::payload_pool::PayloadPool;
use crate::transport::control::{self as ipc, ControlStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use tutti_plugin_types::{BridgeError, PluginFailure};

/// The bridge thread of an [`AudioBridge`](super::AudioBridge): it speaks to
/// the server for the queues. Dropping it shuts it down and joins it.
pub struct BridgeThread {
    channels: Channels,
    lifecycle: Lifecycle,
    handle: Option<thread::JoinHandle<()>>,
}

impl std::fmt::Debug for BridgeThread {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BridgeThread")
            .field("running", &self.handle.is_some())
            .finish_non_exhaustive()
    }
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

    /// Asks the thread to stop and joins it. Idempotent.
    pub fn shutdown(&mut self) {
        self.lifecycle.request_shutdown();
        self.channels.push_command(Command::Shutdown);
        // The queue may have been full, so the push may have rung nothing.
        self.channels.wake();
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
    // A bridge that never connects is as dead as one whose socket drops later,
    // and the audio thread tells them apart only through `is_crashed`. Both
    // exits below must mark the crash: returning silently would leave the flag
    // false forever, so the batcher's crash check could never fire, and the
    // output would be silent only because the slab sequence happened never to
    // match.
    let stream = ipc::connect(&socket_path);
    let mut stream = match stream {
        Ok(s) => s,
        Err(e) => {
            crash(
                &lifecycle,
                &listener,
                PluginFailure::Connect {
                    source: Arc::new(e),
                },
            );
            // A block queued before the crash was marked is never sent.
            drain_with_errors(&channels);
            return;
        }
    };
    // Consume the server's Ready handshake for connection 2, and check its
    // version rather than discarding it. `launch.rs` already gated the same
    // server on connection 1, so a mismatch here is not reachable today — but
    // this is a wire boundary, the check is one comparison off the audio path,
    // and a silently-ignored version field is how a skew becomes a mis-parse
    // instead of an error.
    if let Err(failure) = expect_ready(&mut stream) {
        crash(&lifecycle, &listener, failure);
        drain_with_errors(&channels);
        return;
    }
    // The control connection (3): the server accepts it after the audio one.
    let control = match ipc::connect(&socket_path) {
        Ok(s) => s,
        Err(e) => {
            crash(
                &lifecycle,
                &listener,
                PluginFailure::Connect {
                    source: Arc::new(e),
                },
            );
            drain_with_errors(&channels);
            return;
        }
    };
    let mut control = control;
    if let Err(failure) = expect_ready(&mut control) {
        crash(&lifecycle, &listener, failure);
        drain_with_errors(&channels);
        return;
    }
    let mut lanes = Lanes {
        audio: Lane {
            stream,
            owed: Owed::default(),
        },
        control: Lane {
            stream: control,
            owed: Owed::default(),
        },
    };
    // The control connection is also read when the server speaks unprompted
    // (a request to be processed again, a latency change), whether or not a
    // command is waiting: a thread that only waits on it tells the bridge.
    match ipc::Watch::of(&lanes.control.stream) {
        Ok(watch) => watch_control(watch, channels.clone(), lifecycle.clone()),
        Err(e) => tracing::warn!("the bridge cannot watch the control connection: {e}"),
    }
    pump(&channels, &payloads, &lifecycle, &listener, &mut lanes);
}

/// How long the watcher waits before it re-checks that the bridge still runs.
/// Only bounds how long a stopped bridge's watcher (and the duplicate of the
/// socket it holds) lives on; news ends the wait the instant it arrives.
const WATCH_SLICE: std::time::Duration = std::time::Duration::from_millis(20);

/// Spawns the watcher: it waits for the control connection to have bytes and
/// tells the bridge thread, then waits for the bridge to have read them.
fn watch_control(watch: ipc::Watch, channels: Channels, lifecycle: Lifecycle) {
    let spawned = thread::Builder::new()
        .name("plugin-bridge-news".to_string())
        .spawn(move || {
            while lifecycle.is_running() {
                if !watch.wait_readable(WATCH_SLICE) {
                    continue;
                }
                channels.set_news();
                // The bytes stay readable until the bridge reads them: wait
                // for it, so this thread does not spin on them meanwhile.
                while channels.news_pending() && lifecycle.is_running() {
                    thread::park_timeout(std::time::Duration::from_millis(1));
                }
            }
        });
    if let Err(e) = spawned {
        tracing::warn!("the bridge cannot watch the control connection: {e}");
    }
}

/// Reads a connection's `Ready` and checks its version.
///
/// The reason is spelled out per case rather than as one string: a version skew
/// and a peer that never said `Ready` are different problems for whoever reads
/// the message, and the first is actionable.
fn expect_ready(stream: &mut ControlStream) -> Result<(), PluginFailure> {
    match ipc::recv(stream) {
        Ok(crate::protocol::BridgeMessage::Ready { protocol_version })
            if crate::protocol::check_protocol_version(protocol_version).is_ok() =>
        {
            Ok(())
        }
        other => {
            let source = match other {
                Ok(crate::protocol::BridgeMessage::Ready { protocol_version }) => {
                    BridgeError::ProtocolMismatch {
                        expected: crate::protocol::PROTOCOL_VERSION,
                        got: protocol_version,
                    }
                }
                Ok(got) => crate::error::unexpected_message("Ready", &got),
                Err(e) => e,
            };
            Err(PluginFailure::Handshake {
                source: Arc::new(source),
            })
        }
    }
}

/// The bridge's command loop.
///
/// **Idle, the bridge sleeps on its doorbell, and a watcher thread rings it
/// when the control connection has bytes** ([`watch_control`]), so what the
/// server says unprompted (a request to be processed again, a latency or
/// parameter change, its own death) is read as it arrives, not with the next
/// command. While a command waits for its reply, `handle` reads the same lane
/// and the unsolicited events ride in on that read; either way they go to the
/// listener after the command. There is no timeout in the sleep: a command, the
/// watcher and `shutdown` each ring the doorbell, and a spurious return loops.
fn pump(
    channels: &Channels,
    payloads: &PayloadPool,
    lifecycle: &Lifecycle,
    listener: &ListenerSlot,
    lanes: &mut Lanes,
) {
    // Unsolicited events read while a command waited for its reply. One
    // buffer for the connection, handed to the listener after each command.
    let mut inbox = Unsolicited::with_lifecycle(lifecycle.clone());
    while lifecycle.is_running() {
        let cmd = match channels.pop_command() {
            Some(cmd) => cmd,
            // News on the control connection, no command queued: read it.
            None if channels.news_pending() => Command::Drain,
            None => {
                // Sleep on the doorbell: `push_command` rings it the instant a
                // block arrives, so the socket round-trip starts immediately,
                // and the watcher rings it when the server speaks. The ring
                // cannot be lost and `shutdown` rings it too, so there is no
                // timeout. A spurious return just loops.
                channels.wait_for_command(|| !lifecycle.is_running() || channels.news_pending());
                continue;
            }
        };
        let draining = matches!(cmd, Command::Drain);

        let result = handle(cmd, lanes, channels, payloads, &mut inbox, lifecycle);
        drain_unsolicited(&mut inbox, channels, listener);
        if draining {
            channels.clear_news();
        }

        if let Err(e) = result {
            // The error is classified here, at the only place it exists.
            // `BridgeError` is not `Clone`, and the crash notification outlives
            // this frame, so the failure holds it behind an `Arc`.
            crash(lifecycle, listener, PluginFailure::from_stream(e));
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
fn crash(lifecycle: &Lifecycle, listener: &ListenerSlot, cause: PluginFailure) {
    lifecycle.mark_crashed(cause.clone());
    let cb = listener.lock().clone();
    if let Some(f) = cb.as_ref() {
        f(BridgeEvent::Crashed { cause });
    }
}

fn drain_unsolicited(inbox: &mut Unsolicited, channels: &Channels, listener: &ListenerSlot) {
    channels.note_dropped_param_events(inbox.take_dropped());
    let cb = listener.lock().clone();
    for ev in inbox.drain() {
        if let Some(f) = cb.as_ref() {
            f(ev);
        }
    }
}

/// Answer every queued command with an error. A queued chunk is never sent,
/// so its MIDI is counted dropped; nothing can splice past it, as the server
/// is gone.
fn drain_with_errors(channels: &Channels) {
    while let Some(cmd) = channels.pop_command() {
        if let Command::Process(payload) = &cmd {
            channels.note_dropped_midi(payload.midi_events.len() as u64);
        }
        channels.push_audio_response(AudioResponse::Error { seq: None });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::audio::messages::{ProcessPayload, UNSOLICITED_PARAMS};
    use crate::protocol::{Normalized, ParamId};
    use std::sync::Arc;
    use tutti_fixtures::midi;
    use tutti_plugin_types::ResyncKind;
    use tutti_types::SampleRate;
    use tutti_ump::MidiChannel;

    /// **A crash's drain counts the queued chunks' MIDI.**
    #[test]
    fn a_crash_drain_counts_the_queued_midi() {
        use tutti_plugin_types::WireMidi;

        let channels = Channels::new(SampleRate::SR_48K);
        for n in [3, 0, 2] {
            let mut payload = Box::new(ProcessPayload::empty());
            for _ in 0..n {
                payload.midi_events.push(WireMidi {
                    offset: 0,
                    ump: midi::note_on(MidiChannel::FIRST, 60, 1),
                });
            }
            assert!(channels.push_command(Command::Process(payload)));
        }
        drain_with_errors(&channels);
        assert!(channels.pop_command().is_none());
        assert_eq!(
            channels
                .dropped_midi_cell()
                .load(std::sync::atomic::Ordering::Relaxed),
            5
        );
    }

    /// **A bridge that never connects counts the MIDI queued before it
    /// failed.** `run_thread` runs here on the test thread, so the chunk is
    /// queued before the connect fails, deterministically.
    #[test]
    fn a_failed_connect_counts_the_queued_midi() {
        use tutti_plugin_types::WireMidi;

        let channels = Channels::new(SampleRate::SR_48K);
        let lifecycle = Lifecycle::new();
        let mut payload = Box::new(ProcessPayload::empty());
        for _ in 0..4 {
            payload.midi_events.push(WireMidi {
                offset: 0,
                ump: midi::note_on(MidiChannel::FIRST, 60, 1),
            });
        }
        assert!(channels.push_command(Command::Process(payload)));
        let socket =
            std::env::temp_dir().join(format!("tutti-no-such-bridge-{}.sock", std::process::id()));
        run_thread(
            channels.clone(),
            PayloadPool::new(),
            lifecycle.clone(),
            Arc::new(parking_lot::Mutex::new(None)),
            socket,
        );
        assert!(lifecycle.is_crashed());
        assert!(channels.pop_command().is_none());
        assert_eq!(
            channels
                .dropped_midi_cell()
                .load(std::sync::atomic::Ordering::Relaxed),
            4
        );
    }

    /// **A drain hands every held event to the listener and publishes the
    /// drop count to the bridge's counter, cumulatively.** The inbox's own
    /// test pins what it keeps; this pins that `pump`'s drain reports it.
    #[test]
    fn a_drain_reports_the_drops() {
        let channels = Channels::new(SampleRate::SR_48K);
        let heard = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let listener: ListenerSlot = {
            let heard = Arc::clone(&heard);
            Arc::new(parking_lot::Mutex::new(Some(Arc::new(move |ev| {
                heard.lock().push(format!("{ev:?}"));
            }))))
        };
        let mut inbox = Unsolicited::new();
        let over = 72;
        for round in 1..=2u64 {
            for i in 0..(UNSOLICITED_PARAMS + over) as i32 {
                inbox.param(tutti_plugin_types::ParamEdit::change(
                    ParamId::new(i as u32),
                    Normalized::default(),
                ));
            }
            drain_unsolicited(&mut inbox, &channels, &listener);
            assert_eq!(channels.dropped_param_events(), round * over as u64);
        }
        let heard = heard.lock();
        assert_eq!(
            heard.len(),
            2 * (UNSOLICITED_PARAMS + 1),
            "each drain: the kept params and one resync"
        );
        assert_eq!(
            heard.last().map(String::as_str),
            Some(format!("{:?}", BridgeEvent::Resync(ResyncKind::ParamValues)).as_str())
        );
    }
}
