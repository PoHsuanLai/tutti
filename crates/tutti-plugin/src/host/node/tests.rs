//! Mock-server integration tests for `PluginHandle`.
//!
//! Each test stands up a real [`tutti_plugin_ipc::PluginBridge`] against a
//! mock plugin-server running on a std thread over an interprocess
//! socket, then drives `PluginHandle` IPC round-trips through it. The
//! `respond` callback lets each test script the server's replies.

use std::sync::Arc;
use tutti_fixtures::main_thread;
use tutti_plugin_host::PluginHandle;
use tutti_plugin_ipc::AudioSlab;
use tutti_plugin_ipc::PluginBridge;
use tutti_plugin_ipc::SlabLayout;
use tutti_plugin_ipc::unique_socket_path;
use tutti_plugin_ipc::{BridgeEvent, BridgeThread};
use tutti_plugin_ipc::{BridgeMessage, HostMessage, PROTOCOL_VERSION};
use tutti_plugin_types::{
    ChannelLayout, Features, LoadedPlugin, Normalized, ParamId, ParameterInfo, PluginBus,
    PluginDescriptor, PluginTail, Samples,
};
use tutti_plugin_types::{EditorPresence, PluginClass, SampleFormat};
use tutti_types::SampleRate;

/// A handle over `bridge` as a served plugin's control serves it, with a guard
/// that owns no subprocess.
fn handle_over(
    bridge: Arc<PluginBridge>,
    slab: tutti_plugin_ipc::SlabWriter,
    descriptor: PluginDescriptor,
    loaded: LoadedPlugin,
) -> PluginHandle {
    let guard = Arc::new(tutti_plugin_ipc::ProcessGuard::for_test(
        tutti_plugin_ipc::BridgeConfig::default(),
    ));
    let ipc = tutti_plugin_ipc::Ipc::new(tutti_plugin_ipc::IpcParts {
        bridge,
        guard,
        descriptor: descriptor.clone(),
        loaded: loaded.clone(),
        format: SampleFormat::Float32,
        ceiling: 512,
        slab,
        rate: SampleRate(48_000.0),
        parameters: 0,
        timeout: std::time::Duration::from_secs(5),
        origin: None,
    });
    let served = Arc::new(ipc.controller());
    PluginHandle::from_backend(
        Arc::clone(&served),
        Some(served),
        descriptor,
        loaded,
        Default::default(),
    )
}

fn unique_shm_name(label: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "test_{}_{}_{}",
        label,
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// Read a `HostMessage` from an interprocess socket (blocking).
fn recv_host_msg(stream: &interprocess::local_socket::Stream) -> HostMessage {
    use std::io::Read;
    let mut stream = stream;
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).unwrap();
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).unwrap();
    postcard::from_bytes(&buf).unwrap()
}

/// Send a `BridgeMessage` to an interprocess socket (blocking).
fn send_bridge_msg(stream: &interprocess::local_socket::Stream, msg: &BridgeMessage) {
    use std::io::Write;
    let mut stream = stream;
    let data = postcard::to_allocvec(msg).unwrap();
    let len = (data.len() as u32).to_be_bytes();
    stream.write_all(&len).unwrap();
    stream.write_all(&data).unwrap();
}

