//! `PluginNode<Ipc>`, the generic node over a served plugin, pinned on its own
//! against analytic figures (design 021 step 4b, PR 3a): where a click comes
//! out, and what the mock `plugin-server` is sent for each chunk, derived from
//! the schedule and the chunk length alone, with no other node to compare to.
//!
//! The mock plugin doubles its input and echoes each chunk's MIDI back as its
//! MIDI-out; the chunk is [`CHUNK`] frames (no quantum is given, so it is the
//! block the graph is prepared for), counted from `prepare` and from every
//! `reset`. The node waits offline for each chunk, so the audio is a function
//! of the input and not of scheduling.

use tutti_fixtures::{machine_lock, midi};
use tutti_graph::{
    Env, Event, Harmony, Node, Offset, ParamRamp, PlayClock, Transport, TransportChanges,
};
use tutti_plugin_ipc::{HostMessage, ProcessAudioData};
use tutti_plugin_types::ParamId;
use tutti_types::{Beat, Bpm, Frame, Samples};
use tutti_ump::{MidiChannel, MidiGroup};

use super::mock_server::{
    BEATS_PER_FRAME_120, CHUNK, PARAMS, Record, SR, STEP_BEAT, STEP_FRAME, at, beat_at, new_node,
    rig, tempo,
};

/// What the server was sent, in order.
enum Sent {
    Chunk(Box<ProcessAudioData>),
    Reset,
    Rate,
}

fn sent(record: &Record) -> Vec<Sent> {
    record
        .lock()
        .unwrap()
        .iter()
        .map(
            |bytes| match postcard::from_bytes(bytes).expect("a host frame") {
                HostMessage::ProcessAudio(data) => Sent::Chunk(Box::new(data)),
                HostMessage::Reset => Sent::Reset,
                HostMessage::SetSampleRate { .. } => Sent::Rate,
                other => panic!("not an audio-path frame: {other:?}"),
            },
        )
        .collect()
}

