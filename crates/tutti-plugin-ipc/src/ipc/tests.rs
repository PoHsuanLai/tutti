//! [`Ipc`] over a mock `plugin-server`: what its three halves answer, without a
//! plugin format or a real server. The differential test against the served
//! node, and the real-server suites, are `tutti-plugin`'s.

use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tutti_fixtures::{machine_lock, main_thread};
use tutti_plugin_types::{
    ChannelLayout, Control, Controller, Loaded, LoadedPlugin, MaxBlock, ParamId, ParameterInfo,
    PluginBus, PluginDescriptor, Prepare, Processor, Reopen, Request, SampleFormat, SampleRate,
    Samples,
};

use crate::{
    AudioSlab, BridgeConfig, BridgeMessage, HostMessage, Ipc, IpcController, IpcParts,
    IpcProcessor, PROTOCOL_VERSION, PluginBridge, ProcessGuard, RING_SLOTS, SlabLayout,
    unique_socket_path,
};

const CEILING: usize = 64;
const RATE: SampleRate = SampleRate::SR_48K;
/// The size the mock server's editor opens at.
const MOCK_EDITOR: tutti_plugin_types::EditorSize = tutti_plugin_types::EditorSize {
    width: 640,
    height: 480,
};

/// What the mock server was sent, in order.
type Received = Arc<Mutex<Vec<HostMessage>>>;

/// A served plugin over a mock server, which records what it is sent, answers
/// the asks the tests make, and sends what a test injects.
struct Mock {
    received: Received,
    stream: Arc<interprocess::local_socket::Stream>,
    _bridge: crate::BridgeThread,
}

impl Mock {
    /// The server says something unprompted, as a plugin does.
    fn say(&self, message: &BridgeMessage) {
        use std::io::Write;
        let data = postcard::to_allocvec(message).unwrap();
        let mut stream = &*self.stream;
        stream
            .write_all(&(data.len() as u32).to_be_bytes())
            .unwrap();
        stream.write_all(&data).unwrap();
    }

    /// Waits for `want` messages of the server's record to satisfy `found`.
    fn wait_for(&self, found: impl Fn(&[HostMessage]) -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            if found(&self.received.lock().unwrap()) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }
}

fn no_editor(
    _: &tutti_plugin_types::MainThread,
    path: &std::path::Path,
    _: &tutti_plugin_types::Load,
) -> tutti_plugin_types::BridgeResult<Box<dyn tutti_plugin_types::Editor>> {
    Err(tutti_plugin_types::BridgeError::LoadFailed {
        path: path.to_path_buf(),
        stage: tutti_plugin_types::LoadStage::Opening,
        reason: "no editor in these tests".into(),
    })
}

fn mock() -> (Ipc, Mock) {
    mock_loading(
        tutti_plugin_types::Origin::new("test.vst3", tutti_plugin_types::Load::new()),
        no_editor,
    )
}

/// [`mock`] whose bridge was made for `origin` and loads its editor with
/// `load_editor`.
fn mock_loading(
    origin: tutti_plugin_types::Origin,
    load_editor: crate::EditorLoader,
) -> (Ipc, Mock) {
    mock_with(origin, Some(load_editor))
}

/// [`mock`] whose bridge has no editor loader: its editor lives in the server.
fn mock_remote() -> (Ipc, Mock) {
    mock_with(
        tutti_plugin_types::Origin::new("test.clap", tutti_plugin_types::Load::new()),
        None,
    )
}

