//! What happens when the plugin-server stops behaving?
//!
//! The existing mock-server tests all script a *well-formed* reply, and every
//! crash test uses `bridge_with_no_server()` — a peer that was **never there**.
//! Neither covers a peer that was there and then stopped being there, which is
//! the failure that actually happens in production: a plugin segfaults
//! mid-session, taking its server down with it, while the audio thread is
//! waiting on a reply.
//!
//! Two families here:
//!
//! - **Death mid-session.** The socket closes after a successful exchange. The
//!   host must mark the bridge crashed, release everything in flight, and keep
//!   the audio thread moving.
//! - **A peer that is hostile or corrupt.** `recv` reads a 4-byte big-endian
//!   length and then does `vec![0u8; len]` — an attacker-controlled allocation.
//!   Garbage bincode, a truncated body, and a valid-but-wrong message type are
//!   the same class: bytes the host must reject rather than trust.
//!
//! The bar is not that the host produces audio — a dead peer has none to give.
//! It is that the host **fails in bounded time and stays usable**: no hang, no
//! unbounded allocation, no panic escaping into the audio thread.

use super::PluginBridge;
use crate::host::ipc_client::audio::BridgeThread;
use crate::protocol::{
    BridgeMessage, ChannelLayout, HostMessage, ParamAddress, ParamId, SampleFormat, SlabLayout,
    PROTOCOL_VERSION,
};
use crate::util::transport::shm::{AudioSlab, RING_SLOTS};
use smallvec::smallvec;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use interprocess::local_socket::{
    traits::Listener as _, GenericFilePath, ListenerOptions, Stream, ToFsName as _,
};

/// Upper bound on how long any single "the host must notice" wait may take.
///
/// Generous relative to the bridge's own timeouts (2–50 ms for a process reply,
/// 5 s for a param) so a loaded machine does not fail this spuriously, but
/// finite so a genuine hang is a failure rather than a hung test run.
const NOTICE_TIMEOUT: Duration = Duration::from_secs(10);

fn unique_socket_path(label: &str) -> std::path::PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "tutti-hostile-{}-{}-{}.sock",
        label,
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