/// Create a `PluginHandle` backed by a responsive mock server. The
/// `respond` callback is invoked for each inbound `HostMessage`; returning
/// `Some(..)` sends that `BridgeMessage` back, `None` silently drops.
fn handle_with_mock_server(
    respond: impl Fn(HostMessage) -> Option<BridgeMessage> + Send + 'static,
) -> (PluginHandle, BridgeThread, std::thread::JoinHandle<()>) {
    use interprocess::local_socket::{ListenerOptions, traits::Listener as _};

    let path = unique_socket_path();
    let _ = std::fs::remove_file(&path);
    let name = tutti_plugin_ipc::socket_name(&path).unwrap();
    let listener = ListenerOptions::new().name(name).create_sync().unwrap();

    // The test writes no audio: the slab is kept for the served plugin it builds.
    let slab = AudioSlab::create(
        unique_shm_name("handle"),
        SlabLayout {
            slots: tutti_plugin_ipc::RING_SLOTS as u32,
            samples_per_channel: 512,
            format: SampleFormat::Float32,
            inputs: vec![ChannelLayout::STEREO],
            outputs: vec![ChannelLayout::STEREO],
        },
    )
    .unwrap();
    let buffer = Arc::clone(slab.slab());
    let (bridge, bridge_thread) = PluginBridge::new(
        path.clone(),
        buffer,
        std::path::PathBuf::from("test.vst3"),
        crate::format::gui::load_gui_instance,
        SampleRate(48_000.0),
    )
    .unwrap();

    let server_stream = listener.accept().unwrap();
    send_bridge_msg(
        &server_stream,
        &BridgeMessage::Ready {
            protocol_version: PROTOCOL_VERSION,
        },
    );

    let path_cleanup = path;
    let server_thread = std::thread::Builder::new()
        .name("mock-plugin-server".to_string())
        .spawn(move || {
            struct Cleanup(std::path::PathBuf);
            impl Drop for Cleanup {
                fn drop(&mut self) {
                    let _ = std::fs::remove_file(&self.0);
                }
            }
            let _cleanup = Cleanup(path_cleanup);
            loop {
                let msg = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    recv_host_msg(&server_stream)
                })) {
                    Ok(msg) => msg,
                    Err(_) => break,
                };
                if let Some(response) = respond(msg)
                    && std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        send_bridge_msg(&server_stream, &response)
                    }))
                    .is_err()
                {
                    break;
                }
            }
        })
        .unwrap();

    // Give the bridge thread time to start up and register I/O.
    std::thread::sleep(std::time::Duration::from_millis(50));

    let descriptor = PluginDescriptor {
        id: "test.plugin".into(),
        name: "Test Plugin".into(),
        vendor: String::new(),
        version: String::new(),
        class: PluginClass::Unknown,
        editor: EditorPresence::Present,
    };
    let loaded = LoadedPlugin {
        inputs: vec![PluginBus::main(ChannelLayout::STEREO)],
        outputs: vec![PluginBus::main(ChannelLayout::STEREO)],
        latency_samples: Samples::ZERO,
        tail: PluginTail::Unknown,
        features: Features::EDITOR,
        probed: Features::EDITOR,
    };
    let plugin_handle = handle_over(bridge, slab, descriptor, loaded);

    (plugin_handle, bridge_thread, server_thread)
}

/// Variant that (1) returns the `PluginBridge` so tests can install a
/// listener, and (2) lets `respond` emit zero-or-more messages per
/// request — which is what allows scripting trailing unsolicited events.
fn handle_with_multi_reply_server(
    respond: impl Fn(HostMessage) -> Vec<BridgeMessage> + Send + 'static,
) -> (
    PluginHandle,
    Arc<PluginBridge>,
    BridgeThread,
    std::thread::JoinHandle<()>,
) {
    use interprocess::local_socket::{ListenerOptions, traits::Listener as _};

    let path = unique_socket_path();
    let _ = std::fs::remove_file(&path);
    let name = tutti_plugin_ipc::socket_name(&path).unwrap();
    let listener = ListenerOptions::new().name(name).create_sync().unwrap();

    // The test writes no audio: the slab is kept for the served plugin it builds.
    let slab = AudioSlab::create(
        unique_shm_name("handle-multi"),
        SlabLayout {
            slots: tutti_plugin_ipc::RING_SLOTS as u32,
            samples_per_channel: 512,
            format: SampleFormat::Float32,
            inputs: vec![ChannelLayout::STEREO],
            outputs: vec![ChannelLayout::STEREO],
        },
    )
    .unwrap();
    let buffer = Arc::clone(slab.slab());
    let (bridge, bridge_thread) = PluginBridge::new(
        path.clone(),
        buffer,
        std::path::PathBuf::from("test.vst3"),
        crate::format::gui::load_gui_instance,
        SampleRate(48_000.0),
    )
    .unwrap();

    let server_stream = listener.accept().unwrap();
    send_bridge_msg(
        &server_stream,
        &BridgeMessage::Ready {
            protocol_version: PROTOCOL_VERSION,
        },
    );

    let path_cleanup = path;
    let server_thread = std::thread::Builder::new()
        .name("mock-plugin-server".to_string())
        .spawn(move || {
            struct Cleanup(std::path::PathBuf);
            impl Drop for Cleanup {
                fn drop(&mut self) {
                    let _ = std::fs::remove_file(&self.0);
                }
            }
            let _cleanup = Cleanup(path_cleanup);
            loop {
                let msg = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    recv_host_msg(&server_stream)
                })) {
                    Ok(msg) => msg,
                    Err(_) => break,
                };
                let replies = respond(msg);
                for reply in replies {
                    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        send_bridge_msg(&server_stream, &reply)
                    }))
                    .is_err()
                    {
                        return;
                    }
                }
            }
        })
        .unwrap();

    std::thread::sleep(std::time::Duration::from_millis(50));

    let descriptor = PluginDescriptor {
        id: "test.plugin".into(),
        name: "Test Plugin".into(),
        vendor: String::new(),
        version: String::new(),
        class: PluginClass::Unknown,
        editor: EditorPresence::Present,
    };
    let loaded = LoadedPlugin {
        inputs: vec![PluginBus::main(ChannelLayout::STEREO)],
        outputs: vec![PluginBus::main(ChannelLayout::STEREO)],
        latency_samples: Samples::ZERO,
        tail: PluginTail::Unknown,
        features: Features::EDITOR,
        probed: Features::EDITOR,
    };
    let plugin_handle = handle_over(Arc::clone(&bridge), slab, descriptor, loaded);

    (plugin_handle, bridge, bridge_thread, server_thread)
}