fn mock_with(
    origin: tutti_plugin_types::Origin,
    load_editor: Option<crate::EditorLoader>,
) -> (Ipc, Mock) {
    use interprocess::local_socket::{ListenerOptions, traits::Listener as _};
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let layout = SlabLayout {
        samples_per_channel: CEILING,
        format: SampleFormat::Float32,
        slots: RING_SLOTS as u32,
        inputs: vec![ChannelLayout::STEREO],
        outputs: vec![ChannelLayout::STEREO],
    };
    let shm = format!(
        "ipc_{}_{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let path = unique_socket_path();
    let _ = std::fs::remove_file(&path);
    let listener = ListenerOptions::new()
        .name(crate::socket_name(&path).unwrap())
        .create_sync()
        .unwrap();
    let slab = AudioSlab::create(shm, layout).unwrap();
    let (bridge, thread) = PluginBridge::new(
        path.clone(),
        Arc::clone(slab.slab()),
        origin,
        load_editor,
        RATE,
    )
    .unwrap();
    // The audio connection first, then the control one, as the server accepts
    // them. Blocks and resets arrive on the first and are only logged.
    let audio = Arc::new(listener.accept().unwrap());
    let reply = |stream: &interprocess::local_socket::Stream, message: &BridgeMessage| {
        let data = postcard::to_allocvec(message).unwrap();
        let mut stream = stream;
        stream
            .write_all(&(data.len() as u32).to_be_bytes())
            .unwrap();
        stream.write_all(&data).unwrap();
    };
    // The host connects the control stream only once it has the audio one's
    // `Ready`.
    reply(
        &audio,
        &BridgeMessage::Ready {
            protocol_version: PROTOCOL_VERSION,
        },
    );
    let stream = Arc::new(listener.accept().unwrap());
    reply(
        &stream,
        &BridgeMessage::Ready {
            protocol_version: PROTOCOL_VERSION,
        },
    );
    let received: Received = Arc::default();
    std::thread::spawn({
        let (log, audio) = (Arc::clone(&received), Arc::clone(&audio));
        move || {
            loop {
                let mut len = [0u8; 4];
                let mut stream = &*audio;
                if stream.read_exact(&mut len).is_err() {
                    break;
                }
                let mut bytes = vec![0u8; u32::from_be_bytes(len) as usize];
                if stream.read_exact(&mut bytes).is_err() {
                    break;
                }
                let Ok(message) = postcard::from_bytes::<HostMessage>(&bytes) else {
                    break;
                };
                log.lock().unwrap().push(message);
            }
        }
    });
    let (log, served) = (Arc::clone(&received), Arc::clone(&stream));
    std::thread::spawn(move || {
        loop {
            let mut len = [0u8; 4];
            let mut stream = &*served;
            if stream.read_exact(&mut len).is_err() {
                break;
            }
            let mut bytes = vec![0u8; u32::from_be_bytes(len) as usize];
            if stream.read_exact(&mut bytes).is_err() {
                break;
            }
            let Ok(message) = postcard::from_bytes::<HostMessage>(&bytes) else {
                break;
            };
            match &message {
                HostMessage::SetSampleRate { rate } => reply(
                    &served,
                    &BridgeMessage::SampleRateSet {
                        rate: *rate,
                        error: None,
                    },
                ),
                // The server's editor: a parent of 0 is a window it cannot embed
                // into; any other opens at the size the test pins.
                HostMessage::OpenEditor { parent, .. } => reply(
                    &served,
                    &BridgeMessage::EditorOpened {
                        result: match parent {
                            Some(0) => Err("the mock plugin has no editor".to_string()),
                            Some(_) => Ok(Some(MOCK_EDITOR)),
                            None => Ok(None),
                        },
                    },
                ),
                HostMessage::SetEditorSize { size } => {
                    reply(&served, &BridgeMessage::EditorSized { result: Ok(*size) })
                }
                HostMessage::GetParameterList => reply(
                    &served,
                    &BridgeMessage::ParameterList {
                        parameters: vec![ParameterInfo::new(ParamId::new(3), "gain")],
                    },
                ),
                _ => {}
            }
            log.lock().unwrap().push(message);
        }
        let _ = std::fs::remove_file(&path);
    });
    std::thread::sleep(Duration::from_millis(50));
    let ipc = Ipc::new(IpcParts {
        bridge,
        guard: Arc::new(ProcessGuard::for_test(BridgeConfig::default())),
        descriptor: PluginDescriptor::default(),
        loaded: LoadedPlugin {
            inputs: vec![PluginBus::main(ChannelLayout::STEREO)],
            outputs: vec![PluginBus::main(ChannelLayout::STEREO)],
            latency_samples: Samples(5),
            ..LoadedPlugin::default()
        },
        format: SampleFormat::Float32,
        ceiling: CEILING,
        slab,
        rate: RATE,
        parameters: 1,
        timeout: Duration::from_secs(1),
        origin: None,
    });
    (
        ipc,
        Mock {
            received,
            stream,
            _bridge: thread,
        },
    )
}

fn prepare(block: usize) -> Prepare {
    Prepare::new(RATE, MaxBlock::of(Samples(block)))
}

fn activate(ipc: Ipc, block: usize) -> (IpcProcessor, IpcController) {
    match ipc.activate(&main_thread(), prepare(block)) {
        Ok(halves) => halves,
        Err(refused) => panic!("{}", refused.reason),
    }
}

/// **`IpcProcessor::chunk()` is its configured chunk**: the block the plugin
/// was activated for, never past the slab's ceiling. It is what the graph
/// node cuts a block by, so a Processor that answered the trait's default
/// (`None`) would be handed whole blocks that span chunks.
#[test]
fn the_processor_chunk_is_its_configured_chunk() {
    let _machine = machine_lock();
    let (ipc, _server) = mock();
    let (processor, _controller) = activate(ipc, 48);
    assert_eq!(processor.chunk(), NonZeroUsize::new(48));
    let (ipc, _server) = mock();
    let (past, _controller) = activate(ipc, 1000);
    assert_eq!(
        past.chunk(),
        NonZeroUsize::new(CEILING),
        "the slab's ceiling"
    );
}

/// **A Processor follows the graph without a restart**: `retune` settles the
/// chunk from the device quantum when there is one (the block otherwise), the
/// latency it adds is that chunk, and the server is told the rate by a queued
/// command. Taken off the main thread's proof, which is the point.
#[test]
fn a_processor_follows_the_graphs_rate_block_and_quantum() {
    let _machine = machine_lock();
    let (ipc, server) = mock();
    let received = Arc::clone(&server.received);
    let (mut processor, _controller) = activate(ipc, 48);
    assert!(processor.retune(SampleRate::SR_96K, Samples(60), Some(Samples(40))));
    assert_eq!(processor.chunk(), NonZeroUsize::new(40), "the quantum");
    assert_eq!(processor.latency(), Samples(40), "one chunk of latency");
    assert!(processor.retune(SampleRate::SR_96K, Samples(30), None));
    assert_eq!(processor.chunk(), NonZeroUsize::new(30), "the block");
    let start = Instant::now();
    while !received
        .lock()
        .unwrap()
        .iter()
        .any(|m| matches!(m, HostMessage::SetSampleRate { rate } if *rate == SampleRate::SR_96K))
    {
        assert!(start.elapsed() < Duration::from_secs(5), "the rate is sent");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// **Only the two halves of one activation rejoin.** A pair from two
/// activations is refused with both halves back (`Stuck`); the pair that was
/// split rejoins, and can be activated again.
#[test]
fn only_the_halves_of_one_activation_rejoin() {
    let _machine = machine_lock();
    let main = main_thread();
    let ((first, _a), (second, _b)) = (mock(), mock());
    let (first_p, first_c) = activate(first, 32);
    let (second_p, second_c) = activate(second, 32);
    let Err(stuck) = Ipc::deactivate(&main, first_p, second_c) else {
        panic!("a pair from two activations must not rejoin");
    };
    let (first_p, second_c) = (stuck.processor, stuck.controller);
    let again = Ipc::deactivate(&main, first_p, first_c).expect("one activation rejoins");
    assert!(again.activate(&main, prepare(32)).is_ok());
    drop((second_p, second_c));
}

/// **The controller reports what the plugin re-reports, and the host hears
/// it.** The server says the latency changed: the controller's latency is the
/// plugin's new figure (the pipeline's chunk is the Processor's), the
/// lifecycle is asked to re-read it (a `Request::LatencyChanged`, once), and a
/// host that forwarded the bridge's events gets the event too. Parameters and
/// the descriptor are the bridge's and the loaded plugin's, with no main
/// thread's work in them.
#[test]
fn the_controller_reports_what_the_plugin_re_reports() {
    let _machine = machine_lock();
    let (ipc, server) = mock();
    let heard = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&heard);
    ipc.forward_events(Some(Arc::new(move |event| {
        sink.lock().unwrap().push(format!("{event:?}"));
    })));
    let say = |latency| server.say(&BridgeMessage::LatencyChanged { samples: latency });
    let main = main_thread();
    let (_processor, mut controller) = activate(ipc, 32);
    assert_eq!(controller.latency(&main), Samples(5));
    say(Samples(77));
    // An idle bridge reads nothing: the next command's reply reads it.
    assert_eq!(controller.parameters(&main).len(), 1);
    let start = Instant::now();
    while controller.latency(&main) != Samples(77) {
        assert!(start.elapsed() < Duration::from_secs(5), "latency moves");
        std::thread::sleep(Duration::from_millis(5));
    }
    let mut asked = Vec::new();
    controller.take_requests(&main, &mut asked);
    assert_eq!(asked, [Request::LatencyChanged]);
    controller.take_requests(&main, &mut asked);
    assert_eq!(asked.len(), 1, "once");
    assert!(
        heard
            .lock()
            .unwrap()
            .iter()
            .any(|e| e.contains("LatencyChanged"))
    );
    assert!(server.wait_for(|got| {
        got.iter()
            .any(|m| matches!(m, HostMessage::GetParameterList))
    }));
}

/// **A served plugin with no origin cannot be reopened, and says so**, through
/// both the token-free reopen and the trait's.
#[test]
fn a_plugin_with_no_origin_is_not_reopened() {
    let _machine = machine_lock();
    let (ipc, _server) = mock();
    let err = ipc.reopen_restoring(None).expect_err("no origin");
    assert!(err.to_string().contains("no origin"), "{err}");
    assert!(ipc.reopen(&main_thread()).is_err());
}

/// The `Load` the last [`recording_editor`] was given.
static EDITOR_LOAD: Mutex<Option<tutti_plugin_types::Load>> = Mutex::new(None);

fn recording_editor(
    _: &tutti_plugin_types::MainThread,
    path: &std::path::Path,
    load: &tutti_plugin_types::Load,
) -> tutti_plugin_types::BridgeResult<Box<dyn tutti_plugin_types::Editor>> {
    *EDITOR_LOAD.lock().unwrap() = Some(load.clone());
    Err(tutti_plugin_types::BridgeError::LoadFailed {
        path: path.to_path_buf(),
        stage: tutti_plugin_types::LoadStage::Opening,
        reason: "records the choice and loads nothing".into(),
    })
}

/// **The editor-only instance loads the plugin the audio instance is**, not
/// the file's first: the bridge hands its loader the `Load` it was made for.
/// It needs no display (the floating open takes no window), because the loader
/// is asked before any editor exists; a real editor's identity cannot be read
/// without one, so the choice is pinned where it is made.
#[test]
fn the_editor_instance_is_loaded_with_the_choice_the_bridge_was_made_for() {
    let _machine = machine_lock();
    let chosen = tutti_plugin_types::Load::new().plugin("the.second.plugin");
    let (ipc, _peer) = mock_loading(
        tutti_plugin_types::Origin::new("bundle.clap", chosen.clone()),
        recording_editor,
    );
    let refused = ipc.bridge().open_floating_editor(&main_thread());
    assert!(refused.is_err(), "the recording loader loads no editor");
    assert_eq!(*EDITOR_LOAD.lock().unwrap(), Some(chosen));
}

/// A window number the mock's X11-style server takes.
#[cfg(all(unix, not(target_os = "macos")))]
fn host_window(raw: u64) -> tutti_plugin_types::WindowHandle {
    tutti_plugin_types::WindowHandle::from_remote(raw).expect("an X11 id embeds across processes")
}

/// **A client with no editor loader opens, resizes and closes its editor over
/// the wire**: the open carries the host window's number and scale and answers
/// with the server's size, the resize is the server's answer, and the close
/// is a message with no editor in this process to load.
#[test]
#[cfg(all(unix, not(target_os = "macos")))]
fn a_remote_editor_opens_resizes_and_closes_over_the_wire() {
    use tutti_plugin_types::{ContentScale, EditorSize, EditorWindow};
    let _machine = machine_lock();
    let (mut ipc, server) = mock_remote();
    let main = main_thread();
    let scale = ContentScale::new(2.0).expect("a scale");

    let size = ipc
        .open_editor(&main, host_window(0x40_0001), scale)
        .expect("the server opens it");
    assert_eq!(size, MOCK_EDITOR);
    let wanted = EditorSize {
        width: 801,
        height: 601,
    };
    assert_eq!(ipc.set_editor_size(&main, wanted).expect("resized"), wanted);
    ipc.close_editor(&main);

    assert!(server.wait_for(|got| matches!(
        got,
        [
            HostMessage::OpenEditor { parent: Some(0x40_0001), scale: s },
            HostMessage::SetEditorSize { size },
            HostMessage::CloseEditor,
        ] if *s == scale && *size == wanted
    )));
}

/// **A floating editor is the server's own window**: no parent crosses the
/// wire, and no size comes back.
#[test]
fn a_remote_floating_editor_sends_no_parent() {
    use tutti_plugin_types::EditorWindow;
    let _machine = machine_lock();
    let (mut ipc, server) = mock_remote();
    ipc.open_floating_editor(&main_thread()).expect("opens");
    assert!(server.wait_for(|got| matches!(got, [HostMessage::OpenEditor { parent: None, .. }])));
}

/// **The server's refusal is the editor's error, in the server's words.**
#[test]
#[cfg(all(unix, not(target_os = "macos")))]
fn a_refused_remote_open_carries_the_servers_reason() {
    use tutti_plugin_types::{ContentScale, EditorWindow};
    let _machine = machine_lock();
    let (mut ipc, _server) = mock_remote();
    let refused = ipc
        .open_editor(&main_thread(), host_window(0), ContentScale::IDENTITY)
        .expect_err("the mock refuses a window 0");
    assert!(
        refused
            .to_string()
            .contains("the mock plugin has no editor"),
        "{refused}"
    );
}

/// **A size the plugin asks its window to take reaches the host's poll, once,
/// and the server closing its editor is heard**: the bridge reads both off the
/// stream with the next reply, and the editor is closed, so a resize is
/// refused until it is opened again.
#[test]
#[cfg(all(unix, not(target_os = "macos")))]
fn what_the_servers_editor_says_unprompted_is_heard() {
    use tutti_plugin_types::{ContentScale, EditorSize, EditorWindow};
    let _machine = machine_lock();
    let (mut ipc, server) = mock_remote();
    let main = main_thread();
    ipc.open_editor(&main, host_window(0x40_0001), ContentScale::IDENTITY)
        .expect("opens");
    let asked = EditorSize {
        width: 1024,
        height: 768,
    };
    server.say(&BridgeMessage::EditorWantsSize { size: asked });
    server.say(&BridgeMessage::EditorClosed);
    // An idle bridge reads nothing: the next command's reply reads both.
    assert_eq!(ipc.bridge().parameters().expect("answered").len(), 1);

    assert_eq!(ipc.poll_editor_resize_request(&main), Some(asked));
    assert_eq!(ipc.poll_editor_resize_request(&main), None, "once");
    let refused = ipc
        .set_editor_size(&main, asked)
        .expect_err("the server closed it");
    assert!(refused.to_string().contains("not open"), "{refused}");
    // A close of an editor the server already closed sends nothing: asked
    // after it, in order, a reply the mock answers proves what it was sent.
    ipc.close_editor(&main);
    assert_eq!(ipc.bridge().parameters().expect("answered").len(), 1);
    assert!(
        !server
            .received
            .lock()
            .unwrap()
            .iter()
            .any(|m| matches!(m, HostMessage::CloseEditor)),
        "no close for an editor that is not open"
    );
}

/// **A client with a loader keeps its editor in the host**: nothing about the
/// editor crosses the wire.
#[test]
fn a_client_with_a_loader_sends_no_editor_message() {
    use tutti_plugin_types::EditorWindow;
    let _machine = machine_lock();
    let (mut ipc, server) = mock();
    assert!(
        ipc.open_floating_editor(&main_thread()).is_err(),
        "no editor in these tests"
    );
    assert_eq!(ipc.bridge().parameters().expect("answered").len(), 1);
    assert!(server.wait_for(|got| {
        got.iter()
            .any(|m| matches!(m, HostMessage::GetParameterList))
    }));
    assert!(
        !server
            .received
            .lock()
            .unwrap()
            .iter()
            .any(|m| matches!(m, HostMessage::OpenEditor { .. })),
        "the editor is the host's"
    );
}
