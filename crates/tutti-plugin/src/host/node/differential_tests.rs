//! **A migration aid, deleted with the old served node in PR 3b.** The served
//! node against the generic node over [`Ipc`]: the same schedule through the
//! same mock `plugin-server`, and the audio and everything the server receives
//! compared bit for bit (design 021 step 4b, PR 3a). It compares one render
//! against another, which the repository's testing rules do not allow of a
//! lasting test: what keeps `PluginNode<Ipc>` honest after the old node is gone
//! is the analytic pins of `ipc_node_tests`.
//!
//! Two nodes, each with a mock server of its own that records every
//! audio-path frame it is sent (`ProcessAudio`, `SetSampleRate`, `Reset`) as
//! the raw bytes on the wire, and answers each chunk as a doubling plugin that
//! echoes the chunk's MIDI back as its MIDI-out. The old node is
//! `PluginClient<Bound>` (this crate's `graph_node.rs`); the new one is
//! `tutti_plugin_host::PluginNode<Ipc>`. Both wait offline for each chunk, so
//! the audio is a function of the input and not of scheduling.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tutti_fixtures::{machine_lock, midi};
use tutti_graph::contract::Direct;
use tutti_graph::{Env, Event, Harmony, Node, ParamRamp, PlayClock, Transport, TransportChanges};
use tutti_plugin_host::{InvalidateSink, ParameterChangeSink, PluginControls, RefreshSink};
use tutti_plugin_ipc::{BridgeConfig, HostMessage, OfflineWait, ProcessAudioData, ProcessGuard};
use tutti_plugin_types::{ParamId, PluginTail, SampleFormat};
use tutti_transport::TempoReader;
use tutti_types::{Beat, Bpm, Frame, Samples};
use tutti_ump::{MidiChannel, MidiGroup};

use super::fork::Origin;
use super::mock_server::*;
use super::{Bound, PluginClient, Unbound};

/// What the old node reads: a served `PluginClient`, bound, offline.
fn old_node(rig: Rig, tempo: TempoReader) -> (Direct<PluginClient<Bound>>, Record, Threads) {
    let guard = Arc::new(ProcessGuard::for_test(BridgeConfig::default()));
    let record = Arc::clone(&rig.record);
    let threads = rig.threads;
    let client = PluginClient::<Unbound> {
        bridge: rig.bridge,
        descriptor: rig.descriptor,
        loaded: rig.loaded,
        format: SampleFormat::Float32,
        inputs: 2,
        outputs: 2,
        ceiling: CHUNK,
        controls: PluginControls::new(Samples::ZERO, PluginTail::Unknown, SR),
        param_sink: ParameterChangeSink::new(),
        refresh_sink: RefreshSink::new(),
        invalidate_sink: InvalidateSink::new(),
        process_guard: Arc::clone(&guard),
        origin: Arc::new(Origin {
            config: BridgeConfig::default(),
            plugin_path: "test.vst3".into(),
        }),
        fork_watch: None,
        params: PARAMS.iter().map(|&id| ParamId::new(id)).collect(),
        state: Unbound { slab: rig.slab },
    };
    let mut node = client.bind(tempo);
    node.state.io.set_offline_wait(Arc::new(OfflineWait::new(
        Duration::from_secs(5),
        Arc::downgrade(&guard),
    )));
    node.state.transport.go_offline();
    (Direct::new(node, SR, CHUNK), record, threads)
}

/// One block of the schedule: its length, and what its event input carries.
struct Block {
    len: usize,
    events: Vec<Event>,
    /// `reset` the node before this block.
    reset: bool,
}