#[test]
fn trailing_unsolicited_events_dont_poison_next_reply() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Server scripts `ParameterList` reply *followed by* two unsolicited
    // events (`LatencyChanged`, `ParameterChanged`). Without the dispatch
    // helper, those trailing events would mis-route as the reply to the
    // next `GetParameter` request.
    let event_count = Arc::new(AtomicUsize::new(0));
    let events_for_server = Arc::clone(&event_count);

    let (handle, bridge, _bridge_thread, _server_thread) =
        handle_with_multi_reply_server(move |msg| match msg {
            HostMessage::GetParameterList => vec![
                BridgeMessage::ParameterList {
                    parameters: vec![ParameterInfo::new(ParamId::new(0), "Vol".to_string())],
                },
                BridgeMessage::LatencyChanged {
                    samples: Samples(256),
                },
                BridgeMessage::ParameterChanged {
                    index: ParamId::new(7),
                    value: Normalized::of(0.42),
                },
            ],
            HostMessage::GetParameter { param_id } if param_id == ParamId::new(0) => {
                vec![BridgeMessage::ParameterValue {
                    value: Some(Normalized::of(0.5)),
                }]
            }
            HostMessage::GetParameter { .. } => vec![BridgeMessage::ParameterValue { value: None }],
            _ => {
                // Count any other request as a test failure signal.
                events_for_server.fetch_add(1, Ordering::Relaxed);
                vec![]
            }
        });

    let latency_seen = Arc::new(AtomicUsize::new(0));
    let param_seen = Arc::new(parking_lot::Mutex::new(None::<(u32, f32)>));

    {
        let latency_seen = Arc::clone(&latency_seen);
        let param_seen = Arc::clone(&param_seen);
        bridge.set_listener(Some(Arc::new(move |ev| match ev {
            BridgeEvent::LatencyChanged { samples } => {
                latency_seen.store(samples.get(), Ordering::Release);
            }
            BridgeEvent::ParameterChanged { index, value } => {
                *param_seen.lock() = Some((index.get(), value.get() as f32));
            }
            BridgeEvent::TailChanged { .. }
            | BridgeEvent::Resync(_)
            | BridgeEvent::RateRefusal(_)
            | BridgeEvent::Crashed { .. } => {}
        })));
    }

    // First request: receives `ParameterList`. Trailing events sit in
    // the socket buffer until the next `recv_reply` pulls them — which
    // happens on the next request.
    let params = handle.any_thread().parameter_descriptors().unwrap();
    assert_eq!(params.len(), 1);
    assert_eq!(params[0].name, "Vol");

    // Second request: the helper must peel the two trailing events off
    // before returning the real `ParameterValue` reply.
    assert_eq!(
        handle.any_thread().parameter_value(ParamId::new(0)),
        Some(Normalized::of(0.5))
    );

    // Give the bridge thread a moment to fire the listener after the
    // dispatch that drained the unsolicited queue.
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert_eq!(latency_seen.load(Ordering::Acquire), 256);
    assert_eq!(*param_seen.lock(), Some((7, 0.42)));
}

