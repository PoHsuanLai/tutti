//! A mock `plugin-server` for `tutti-plugin`'s node tests: a doubling plugin
//! that echoes each chunk's MIDI as its MIDI-out, records the audio-path frames
//! it is sent as the raw bytes of the wire, and the helpers that build the
//! generic node over [`Ipc`] against it.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tutti_fixtures::main_thread;
use tutti_graph::Offset;
use tutti_graph::contract::Direct;
use tutti_plugin_host::client_over;
use tutti_plugin_ipc::{
    AudioSlab, BridgeConfig, BridgeMessage, HostMessage, Ipc, IpcParts, PROTOCOL_VERSION,
    PluginBridge, ProcessAudioData, ProcessGuard, RING_SLOTS, SlabLayout, SlabWriter,
    unique_socket_path,
};
use tutti_plugin_types::{
    ChannelLayout, EditorPresence, Features, LoadedPlugin, ParamId, ParameterChanges,
    ParameterInfo, PluginClass, PluginDescriptor, PluginTail, SampleFormat,
};
use tutti_tempo::{Shape, TempoMap, TempoPoint};
use tutti_transport::TempoReader;
use tutti_types::{Beat, Bpm, SampleRate, Samples};

pub(super) const SR: SampleRate = SampleRate(48_000.0);
/// The slab's chunk ceiling, and the block the graph is prepared for: the
/// pipeline's chunk is this, as no quantum is given.
pub(super) const CHUNK: usize = 128;
/// The tempo map's step, in beats, and the frame it lands on at 120 BPM.
pub(super) const STEP_BEAT: f64 = 4.01;
pub(super) const STEP_FRAME: u64 = 240;
/// Beats per frame at 120 BPM and at 90 BPM, at 48 kHz.
pub(super) const BEATS_PER_FRAME_120: f64 = 2.0 / 48_000.0;
pub(super) const BEATS_PER_FRAME_90: f64 = 1.5 / 48_000.0;

/// The playhead at stream frame `frame`, rolling from beat 4: at the map's
/// tempo, so a block starting where the last ended continues the stretch (a
/// playhead that did not is a seek, which the served node reports at the call
/// and this node, reading each piece apart, does not).
pub(super) fn beat_at(frame: u64) -> f64 {
    if frame <= STEP_FRAME {
        4.0 + frame as f64 * BEATS_PER_FRAME_120
    } else {
        STEP_BEAT + (frame - STEP_FRAME) as f64 * BEATS_PER_FRAME_90
    }
}
pub(super) const PARAMS: [u32; 2] = [7, 9];

/// The bytes of every audio-path frame the mock server was sent.
pub(super) type Record = Arc<Mutex<Vec<Vec<u8>>>>;

/// A bridge, its mock server, and what the server was sent.
pub(super) struct Rig {
    pub(super) bridge: Arc<PluginBridge>,
    pub(super) slab: SlabWriter,
    pub(super) record: Record,
    pub(super) descriptor: PluginDescriptor,
    pub(super) loaded: LoadedPlugin,
    pub(super) threads: Threads,
}

/// The bridge thread and the server thread: the rig lives while they do.
pub(super) struct Threads {
    _bridge: tutti_plugin_ipc::BridgeThread,
    _server: std::thread::JoinHandle<()>,
}