/// Waits for the server to have been sent `chunks` chunks: the last one the
/// schedule completed is on its way asynchronously.
fn wait_for_chunks(record: &Record, chunks: usize) -> Vec<Sent> {
    let start = std::time::Instant::now();
    loop {
        let all = sent(record);
        let n = all.iter().filter(|s| matches!(s, Sent::Chunk(_))).count();
        if n >= chunks || start.elapsed() > std::time::Duration::from_secs(5) {
            return all;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// The block at stream frame `frame`, rolling from beat 4 at the map's tempo.
fn block_env(frame: u64, len: usize) -> Env {
    Env {
        frame: Frame(frame),
        sample_rate: SR,
        block_len: Samples(len),
        transport: Transport::new(
            true,
            Bpm::of(if frame >= STEP_FRAME { 90.0 } else { 120.0 }),
            Beat(beat_at(frame)),
            None,
        ),
        changes: TransportChanges::NONE,
        play: PlayClock::ZERO,
    }
}

/// **A click comes out one declared latency later, and nothing else comes
/// out.** The mock plugin doubles its input and reports no latency of its
/// own, so the node's declared latency is the pipeline's chunk: a click of 1.0
/// at stream frame 50 is a 2.0 at frame `50 + CHUNK` and every other frame of
/// the render is silence, whatever the blocks' lengths (100, 37, 128, 90, 128,
/// 128 and 128 against a chunk of 128).
///
/// Mutation (run): `IpcProcessor::latency` answering 0 (the trait default) ->
/// the declared latency is 0, not the chunk -> the first assertion fails.
/// Mutation (run): `IpcProcessor::process` handing the batcher no input
/// (`&[]`) -> the click never reaches the plugin -> the render is all
/// silence -> fails.
#[test]
fn a_click_comes_out_one_declared_latency_later() {
    let _machine = machine_lock();
    let (mut unit, _record, _threads) = new_node(rig("latency"), tempo(STEP_BEAT));
    let declared = unit.node.shape().latency.samples();
    assert_eq!(
        declared,
        Samples(CHUNK),
        "the plugin's own (none) and a chunk"
    );

    let (mut frame, mut heard) = (0u64, Vec::new());
    for len in [100usize, 37, 128, 90, 128, 128, 128] {
        for (c, ch) in unit.inputs_mut().iter_mut().enumerate() {
            for (i, s) in ch.iter_mut().enumerate().take(len) {
                *s = if c == 0 && frame + i as u64 == 50 {
                    1.0
                } else {
                    0.0
                };
            }
        }
        unit.block_in(&block_env(frame, len));
        heard.extend_from_slice(&unit.output(0)[..len]);
        frame += len as u64;
    }
    let click = 50 + declared.get();
    for (i, s) in heard.iter().enumerate() {
        let want = if i == click { 2.0 } else { 0.0 };
        assert_eq!(*s, want, "frame {i}");
    }
}

/// An absolute-frame item of the schedule below.
struct Item {
    frame: u64,
    event: fn(Offset) -> Event,
}

fn note_on(o: Offset) -> Event {
    Event::midi(
        o,
        midi::note_on(
            MidiGroup::FIRST,
            MidiChannel::of(1),
            60,
            midi::wide_velocity(100),
        ),
    )
}

fn ramp_7(o: Offset) -> Event {
    Event::ramp(o, ParamRamp::foreign(PARAMS[0], 0.5, Samples(0)))
}

fn ramp_9(o: Offset) -> Event {
    Event::ramp(o, ParamRamp::foreign(PARAMS[1], 0.25, Samples(0)))
}

fn chord(o: Offset) -> Event {
    Event::harmony(o, Harmony::chord(0, 0, 0b1001_0001))
}

/// **What the server is sent for each chunk is the schedule's, on the chunk's
/// frames.** Blocks of 100, 37, 128 and 90 frames (355 in all), a `reset`, then
/// 100, 60 and 128, against a chunk of 128 counted from `prepare` and from the
/// reset. So the chunks are frames 0..128, 128..256 (which spans three
/// blocks), then, the 99 frames of the one in progress dropped by the reset,
/// 355..483 and 483..611. Per chunk, the server is sent:
///
/// - the **transport at the chunk's first frame**: the playhead there, and the
///   tempo (120 BPM, 90 after the map's step at frame 200);
/// - the **tempo step** as a transport change at its frame in the chunk that
///   holds it (frame 72 of the second), and in no other;
/// - the **notes** at 130 and 250 at frames 2 and 122 of the second chunk, and
///   the one at 360 at frame 5 of the third (the first after the reset);
/// - the **parameter points** (7 at frame 140 is the second chunk's frame 12;
///   9 at frame 370 is the third's 15), and the **chord** at 150 (frame 22, in the second block's piece of the chunk);
/// - a `reset` frame between the second and third chunks, and the rate once
///   at the start; sequence numbers run 1 to 4 across the reset.
///
/// And the plugin's MIDI-out (the server echoes each chunk's MIDI) plays one
/// chunk later on the frame it was emitted at: the notes of the second chunk
/// at 258 (the reset drops the one at 378), the note of the third at 488.
///
/// Mutation (run): `Piece::payload` sending the default transport -> the
/// playheads are all 0 -> fails. Mutation (run): the node taking its
/// transport snapshot per piece (see `chunked_node`) -> the second chunk, begun
/// in the block before the one that completes it, carries a later playhead ->
/// fails. Mutation (run): `IpcProcessor::reset` not resetting the
/// batcher -> the chunk phase carries over the reset, so the third chunk
/// starts at 384 and every figure after it moves -> fails. Mutation (run):
/// `Piece::take` dropping the `at` shift of the MIDI offsets -> the note at 250,
/// in the chunk's second piece, arrives at 113, not 122 -> fails. Mutation
/// (run): the same shift dropped from the chord -> the chord at 150 arrives at
/// 13, not 22 -> fails. Mutation (run): `Piece::params` not copying the node's
/// changes (`open_queue` skipped) -> no parameter points -> fails.
#[test]
fn the_server_is_sent_the_schedule_on_the_chunks_frames() {
    let _machine = machine_lock();
    // The step lands on stream frame 200, half a frame inside it.
    let (mut unit, record, _threads) =
        new_node(rig("chunks"), tempo(4.0 + 199.5 * BEATS_PER_FRAME_120));
    let items = [
        Item {
            frame: 130,
            event: note_on,
        },
        Item {
            frame: 140,
            event: ramp_7,
        },
        Item {
            frame: 150,
            event: chord,
        },
        Item {
            frame: 250,
            event: note_on,
        },
        Item {
            frame: 360,
            event: note_on,
        },
        Item {
            frame: 370,
            event: ramp_9,
        },
    ];
    let blocks = [
        (100usize, false),
        (37, false),
        (128, false),
        (90, false),
        (100, true),
        (60, false),
        (128, false),
    ];
    let (mut frame, mut out_events) = (0u64, Vec::new());
    for (len, reset) in blocks {
        if reset {
            unit.node.reset();
        }
        let events: Vec<Event> = items
            .iter()
            .filter(|i| i.frame >= frame && i.frame < frame + len as u64)
            .map(|i| (i.event)(at((i.frame - frame) as usize, len)))
            .collect();
        unit.events(0, &events);
        unit.block_in(&block_env(frame, len));
        out_events.push(
            unit.events_out(0)
                .iter()
                .map(|e| e.offset.index())
                .collect::<Vec<_>>(),
        );
        frame += len as u64;
    }

    let all = wait_for_chunks(&record, 4);
    let order: Vec<&str> = all
        .iter()
        .map(|s| match s {
            Sent::Chunk(_) => "chunk",
            Sent::Reset => "reset",
            Sent::Rate => "rate",
        })
        .collect();
    assert_eq!(
        order,
        ["rate", "chunk", "chunk", "reset", "chunk", "chunk"],
        "the rate once, and the reset between the second and third chunks"
    );
    let chunks: Vec<&ProcessAudioData> = all
        .iter()
        .filter_map(|s| match s {
            Sent::Chunk(c) => Some(&**c),
            _ => None,
        })
        .collect();
    assert_eq!(
        chunks.iter().map(|c| c.seq).collect::<Vec<_>>(),
        [1, 2, 3, 4],
        "sequence numbers run across the reset"
    );
    assert!(chunks.iter().all(|c| c.num_samples == CHUNK));

    // The chunk's first frame on the stream, and what it carries.
    let starts = [0u64, 128, 355, 483];
    let notes: [&[u32]; 4] = [&[], &[2, 122], &[5], &[]];
    for (i, (chunk, start)) in chunks.iter().zip(starts).enumerate() {
        let quarters = chunk.transport.position.quarters;
        assert!(
            (quarters - beat_at(start)).abs() < 1e-9,
            "chunk {i}: the playhead at frame {start} is {}, not {quarters}",
            beat_at(start)
        );
        let tempo = if start >= STEP_FRAME { 90.0 } else { 120.0 };
        assert_eq!(chunk.transport.timing.tempo, tempo, "chunk {i}: tempo");
        let heard: Vec<u32> = chunk.midi_events.iter().map(|m| m.offset).collect();
        assert_eq!(heard, notes[i], "chunk {i}: notes");
        let steps: Vec<(u32, f64)> = chunk
            .transport_changes
            .iter()
            .map(|c| (c.offset, c.transport.timing.tempo))
            .collect();
        let want: &[(u32, f64)] = if i == 1 { &[(72, 90.0)] } else { &[] };
        assert_eq!(steps, want, "chunk {i}: the tempo step, in its chunk only");
    }
    let points = |c: &ProcessAudioData, id: u32| -> Vec<(i32, f64)> {
        c.param_changes
            .queues
            .iter()
            .filter(|q| q.param_id == ParamId::new(id))
            .flat_map(|q| {
                q.points
                    .iter()
                    .map(|p| (p.sample_offset, f64::from(p.value)))
            })
            .collect()
    };
    assert_eq!(points(chunks[1], PARAMS[0]), [(12, 0.5)], "7 at frame 140");
    assert_eq!(points(chunks[2], PARAMS[1]), [(15, 0.25)], "9 at frame 370");
    for (i, c) in chunks.iter().enumerate() {
        let (a, b) = (points(c, PARAMS[0]), points(c, PARAMS[1]));
        let (want_a, want_b) = (usize::from(i == 1), usize::from(i == 2));
        assert_eq!(
            (a.len(), b.len()),
            (want_a, want_b),
            "chunk {i}: no other points"
        );
    }
    let chords: Vec<i32> = chunks[1]
        .chords
        .changes
        .iter()
        .map(|c| c.sample_offset)
        .collect();
    assert_eq!(chords, [22], "the chord at 150");
    assert!(
        chunks
            .iter()
            .enumerate()
            .all(|(i, c)| i == 1 || c.chords.is_empty())
    );

    // The MIDI-out: the second chunk's notes, one chunk later (258), and the
    // third's (488); the reset drops what the second would play after it.
    let want: [&[usize]; 7] = [&[], &[], &[121], &[], &[], &[33], &[]];
    for (i, (heard, want)) in out_events.iter().zip(want).enumerate() {
        assert_eq!(heard, want, "block {i}: the plugin's MIDI-out");
    }
}

/// **A served plugin reopens and restores its state off the main thread**,
/// against a real `plugin-server` and the reference CLAP plugin: the
/// token-free [`Ipc::reopen_restoring`], called on a thread that holds no
/// [`MainThread`](tutti_plugin_types::MainThread), starts a server of its own
/// (another process), hands it the saved state before it is activated, and the
/// state is taken (`Some(Ok(()))`). The reopened plugin is the same plugin and
/// renders through the generic node: a click comes out a declared latency
/// later, as the plugin's tag-added signal (`TUTTI_CLAP_PROBE_RENDER_MODE=1`
/// is the passthrough that adds its tag to channel 0; the figure is the
/// plugin's own, 1.0, over a click of 1.0).
///
/// Mutation (run): `reopen_restoring` passing `None` for the state -> the
/// outcome is `None`, not `Some(Ok(()))` -> fails. Mutation (run): the
/// reopened launch keeping the first's socket path (the `unique_socket_path`
/// line dropped) -> the second server and bridge never meet -> the reopen
/// times out -> fails.
#[cfg(feature = "clap")]
#[test]
fn a_served_plugin_reopens_and_restores_off_the_main_thread() {
    use tutti_plugin_ipc::{BridgeConfig, Ipc};
    use tutti_plugin_types::Control;
    let _machine = machine_lock();
    let mut env = tutti_fixtures::ScopedEnv::new();
    env.set("TUTTI_PLUGIN_SERVER", tutti_fixtures::plugin_server());
    let first = Ipc::launch(
        BridgeConfig::default(),
        tutti_fixtures::clap_probe().to_path_buf(),
        SR,
        None,
        crate::format::gui::load_gui_instance,
    )
    .expect("the reference plugin loads in a real server")
    .ipc;
    let state = first.bridge().save_state().expect("the state saves");
    let first_pid = first.guard().pid();
    let reopened = std::thread::spawn(move || {
        assert!(
            tutti_plugin_types::MainThread::current().is_none(),
            "no main thread here"
        );
        first.reopen_restoring(Some(&state)).map(|r| (first, r))
    })
    .join()
    .expect("the worker returns")
    .expect("the reopen needs no token");
    let (first, second) = reopened;
    assert_eq!(second.restored, Some(Ok(())), "the state was taken");
    assert_ne!(
        second.ipc.guard().pid(),
        first.guard().pid(),
        "a server of its own"
    );
    assert_ne!(first_pid, None);
    assert_eq!(second.ipc.info().id, first.info().id, "the same plugin");
}

/// The block at stream frame `frame` with its own transport state.
fn block_env_with(frame: u64, len: usize, playing: bool, bpm: Bpm, beat: Beat) -> Env {
    Env {
        transport: Transport::new(playing, bpm, beat, None),
        ..block_env(frame, len)
    }
}

/// Runs blocks of 100, 60 and 128 frames against a chunk of 128, the third
/// (stream frame 160, chunk frame 32 of the chunk begun at 128) under
/// `third`'s transport, and returns the chunk 128..256 as the server was sent
/// it.
fn chunk_across_the_third_block(
    label: &str,
    map: tutti_transport::TempoReader,
    first: impl Fn(u64) -> (Bpm, Beat),
    third: (bool, Bpm, Beat),
) -> ProcessAudioData {
    let (mut unit, record, _threads) = new_node(rig(label), map);
    for (frame, len) in [(0u64, 100usize), (100, 60)] {
        let (bpm, beat) = first(frame);
        unit.block_in(&block_env_with(frame, len, true, bpm, beat));
    }
    unit.block_in(&block_env_with(160, 128, third.0, third.1, third.2));
    let all = wait_for_chunks(&record, 2);
    let chunks: Vec<ProcessAudioData> = all
        .into_iter()
        .filter_map(|s| match s {
            Sent::Chunk(c) => Some(*c),
            _ => None,
        })
        .collect();
    chunks.into_iter().nth(1).expect("the second chunk")
}

/// **A stop and a seek on a block boundary inside a chunk reach the server as
/// changes at that chunk frame.** The third block starts at stream frame 160,
/// frame 32 of the chunk begun at 128. Stopped there, the server's chunk has
/// one change at 32 (not playing); moved to beat 10 there, one at 32 playing
/// at beat 10. The chunk's own transport is its first frame's, playing, at
/// the playhead of frame 128. `-ipc` walks nothing: it ships what the node
/// walked.
///
/// Mutation (run): `Piece::payload` not copying the context's transport
/// changes -> the server is sent none -> fails.
#[test]
fn a_stop_and_a_seek_inside_a_chunk_reach_the_server_on_the_chunk_frame() {
    let _machine = machine_lock();
    let steady = |frame: u64| {
        (
            Bpm::of(120.0),
            Beat(4.0 + frame as f64 * BEATS_PER_FRAME_120),
        )
    };
    for (label, third, playing, quarters) in [
        (
            "stop",
            (
                false,
                Bpm::of(120.0),
                Beat(4.0 + 160.0 * BEATS_PER_FRAME_120),
            ),
            false,
            4.0 + 160.0 * BEATS_PER_FRAME_120,
        ),
        ("seek", (true, Bpm::of(120.0), Beat(10.0)), true, 10.0),
    ] {
        let chunk = chunk_across_the_third_block(label, tempo(1000.0), steady, third);
        let moves: Vec<(u32, bool, f64)> = chunk
            .transport_changes
            .iter()
            .map(|c| {
                (
                    c.offset,
                    c.transport.state.playing,
                    c.transport.position.quarters,
                )
            })
            .collect();
        assert_eq!(moves.len(), 1, "{label}: {moves:?}");
        assert_eq!((moves[0].0, moves[0].1), (32, playing), "{label}");
        assert!((moves[0].2 - quarters).abs() < 1e-9, "{label}: {moves:?}");
        let first = chunk.transport;
        assert!(first.state.playing, "{label}: the chunk began playing");
        assert!(
            (first.position.quarters - (4.0 + 128.0 * BEATS_PER_FRAME_120)).abs() < 1e-9,
            "{label}: the chunk's transport is its first frame's"
        );
    }
}

/// **A tempo ramp across a block boundary inside a chunk reaches the server
/// with the chunk's `tempo_inc`**, the ramp's own slope: linear in time from
/// 120 BPM at beat 0 to 90 at beat 100 is `120 * 100 / 210` seconds, so
/// `-30 / D / 48000` BPM a frame. The chunk 128..256 spans the third block's
/// start (frame 160), and the figure is in the transport the server is sent.
///
/// Mutation (run): `Piece::payload` sending the default transport (no slope)
/// -> `tempo_inc` is 0 -> fails.
#[test]
fn a_tempo_ramp_across_a_block_boundary_reaches_the_server_as_its_slope() {
    use tutti_tempo::{Shape, TempoMap, TempoPoint};
    let _machine = machine_lock();
    let point = |beat: f64, bpm: f64, shape| TempoPoint {
        beat: Beat(beat),
        tempo: Bpm::of(bpm),
        shape,
    };
    let map = tutti_transport::TempoReader::fixed(std::sync::Arc::new(
        TempoMap::new(vec![
            point(0.0, 120.0, Shape::RampTime),
            point(100.0, 90.0, Shape::Step),
        ])
        .expect("a map"),
    ));
    let (duration, rate) = (120.0 * 100.0 / 210.0, 48_000.0);
    let k = -30.0 / duration;
    let at = move |frame: u64| {
        let t = frame as f64 / rate;
        (
            Bpm::of(120.0 + k * t),
            Beat((120.0 * t + k * t * t / 2.0) / 60.0),
        )
    };
    let (bpm, beat) = at(160);
    let chunk = chunk_across_the_third_block("ramp", map, at, (true, bpm, beat));
    let want = k / rate;
    assert!(
        (chunk.transport.timing.tempo_inc - want).abs() < 1e-12,
        "tempo_inc {} against the ramp's {want}",
        chunk.transport.timing.tempo_inc
    );
}