/// A `TailChanged` message arriving on the control stream must reach a
/// listener as a `BridgeEvent::TailChanged` carrying the same arm.
///
/// This is the client half of the runtime-tail path. The server half
/// (`a_clap_tail_change_reaches_the_event_list`, in `tutti-plugin-server`)
/// proves the notification becomes a message; this proves the message becomes
/// an event. `PluginClient` itself needs a live subprocess to build, so the
/// final hop — the listener storing into the cell the node's `Shape` tail reads
/// — is exercised by the plugin-server integration suite rather than here.
///
/// `Unbounded` is the arm under test deliberately: it is the one a count
/// cannot carry, so a regression that flattened the enum to a number on the
/// wire would still pass with `Finite`.
#[test]
fn a_tail_change_reaches_the_listener() {
    let (handle, bridge, _bridge_thread, _server_thread) =
        handle_with_multi_reply_server(move |msg| match msg {
            HostMessage::GetParameterList => vec![
                BridgeMessage::ParameterList {
                    parameters: vec![ParameterInfo::new(ParamId::new(0), "Vol".to_string())],
                },
                BridgeMessage::TailChanged {
                    tail: PluginTail::Unbounded,
                },
            ],
            HostMessage::GetParameter { .. } => {
                vec![BridgeMessage::ParameterValue {
                    value: Some(Normalized::of(0.5)),
                }]
            }
            _ => vec![],
        });

    let tail_seen = Arc::new(parking_lot::Mutex::new(None::<PluginTail>));
    {
        let tail_seen = Arc::clone(&tail_seen);
        bridge.set_listener(Some(Arc::new(move |ev| {
            if let BridgeEvent::TailChanged { tail } = ev {
                *tail_seen.lock() = Some(tail);
            }
        })));
    }

    // First request takes the reply; the trailing event waits in the socket
    // buffer until the next `recv_reply` drains it.
    assert_eq!(
        handle.any_thread().parameter_descriptors().unwrap().len(),
        1
    );
    assert_eq!(
        handle.any_thread().parameter_value(ParamId::new(0)),
        Some(Normalized::of(0.5))
    );

    std::thread::sleep(std::time::Duration::from_millis(50));
    assert_eq!(
        *tail_seen.lock(),
        Some(PluginTail::Unbounded),
        "the tail change did not reach the listener as an unbounded tail"
    );
}

#[test]
fn handle_save_state_roundtrip() {
    let (handle, _bridge_handle, _server_thread) = handle_with_mock_server(|msg| match msg {
        HostMessage::SaveState => Some(BridgeMessage::StateChunk {
            seq: 0,
            last: true,
            bytes: vec![0xDE, 0xAD, 0xBE, 0xEF],
        }),
        // A load is answered only on its final chunk; this mock sends none,
        // which is the "no response" this test's other arms rely on.
        HostMessage::LoadStateChunk { .. } => None,
        _ => None,
    });

    let state = handle.any_thread().save_state();
    assert_eq!(state.expect("mock answered"), vec![0xDE, 0xAD, 0xBE, 0xEF]);
}

#[test]
fn handle_get_parameter_list() {
    let (handle, _bridge_handle, _server_thread) = handle_with_mock_server(|msg| match msg {
        HostMessage::GetParameterList => Some(BridgeMessage::ParameterList {
            parameters: vec![
                ParameterInfo::new(ParamId::new(0), "Volume".to_string()),
                ParameterInfo::new(ParamId::new(1), "Pan".to_string()),
                ParameterInfo::new(ParamId::new(2), "Cutoff".to_string()),
            ],
        }),
        _ => None,
    });

    let params = handle.any_thread().parameter_descriptors().unwrap();
    assert_eq!(params.len(), 3);
    assert_eq!(params[0].name, "Volume");
    assert_eq!(params[1].name, "Pan");
    assert_eq!(params[2].name, "Cutoff");
}

#[test]
fn handle_get_parameter_value() {
    let (handle, _bridge_handle, _server_thread) = handle_with_mock_server(|msg| match msg {
        HostMessage::GetParameter { param_id } if param_id == ParamId::new(42) => {
            Some(BridgeMessage::ParameterValue {
                value: Some(Normalized::of(0.75)),
            })
        }
        HostMessage::GetParameter { .. } => Some(BridgeMessage::ParameterValue { value: None }),
        _ => None,
    });

    assert_eq!(
        handle.any_thread().parameter_value(ParamId::new(42)),
        Some(Normalized::of(0.75))
    );
}

#[test]
fn handle_open_editor_errors_for_missing_plugin() {
    // open_editor loads the plugin GUI in-process (not via IPC), so with
    // a bogus plugin path it must surface a structured error rather than panic.
    let (handle, _bridge_handle, _server_thread) = handle_with_mock_server(|_| None);
    assert!(
        handle
            .open_editor(
                &main_thread(),
                tutti_fixtures::StandInWindow,
                tutti_plugin_types::ContentScale::IDENTITY
            )
            .is_err()
    );
}