fn layout() -> SlabLayout {
    SlabLayout {
        samples_per_channel: CHUNK,
        format: SampleFormat::Float32,
        slots: RING_SLOTS as u32,
        inputs: vec![ChannelLayout::STEREO],
        outputs: vec![ChannelLayout::STEREO],
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

/// A frame off the socket, whole, with its decoding. `None` at EOF.
fn read_frame(stream: &interprocess::local_socket::Stream) -> Option<(Vec<u8>, HostMessage)> {
    use std::io::Read;
    let mut stream = stream;
    let mut len = [0u8; 4];
    stream.read_exact(&mut len).ok()?;
    let mut bytes = vec![0u8; u32::from_be_bytes(len) as usize];
    stream.read_exact(&mut bytes).ok()?;
    let message = bincode::deserialize(&bytes).ok()?;
    Some((bytes, message))
}

fn send(stream: &interprocess::local_socket::Stream, message: &BridgeMessage) -> bool {
    use std::io::Write;
    let mut stream = stream;
    let data = bincode::serialize(message).expect("a reply serializes");
    stream.write_all(&(data.len() as u32).to_be_bytes()).is_ok() && stream.write_all(&data).is_ok()
}

pub(super) fn rig(label: &str) -> Rig {
    use interprocess::local_socket::{ListenerOptions, traits::Listener as _};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let shm = format!(
        "diff_{label}_{}_{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let path = unique_socket_path();
    let _ = std::fs::remove_file(&path);
    let listener = ListenerOptions::new()
        .name(tutti_plugin_ipc::socket_name(&path).unwrap())
        .create_sync()
        .unwrap();
    let slab = AudioSlab::create(shm.clone(), layout()).unwrap();
    let mut server_slab = AudioSlab::open(shm, layout()).unwrap();
    let (bridge, thread) = PluginBridge::new(
        path.clone(),
        Arc::clone(slab.slab()),
        std::path::PathBuf::from("test.vst3"),
        no_editor,
        SR,
    )
    .unwrap();
    let stream = listener.accept().unwrap();
    send(
        &stream,
        &BridgeMessage::Ready {
            protocol_version: PROTOCOL_VERSION,
        },
    );
    let record: Record = Arc::default();
    let log = Arc::clone(&record);
    let server = std::thread::Builder::new()
        .name("mock-differential-server".into())
        .spawn(move || {
            let mut scratch = vec![0.0f32; CHUNK];
            while let Some((bytes, message)) = read_frame(&stream) {
                let reply = match message {
                    HostMessage::GetParameterList => Some(BridgeMessage::ParameterList {
                        parameters: PARAMS
                            .iter()
                            .map(|&id| ParameterInfo::new(ParamId::new(id), "p"))
                            .collect(),
                    }),
                    HostMessage::GetPresetList => Some(BridgeMessage::PresetList {
                        presets: Vec::new(),
                    }),
                    HostMessage::ProcessAudio(data) => {
                        log.lock().unwrap().push(bytes);
                        Some(answer(&mut server_slab, &mut scratch, data))
                    }
                    // The bridge holds every block behind a rate change until the
                    // server answers it.
                    HostMessage::SetSampleRate { rate } => {
                        log.lock().unwrap().push(bytes);
                        Some(BridgeMessage::SampleRateSet { rate, error: None })
                    }
                    HostMessage::Reset => {
                        log.lock().unwrap().push(bytes);
                        None
                    }
                    _ => None,
                };
                if let Some(reply) = reply
                    && !send(&stream, &reply)
                {
                    break;
                }
            }
            let _ = std::fs::remove_file(&path);
        })
        .unwrap();
    std::thread::sleep(Duration::from_millis(50));
    Rig {
        bridge,
        slab,
        record,
        descriptor: PluginDescriptor {
            id: "test.plugin".into(),
            name: "Test Plugin".into(),
            vendor: String::new(),
            version: String::new(),
            class: PluginClass::Unknown,
            editor: EditorPresence::Absent,
        },
        loaded: LoadedPlugin {
            inputs: vec![ChannelLayout::STEREO],
            outputs: vec![ChannelLayout::STEREO],
            latency_samples: Samples::ZERO,
            tail: PluginTail::Unknown,
            features: Features::TRANSPORT
                | Features::MIDI_OUT
                | Features::PARAM_AUTOMATION
                | Features::SEQUENCER_CONTEXT,
            ..LoadedPlugin::default()
        },
        threads: Threads {
            _bridge: thread,
            _server: server,
        },
    }
}

/// The plugin: every channel doubled, the chunk's MIDI echoed as its MIDI-out.
fn answer(slab: &mut SlabWriter, scratch: &mut [f32], data: ProcessAudioData) -> BridgeMessage {
    let n = data.num_samples;
    if slab.has_input(data.seq) {
        for ch in 0..2 {
            let got = slab
                .read_input_into(data.seq, ch, &mut scratch[..n])
                .unwrap_or(0);
            for s in &mut scratch[..got] {
                *s *= 2.0;
            }
            slab.write(data.seq, ch, &scratch[..got]).unwrap();
        }
        slab.publish(data.seq);
    }
    BridgeMessage::AudioProcessed {
        latency_us: 0,
        seq: data.seq,
        midi_out: data.midi_events,
    }
}

/// A map at 120 BPM that steps to 90 at `beat`.
pub(super) fn tempo(beat: f64) -> TempoReader {
    let point = |beat: f64, bpm: f64| TempoPoint {
        beat: Beat(beat),
        tempo: Bpm::of(bpm),
        shape: Shape::Step,
    };
    TempoReader::fixed(Arc::new(
        TempoMap::new(vec![point(0.0, 120.0), point(beat, 90.0)]).expect("a map"),
    ))
}

/// What the new node reads: the generic node over [`Ipc`], offline.
pub(super) fn new_node(
    rig: Rig,
    tempo: TempoReader,
) -> (Direct<tutti_plugin_host::PluginNode<Ipc>>, Record, Threads) {
    let guard = Arc::new(ProcessGuard::for_test(BridgeConfig::default()));
    let record = Arc::clone(&rig.record);
    let threads = rig.threads;
    let mut ipc = Ipc::new(IpcParts {
        bridge: rig.bridge,
        guard: Arc::clone(&guard),
        descriptor: rig.descriptor,
        loaded: rig.loaded,
        format: SampleFormat::Float32,
        ceiling: CHUNK,
        slab: rig.slab,
        rate: SR,
        parameters: PARAMS.len(),
        timeout: Duration::from_secs(5),
        origin: None,
    });
    let _ = ipc.wait_offline();
    let mut client = client_over(
        &main_thread(),
        ipc,
        SR,
        CHUNK,
        ParameterChanges::SPARE_QUEUES,
    )
    .expect("the served plugin activates");
    client.go_offline();
    (Direct::new(client.bind(tempo), SR, CHUNK), record, threads)
}

pub(super) fn at(frame: usize, len: usize) -> Offset {
    Offset::new(frame, Samples(len)).expect("inside the block")
}