fn unique_shm_name(label: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "hostile_{}_{}_{}",
        label,
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn stereo_layout() -> SlabLayout {
    SlabLayout {
        slots: RING_SLOTS as u32,
        samples_per_channel: 512,
        format: SampleFormat::Float32,
        inputs: smallvec![ChannelLayout::STEREO],
        outputs: smallvec![ChannelLayout::STEREO],
    }
}

fn recv_host_msg(stream: &Stream) -> std::io::Result<HostMessage> {
    let mut stream = stream;
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf)?;
    bincode::deserialize(&buf).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

fn send_bridge_msg(stream: &Stream, msg: &BridgeMessage) -> std::io::Result<()> {
    let mut stream = stream;
    let data = bincode::serialize(msg).expect("serialize");
    stream.write_all(&(data.len() as u32).to_be_bytes())?;
    stream.write_all(&data)
}

/// Write a raw frame the host must cope with but which no honest server would
/// send. `len` is the advertised length; `body` is what actually follows, so the
/// two can deliberately disagree.
fn send_raw_frame(stream: &Stream, len: u32, body: &[u8]) -> std::io::Result<()> {
    let mut stream = stream;
    stream.write_all(&len.to_be_bytes())?;
    stream.write_all(body)
}

/// A mock server whose behaviour a test scripts per message, and which can drop
/// its connection part-way through.
///
/// The `respond` closure returns `Action`, so a test can answer normally for a
/// while and then die — which is the whole point of this file. Returning
/// [`Action::Die`] closes the socket the way a segfaulting server does: no
/// shutdown message, no goodbye, just EOF at the host's next read.
enum Action {
    /// Boxed: `BridgeMessage` inlines a ~5 KB MIDI vec, and an unboxed variant
    /// would make every `Action` that large.
    Reply(Box<BridgeMessage>),
    /// Answer nothing and keep the connection open (a hung server).
    Silent,
    /// Close the connection immediately (a crashed server).
    Die,
    /// Send a raw, deliberately malformed frame: `(advertised_len, body)`.
    Raw(u32, Vec<u8>),
}

struct MockServer {
    bridge: Arc<PluginBridge>,
    /// `Option` only so `Drop` can take it out and `mem::forget` it; see there.
    thread: Option<BridgeThread>,
    server: Option<std::thread::JoinHandle<()>>,
    socket: std::path::PathBuf,
}

impl MockServer {
    fn start(label: &str, respond: impl Fn(HostMessage) -> Action + Send + 'static) -> Self {
        let path = unique_socket_path(label);
        let _ = std::fs::remove_file(&path);
        let name = path
            .clone()
            .to_fs_name::<GenericFilePath>()
            .expect("socket name");
        let listener = ListenerOptions::new()
            .name(name)
            .create_sync()
            .expect("listen");

        let slab = Arc::new(
            AudioSlab::create(unique_shm_name(label), stereo_layout()).expect("create slab"),
        );
        let (bridge, thread) = PluginBridge::new(
            path.clone(),
            slab,
            std::path::PathBuf::from("test.vst3"),
            48_000.0,
        )
        .expect("bridge");

        let stream = listener.accept().expect("accept");
        send_bridge_msg(
            &stream,
            &BridgeMessage::Ready {
                protocol_version: PROTOCOL_VERSION,
            },
        )
        .expect("handshake");

        let server = std::thread::Builder::new()
            .name(format!("mock-hostile-{label}"))
            .spawn(move || loop {
                let Ok(msg) = recv_host_msg(&stream) else {
                    return; // host hung up
                };
                match respond(msg) {
                    Action::Reply(m) => {
                        if send_bridge_msg(&stream, &m).is_err() {
                            return;
                        }
                    }
                    Action::Silent => {}
                    Action::Raw(len, body) => {
                        if send_raw_frame(&stream, len, &body).is_err() {
                            return;
                        }
                    }
                    // Dropping `stream` closes the socket, so the host's next
                    // read sees EOF — exactly what a killed server produces.
                    Action::Die => return,
                }
            })
            .expect("spawn mock");

        // Let the bridge thread connect and consume the handshake before a test
        // starts asserting on it.
        std::thread::sleep(Duration::from_millis(50));

        Self {
            bridge,
            thread: Some(thread),
            server: Some(server),
            socket: path,
        }
    }

    /// Poll until the bridge reports itself crashed, or `NOTICE_TIMEOUT` passes.
    ///
    /// Polling rather than a single read because `mark_crashed` happens on the
    /// bridge thread and is published with `Release`/`Acquire` — a single read
    /// races it and would make this flaky rather than wrong.
    fn wait_for_crash(&self) -> bool {
        let deadline = Instant::now() + NOTICE_TIMEOUT;
        while Instant::now() < deadline {
            if self.bridge.is_crashed() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        false
    }
}

impl Drop for MockServer {
    /// Tear down **without joining anything**.
    ///
    /// Both obvious joins can block indefinitely in exactly the scenarios this
    /// file constructs, and a hang in teardown is as bad as a hang in the test:
    ///
    /// - `BridgeThread::shutdown` joins the bridge thread, which may be parked
    ///   in `recv_within` waiting out a reply timeout.
    /// - joining the mock server thread waits on a socket read that only ends
    ///   when the host hangs up — which the line above was supposed to cause.
    ///
    /// Verified rather than assumed: raising the bridge's timeouts to an hour
    /// (a mutation that *should* have failed four tests) instead wedged the
    /// whole run, with three threads in `futex_wait` inside this `drop` and the
    /// mock threads in `unix_stream_data_wait`.
    ///
    /// So the socket file is removed and both threads are detached. Each exits
    /// on its own once its read fails or its timeout expires; the process is a
    /// test binary that is about to end anyway. `BridgeThread`'s own `Drop`
    /// joins, so it is `mem::forget`-ed rather than dropped — the cost is a
    /// detached thread for the life of the test binary, and the alternative is
    /// a run that never finishes.
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
        if let Some(h) = self.server.take() {
            drop(h); // detach: joining would wait on a socket read
        }
        if let Some(t) = self.thread.take() {
            std::mem::forget(t); // detach: its Drop joins
        }
    }
}

/// Run `f` on a worker thread and give up after `NOTICE_TIMEOUT`.
///
/// **A test asserting "this returns in bounded time" must not itself block
/// forever to find out.** Measuring `Instant::elapsed()` around a direct call
/// only works if the call returns: raising the bridge's timeouts to an hour made
/// two of these tests hang the whole `cargo test` run instead of failing, which
/// is strictly worse than a red test — CI reports a timeout with no attribution,
/// and a developer sees a wedged terminal.
///
/// So the call happens elsewhere and this waits on a channel. `Err` means it was
/// still running at the deadline, which is the failure the caller reports.
///
/// The worker is deliberately left running rather than joined: it is blocked in
/// the very call under test, so joining it would reintroduce the hang. It holds
/// only an `Arc<PluginBridge>` clone and exits when its timeout finally expires.
fn call_within<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> std::result::Result<(T, Duration), Duration> {
    let (tx, rx) = crossbeam_channel::bounded(1);
    let start = Instant::now();
    std::thread::Builder::new()
        .name("hostile-call".to_string())
        .spawn(move || {
            let value = f();
            let _ = tx.send(value);
        })
        .expect("spawn call thread");

    match rx.recv_timeout(NOTICE_TIMEOUT) {
        Ok(v) => Ok((v, start.elapsed())),
        Err(_) => Err(start.elapsed()),
    }
}

// ── Death mid-session ────────────────────────────────────────────────────────

/// A server that dies **after** a successful exchange must mark the bridge
/// crashed.
///
/// Distinct from `bridge_with_no_server()`, which never connects: there the
/// failure happens in `run_thread` before `pump` is ever entered. Here the
/// bridge is inside `pump`, mid-conversation, with the connection already
/// proven good — the path where a real plugin segfault lands, and the one no
/// existing test covers.
#[test]
fn a_server_that_dies_mid_session_marks_the_bridge_crashed() {
    use std::sync::atomic::AtomicUsize;
    static SEEN: AtomicUsize = AtomicUsize::new(0);
    SEEN.store(0, Ordering::SeqCst);

    let mock = MockServer::start("mid-death", |msg| match msg {
        HostMessage::GetParameter { .. } => {
            // Answer the first request, die on the second. Answering once is
            // what makes this "died mid-session" rather than "never worked".
            if SEEN.fetch_add(1, Ordering::SeqCst) == 0 {
                Action::Reply(Box::new(BridgeMessage::ParameterValue { value: Some(0.5) }))
            } else {
                Action::Die
            }
        }
        _ => Action::Silent,
    });

    let first = mock.bridge.parameter(ParamAddress::Opaque(ParamId::new(1)));
    assert_eq!(
        first,
        Some(0.5),
        "the mock must answer the first request, or this test is just \
         `bridge_with_no_server` under another name"
    );
    assert!(
        !mock.bridge.is_crashed(),
        "the bridge reported a crash while the server was still healthy"
    );

    // Second request: the server dies instead of replying.
    let second = mock.bridge.parameter(ParamAddress::Opaque(ParamId::new(1)));
    assert_eq!(
        second, None,
        "a request to a dead server returned a value: {second:?}"
    );
    assert!(
        mock.wait_for_crash(),
        "the server closed the connection mid-session but the bridge never \
         marked itself crashed, so `is_crashed` stays false forever and every \
         downstream liveness check is dead code"
    );
}

// ── The cause, and who hears about it ────────────────────────────────────────

/// A mid-session death latches *why*, not just *that*.
///
/// The cause has to be captured where the failure is noticed: `BridgeError` is
/// not `Clone` and is dropped as soon as the failing call returns, so a host
/// that polls afterwards can only report a placeholder — which is exactly what
/// it used to do, for every death alike.
///
/// Asserts non-emptiness rather than an exact string: the message comes from
/// the transport and differs across platforms ("connection reset", "broken
/// pipe"). Pinning the text would make this a test of the OS.
#[test]
fn a_crash_latches_a_cause_that_outlives_the_call() {
    let mock = MockServer::start("cause-latch", |_| Action::Die);

    assert_eq!(
        mock.bridge.crash_cause(),
        None,
        "a bridge reported a cause before anything went wrong"
    );

    let _ = mock.bridge.parameter(ParamAddress::Opaque(ParamId::new(1)));
    assert!(
        mock.wait_for_crash(),
        "the peer died but the bridge did not notice"
    );

    let cause = mock.bridge.crash_cause();
    assert!(
        cause.as_deref().is_some_and(|c| !c.is_empty()),
        "a crashed bridge reported no cause ({cause:?}); the reason is gone \
         once the failing call returns, so latching it at the detection site \
         is the only chance to keep it"
    );
}

/// A listener installed before the death is told, and told why.
///
/// The notification is the half that makes a crash *prompt*: without it a host
/// has to poll every frame to discover something that already happened.
#[test]
fn a_listener_hears_the_crash_with_its_cause() {
    use parking_lot::Mutex as PlMutex;

    let mock = MockServer::start("crash-event", |_| Action::Die);

    let heard: Arc<PlMutex<Vec<String>>> = Arc::new(PlMutex::new(Vec::new()));
    let sink = Arc::clone(&heard);
    mock.bridge.set_listener(Some(Arc::new(move |ev| {
        if let crate::host::ipc_client::audio::BridgeEvent::Crashed { cause } = ev {
            sink.lock().push(cause);
        }
    })));

    let _ = mock.bridge.parameter(ParamAddress::Opaque(ParamId::new(1)));
    assert!(
        mock.wait_for_crash(),
        "the peer died but the bridge did not notice"
    );

    let seen = heard.lock().clone();
    assert_eq!(
        seen.len(),
        1,
        "expected exactly one crash notification, got {seen:?} — firing per \
         failed in-flight request instead of once at the flag would flood a \
         host with duplicates of one death"
    );
    assert!(
        !seen[0].is_empty(),
        "the crash notification carried an empty cause"
    );
}

/// A crash that happens **before** any listener exists is still reported.
///
/// This is the case a callback-only design loses. `PluginBridge::new` spawns
/// the bridge thread and `set_listener` runs afterwards, so a peer that never
/// connects — the common shape of a bad install — dies in the gap with nobody
/// subscribed. The latch is what makes the query authoritative rather than a
/// convenience beside the event.
#[test]
fn a_crash_before_any_listener_is_still_reported_by_the_query() {
    let mock = MockServer::start("pre-listener", |_| Action::Die);

    // No listener is ever installed. Provoke the death.
    let _ = mock.bridge.parameter(ParamAddress::Opaque(ParamId::new(1)));
    assert!(
        mock.wait_for_crash(),
        "the peer died but the bridge did not notice"
    );

    assert!(
        mock.bridge.crash_cause().is_some(),
        "a crash with no listener installed left no cause behind, so a host \
         that subscribes later can never learn why its plugin is dead"
    );
}

/// Once crashed, further calls must fail fast rather than block.
///
/// A host that keeps waiting the full timeout on every subsequent request turns
/// one dead plugin into a UI that hangs for `PARAM_TIMEOUT` per interaction. The
/// crash flag exists to short-circuit that, so it has to actually be consulted.
#[test]
fn calls_after_a_crash_fail_fast_instead_of_waiting_out_the_timeout() {
    let mock = MockServer::start("fail-fast", |_| Action::Die);

    // First call establishes the crash.
    let _ = mock.bridge.parameter(ParamAddress::Opaque(ParamId::new(1)));
    assert!(
        mock.wait_for_crash(),
        "the bridge never noticed the server was gone"
    );

    // Subsequent calls must return promptly. `PARAM_TIMEOUT` is 5 s, so
    // anything near that means the short-circuit is missing.
    let start = Instant::now();
    for _ in 0..5 {
        assert_eq!(
            mock.bridge.parameter(ParamAddress::Opaque(ParamId::new(1))),
            None,
            "a crashed bridge returned a parameter value"
        );
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(1),
        "five calls to a crashed bridge took {elapsed:?} — the crash flag is \
         not short-circuiting, so each call is waiting out its full timeout"
    );
}

/// A crash must not leave a caller blocked forever.
///
/// `SaveState` waits on a one-shot `Ask`. If the bridge thread dies without
/// sending into the paired `Reply`, the caller depends on either the channel
/// disconnecting or its own timeout. Both are acceptable; blocking forever is
/// not, and only a bounded test can tell them apart.
#[test]
fn a_crash_during_save_state_does_not_block_the_caller_forever() {
    let mock = MockServer::start("save-death", |msg| match msg {
        HostMessage::SaveState => Action::Die,
        _ => Action::Silent,
    });

    let bridge = Arc::clone(&mock.bridge);
    let (saved, elapsed) = call_within(move || bridge.save_state()).unwrap_or_else(|waited| {
        panic!(
            "save_state against a server that died mid-request was still \
             blocked after {waited:?}; the caller is waiting on a reply that \
             will never arrive"
        )
    });
    assert!(
        saved.is_none(),
        "save_state returned {saved:?} from a server that never answered \
         (after {elapsed:?})"
    );
}

// ── Hostile / corrupt peers ──────────────────────────────────────────────────

/// A wildly oversized length prefix must not be honoured as an allocation.
///
/// `recv` reads a 4-byte big-endian length and then does `vec![0u8; len]`. The
/// length comes off the wire, so `0xFFFFFFFF` asks the host to allocate 4 GiB
/// before a single byte of body has been validated. On a machine with
/// overcommit that is a large mapping and a long stall; without it, an abort.
///
/// The body deliberately does not follow: a server that advertises 4 GiB and
/// sends 4 bytes is exactly the corrupt-framing case. The host must fail the
/// read rather than sit on the allocation.
#[test]
fn an_absurd_length_prefix_does_not_hang_or_exhaust_memory() {
    let mock = MockServer::start("huge-len", |msg| match msg {
        HostMessage::GetParameter { .. } => Action::Raw(u32::MAX, vec![0u8; 4]),
        _ => Action::Silent,
    });

    let bridge = Arc::clone(&mock.bridge);
    let (value, elapsed) =
        call_within(move || bridge.parameter(ParamAddress::Opaque(ParamId::new(1))))
            .unwrap_or_else(|waited| {
                panic!(
                    "a 4 GiB length prefix left the host blocked after {waited:?} — it \
             is allocating on an unvalidated wire length"
                )
            });
    assert_eq!(
        value, None,
        "the host returned a parameter value from a frame it could not have \
         read (after {elapsed:?})"
    );
    assert!(
        mock.wait_for_crash(),
        "an unreadable frame left the bridge believing the connection was healthy"
    );
}

/// Garbage where a bincode payload should be must be rejected, not decoded.
///
/// The length is honest here and the body is the right size; only the *contents*
/// are nonsense. That isolates deserialisation from framing — a host that
/// somehow produced a message from these bytes would be reading uninitialised
/// or attacker-chosen structure.
#[test]
fn a_garbage_payload_is_rejected_rather_than_decoded() {
    // Honest length, nonsense body: this isolates *deserialisation* from
    // framing, unlike the truncated and oversized cases.
    const GARBAGE_LEN: usize = 64;
    let mock = MockServer::start("garbage", |msg| match msg {
        HostMessage::GetParameter { .. } => {
            Action::Raw(GARBAGE_LEN as u32, vec![0xABu8; GARBAGE_LEN])
        }
        _ => Action::Silent,
    });

    let value = mock.bridge.parameter(ParamAddress::Opaque(ParamId::new(1)));
    assert_eq!(
        value, None,
        "the host decoded a parameter value out of 64 bytes of 0xAB"
    );
    assert!(
        mock.wait_for_crash(),
        "an undecodable payload left the bridge believing the connection was healthy"
    );
}

/// A frame whose body is shorter than its own length prefix must fail.
///
/// This is what a server killed *mid-write* produces: the header made it out,
/// the body did not. The host must not block forever waiting for bytes that
/// will never arrive, nor treat the short read as a complete message.
#[test]
fn a_truncated_body_is_not_mistaken_for_a_complete_message() {
    let mock = MockServer::start("truncated", |msg| match msg {
        // Advertise 4 KiB, send 8 bytes, then the closure returns and the
        // server loops — the remaining bytes never come.
        HostMessage::GetParameter { .. } => Action::Raw(4096, vec![0u8; 8]),
        _ => Action::Silent,
    });

    let bridge = Arc::clone(&mock.bridge);
    let (value, elapsed) =
        call_within(move || bridge.parameter(ParamAddress::Opaque(ParamId::new(1))))
            .unwrap_or_else(|waited| {
                panic!(
                    "a truncated frame left the host blocked after {waited:?}, waiting \
             for a body the server never finished sending"
                )
            });
    assert_eq!(
        value, None,
        "a truncated frame produced a value (after {elapsed:?})"
    );
}

/// A well-formed message of the *wrong type* must not satisfy a pending request.
///
/// Every byte here is valid — this is a legal `BridgeMessage`, just not the one
/// that was asked for. A host that matched on "some reply arrived" rather than
/// on the variant would accept it and return a value it never received. The
/// existing `trailing_unsolicited_events_dont_poison_next_reply` covers
/// *unsolicited* interleaving; this covers a substituted reply.
#[test]
fn a_valid_but_wrong_reply_type_does_not_satisfy_the_request() {
    let mock = MockServer::start("wrong-type", |msg| match msg {
        // Asked for a parameter value; answer with state data instead.
        HostMessage::GetParameter { .. } => Action::Reply(Box::new(BridgeMessage::StateData {
            data: vec![1, 2, 3, 4],
        })),
        _ => Action::Silent,
    });

    let value = mock.bridge.parameter(ParamAddress::Opaque(ParamId::new(1)));
    assert_eq!(
        value, None,
        "a StateData reply was accepted as the answer to GetParameter — the \
         host is matching on 'a reply arrived', not on which reply"
    );
}

/// A server that accepts requests but never answers must not hang the host.
///
/// Distinct from death: the connection stays open, so there is no EOF to notice.
/// Only the timeout can end this, which makes it the test that proves the
/// timeout exists at all.
#[test]
fn a_silent_server_is_bounded_by_the_timeout() {
    let mock = MockServer::start("silent", |_| Action::Silent);

    let bridge = Arc::clone(&mock.bridge);
    let (value, elapsed) =
        call_within(move || bridge.parameter(ParamAddress::Opaque(ParamId::new(1))))
            .unwrap_or_else(|waited| {
                panic!(
                    "a server that never replies left the caller blocked for {waited:?} \
             — the request timeout is not bounding this path"
                )
            });
    assert_eq!(
        value, None,
        "a silent server produced a value (after {elapsed:?})"
    );
}