/// Blocks of awkward lengths against the 128-frame chunk, with a note, two
/// parameter ramps and a step, a chord, a tempo change inside a block (the
/// map's, at beat 4.01) and a `reset` between two blocks.
fn schedule() -> Vec<Block> {
    let note = |frame, len, on: bool| {
        let (group, channel) = (MidiGroup::FIRST, MidiChannel::of(1));
        Event::midi(
            at(frame, len),
            if on {
                midi::note_on(group, channel, 60, midi::wide_velocity(100))
            } else {
                midi::note_off(group, channel, 60, midi::wide_velocity(0))
            },
        )
    };
    let ramp = |frame, len, id, to, over| {
        Event::ramp(at(frame, len), ParamRamp::foreign(id, to, Samples(over)))
    };
    vec![
        Block {
            len: 100,
            events: vec![
                note(10, 100, true),
                ramp(20, 100, PARAMS[0], 0.5, 30),
                ramp(90, 100, PARAMS[1], 0.25, 0),
            ],
            reset: false,
        },
        Block {
            len: 37,
            events: vec![ramp(0, 37, PARAMS[0], 0.75, 20), note(5, 37, false)],
            reset: false,
        },
        Block {
            len: 128,
            events: vec![
                Event::harmony(at(12, 128), Harmony::chord(0, 0, 0b1001_0001)),
                note(60, 128, true),
                ramp(100, 128, PARAMS[1], 0.9, 40),
            ],
            reset: false,
        },
        Block {
            len: 90,
            events: vec![note(0, 90, false), note(89, 90, true)],
            reset: false,
        },
        Block {
            len: 7,
            events: vec![],
            reset: false,
        },
        Block {
            len: 100,
            events: vec![note(33, 100, true), ramp(50, 100, PARAMS[0], 0.1, 10)],
            reset: true,
        },
        Block {
            len: 128,
            events: vec![note(1, 128, false)],
            reset: false,
        },
        Block {
            len: 65,
            events: vec![],
            reset: false,
        },
        Block {
            len: 128,
            events: vec![],
            reset: false,
        },
    ]
}

/// What a node did: per block, the bits of its two output channels and its
/// MIDI-out.
type Heard = Vec<(Vec<u32>, Vec<u32>, String)>;

fn play<N: Node>(unit: &mut Direct<N>, blocks: &[Block]) -> Heard {
    let mut frame = 0u64;
    let mut heard = Vec::new();
    for block in blocks {
        if block.reset {
            unit.node.reset();
        }
        let env = Env {
            frame: Frame(frame),
            sample_rate: SR,
            block_len: Samples(block.len),
            transport: Transport::new(
                true,
                Bpm::of(if frame >= STEP_FRAME { 90.0 } else { 120.0 }),
                Beat(beat_at(frame)),
                None,
            ),
            changes: TransportChanges::NONE,
            play: PlayClock::ZERO,
        };
        for (i, s) in unit.inputs_mut()[0].iter_mut().enumerate().take(block.len) {
            *s = 1.0 + (frame as usize + i) as f32 * 0.001;
        }
        for (i, s) in unit.inputs_mut()[1].iter_mut().enumerate().take(block.len) {
            *s = -1.0 - (frame as usize + i) as f32 * 0.002;
        }
        unit.events(0, &block.events);
        unit.block_in(&env);
        let bits = |c: usize| {
            unit.output(c)[..block.len]
                .iter()
                .map(|s| s.to_bits())
                .collect::<Vec<_>>()
        };
        heard.push((bits(0), bits(1), format!("{:?}", unit.events_out(0))));
        frame += block.len as u64;
    }
    heard
}

