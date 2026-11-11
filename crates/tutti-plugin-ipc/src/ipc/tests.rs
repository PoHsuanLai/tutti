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
const RATE: SampleRate = SampleRate(48_000.0);

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
) -> tutti_plugin_types::BridgeResult<Box<dyn tutti_plugin_types::Editor>> {
    Err(tutti_plugin_types::BridgeError::LoadFailed {
        path: path.to_path_buf(),
        stage: tutti_plugin_types::LoadStage::Opening,
        reason: "no editor in these tests".into(),
    })
}

fn mock() -> (Ipc, Mock) {
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
        std::path::PathBuf::from("test.vst3"),
        no_editor,
        RATE,
    )
    .unwrap();
    let stream = Arc::new(listener.accept().unwrap());
    let reply = |stream: &interprocess::local_socket::Stream, message: &BridgeMessage| {
        let data = postcard::to_allocvec(message).unwrap();
        let mut stream = stream;
        stream
            .write_all(&(data.len() as u32).to_be_bytes())
            .unwrap();
        stream.write_all(&data).unwrap();
    };
    reply(
        &stream,
        &BridgeMessage::Ready {
            protocol_version: PROTOCOL_VERSION,
        },
    );
    let received: Received = Arc::default();
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
    Prepare::new(RATE, MaxBlock::of(Samples(block))).expect("a rate")
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
    assert!(processor.retune(SampleRate(96_000.0), Samples(60), Some(Samples(40))));
    assert_eq!(processor.chunk(), NonZeroUsize::new(40), "the quantum");
    assert_eq!(processor.latency(), Samples(40), "one chunk of latency");
    assert!(processor.retune(SampleRate(96_000.0), Samples(30), None));
    assert_eq!(processor.chunk(), NonZeroUsize::new(30), "the block");
    let start = Instant::now();
    while !received
        .lock()
        .unwrap()
        .iter()
        .any(|m| matches!(m, HostMessage::SetSampleRate { rate } if *rate == SampleRate(96_000.0)))
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