/// Waits until the server has stopped receiving: the chunk the last block
/// completed is on its way to it asynchronously.
fn settled(a: &Record, b: &Record) {
    let (mut last, mut since) = ((0, 0), Instant::now());
    while since.elapsed() < Duration::from_millis(300) {
        let now = (a.lock().unwrap().len(), b.lock().unwrap().len());
        if now != last {
            (last, since) = (now, Instant::now());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// **Migration aid: delete with the old served node (PR 3b).** The generic
/// node over `Ipc` sends the server exactly what the served
/// node sends, and renders the same audio. The same schedule (blocks of
/// 100, 37, 128, 90, 7, 100, 128, 65 and 128 frames against a 128-frame
/// chunk, a note on and off, parameter ramps and a step on two parameters, a
/// chord, a tempo step inside a block, a `reset` between two blocks) runs
/// through both, each against a mock server of its own. Compared bit for
/// bit: the audio of every block, the node's MIDI-out (the server echoes the
/// chunk's MIDI back), and every audio-path frame the server received, as the
/// raw bytes of the wire (frames, MIDI, parameter queues, harmony, the
/// transport at the chunk's first frame and its changes, the rate and the
/// reset).
///
/// What the schedule leaves out and why is in the doc 021 §10.4 "PR 3a as
/// built": a tempo **ramp** and a transport stop or seek landing between two
/// blocks inside a chunk (the node reads each piece apart, so the slope over a
/// chunk and a break between pieces are not the served node's).
///
/// Mutation (run): `Piece::begin` in `tutti-plugin-ipc`'s `ipc/processor.rs`
/// putting the transport's `continuous_samples` one frame ahead -> every
/// chunk's wire bytes differ -> fails. Mutation (run): `Piece::take` dropping
/// the `at` shift of the MIDI offsets -> the notes of a continued chunk
/// arrive early -> fails.
#[test]
fn migration_old_and_new_served_nodes_agree() {
    let _machine = machine_lock();
    // The step lands inside the third block.
    let blocks = schedule();
    let (mut old, old_record, _old_threads) = old_node(rig("old"), tempo(STEP_BEAT));
    let (mut new, new_record, _new_threads) = new_node(rig("new"), tempo(STEP_BEAT));

    let old_heard = play(&mut old, &blocks);
    let new_heard = play(&mut new, &blocks);
    settled(&old_record, &new_record);

    for (i, (a, b)) in old_heard.iter().zip(&new_heard).enumerate() {
        assert_eq!(a.0, b.0, "block {i}: channel 0");
        assert_eq!(a.1, b.1, "block {i}: channel 1");
        assert_eq!(a.2, b.2, "block {i}: MIDI-out");
    }
    let (old_sent, new_sent) = (
        old_record.lock().unwrap().clone(),
        new_record.lock().unwrap().clone(),
    );
    assert_eq!(old_sent.len(), new_sent.len(), "frames received");
    for (i, (a, b)) in old_sent.iter().zip(&new_sent).enumerate() {
        if a != b {
            let show = |bytes: &[u8]| match bincode::deserialize::<HostMessage>(bytes) {
                Ok(HostMessage::ProcessAudio(d)) => format!(
                    "seq {} n {} midi {:?} params {:?} chords {:?} transport {:?} changes {:?}",
                    d.seq,
                    d.num_samples,
                    d.midi_events,
                    d.param_changes.queues,
                    d.chords,
                    d.transport,
                    d.transport_changes
                ),
                other => format!("{other:?}"),
            };
            panic!(
                "the server's frame {i} differs:\n old {}\n new {}",
                show(a),
                show(b)
            );
        }
    }

    // The schedule must exercise what it says, or the comparison is vacuous.
    let chunks: Vec<ProcessAudioData> = new_sent
        .iter()
        .filter_map(|bytes| match bincode::deserialize(bytes) {
            Ok(HostMessage::ProcessAudio(data)) => Some(data),
            _ => None,
        })
        .collect();
    assert!(chunks.len() >= 5, "{} chunks", chunks.len());
    assert!(chunks.iter().any(|c| !c.midi_events.is_empty()), "MIDI");
    assert!(
        chunks
            .iter()
            .any(|c| c.param_changes.queues.iter().any(|q| !q.points.is_empty())),
        "parameter points"
    );
    assert!(chunks.iter().any(|c| !c.chords.is_empty()), "a chord");
    assert!(
        chunks.iter().any(|c| !c.transport_changes.is_empty()),
        "a transport change (the tempo step)"
    );
    assert!(
        new_sent
            .iter()
            .any(|b| matches!(bincode::deserialize(b), Ok(HostMessage::Reset))),
        "the reset"
    );
    assert!(
        new_sent.iter().any(|b| matches!(
            bincode::deserialize(b),
            Ok(HostMessage::SetSampleRate { .. })
        )),
        "the rate"
    );
}

/// **Migration aid (deleted with the old served node in PR 3b): a stop, a seek
/// and a tempo ramp across a block boundary inside a chunk reach the server
/// from the new node exactly as from the old.** Blocks of 100, 60, 128 and 128
/// frames against a chunk of 128; the third block (frame 160, chunk frame 32)
/// stopped, or moved to beat 10, or, in the third case, rolling on under a
/// tempo ramp (a map ramping 120 to 90 BPM over 100 beats, with the playhead
/// following it). Audio and every frame the server is sent, bit for bit.
///
/// Mutation (run): `Piece::payload` not copying the context's transport
/// changes -> the stop and seek cases send none, the old node's change at 32
/// is missing -> fails.
#[test]
fn migration_old_and_new_agree_on_stops_seeks_and_ramps() {
    use tutti_tempo::{Shape, TempoMap, TempoPoint};
    let _machine = machine_lock();
    let ramp_map = || {
        let point = |beat: f64, bpm: f64, shape| TempoPoint {
            beat: Beat(beat),
            tempo: Bpm::of(bpm),
            shape,
        };
        TempoReader::fixed(Arc::new(
            TempoMap::new(vec![
                point(0.0, 120.0, Shape::RampTime),
                point(100.0, 90.0, Shape::Step),
            ])
            .expect("a map"),
        ))
    };
    let (duration, rate) = (120.0 * 100.0 / 210.0, 48_000.0);
    let k = -30.0 / duration;
    let on_ramp = move |frame: u64| {
        let t = frame as f64 / rate;
        (
            Bpm::of(120.0 + k * t),
            Beat((120.0 * t + k * t * t / 2.0) / 60.0),
        )
    };
    let flat = |frame: u64| {
        (
            Bpm::of(120.0),
            Beat(4.0 + frame as f64 * BEATS_PER_FRAME_120),
        )
    };
    type Third = fn(u64) -> (bool, Bpm, Beat);
    let cases: [(&str, bool, Third); 3] = [
        ("stop", false, |f| {
            (
                false,
                Bpm::of(120.0),
                Beat(4.0 + f as f64 * BEATS_PER_FRAME_120),
            )
        }),
        ("seek", false, |_| (true, Bpm::of(120.0), Beat(10.0))),
        ("ramp", true, |_| (true, Bpm::of(120.0), Beat(0.0))),
    ];
    for (label, ramped, third) in cases {
        let env = |frame: u64, len: usize| {
            let (playing, bpm, beat) = if frame == 160 && !ramped {
                third(frame)
            } else {
                let (bpm, beat) = if ramped { on_ramp(frame) } else { flat(frame) };
                (true, bpm, beat)
            };
            Env {
                frame: Frame(frame),
                sample_rate: SR,
                block_len: Samples(len),
                transport: Transport::new(playing, bpm, beat, None),
                changes: TransportChanges::NONE,
                play: PlayClock::ZERO,
            }
        };
        let map = || if ramped { ramp_map() } else { tempo(1000.0) };
        let (mut old, old_record, _a) = old_node(rig(label), map());
        let (mut new, new_record, _b) = new_node(rig(label), map());
        let run = |unit: &mut dyn FnMut(&Env)| {
            let mut frame = 0u64;
            for len in [100usize, 60, 128, 128] {
                unit(&env(frame, len));
                frame += len as u64;
            }
        };
        let (mut old_heard, mut new_heard) = (Vec::new(), Vec::new());
        run(&mut |e| {
            for (i, s) in old.inputs_mut()[0].iter_mut().enumerate() {
                *s = 1.0 + (e.frame.0 as usize + i) as f32 * 0.001;
            }
            old.block_in(e);
            old_heard.push(old.output(0)[..e.block_len.get()].to_vec());
        });
        run(&mut |e| {
            for (i, s) in new.inputs_mut()[0].iter_mut().enumerate() {
                *s = 1.0 + (e.frame.0 as usize + i) as f32 * 0.001;
            }
            new.block_in(e);
            new_heard.push(new.output(0)[..e.block_len.get()].to_vec());
        });
        settled(&old_record, &new_record);
        assert_eq!(old_heard, new_heard, "{label}: audio");
        let (a, b) = (
            old_record.lock().unwrap().clone(),
            new_record.lock().unwrap().clone(),
        );
        assert_eq!(a.len(), b.len(), "{label}: frames received");
        assert!(a.len() >= 3, "{label}: {} frames", a.len());
        for (i, (x, y)) in a.iter().zip(&b).enumerate() {
            assert_eq!(x, y, "{label}: the server's frame {i}");
        }
        if label != "ramp" {
            let changes = a
                .iter()
                .filter_map(|bytes| match bincode::deserialize(bytes) {
                    Ok(HostMessage::ProcessAudio(d)) => Some(d.transport_changes.len()),
                    _ => None,
                })
                .sum::<usize>();
            assert!(
                changes >= 1,
                "{label}: the break is a change, as the old node sent it"
            );
        }
    }
}
