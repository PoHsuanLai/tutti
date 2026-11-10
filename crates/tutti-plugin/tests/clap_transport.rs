//! **The transport a hosted plugin sees is the graph's `Env`**, at every
//! chunk it is sent — through a seek, a loop wrap, a tempo change and a stop,
//! live and in an offline fork — read back out of a real `plugin-server`.
//!
//! The plugin node's transport is a
//! pure function of the block's `Env` plus the meter; nothing polls a shared
//! timeline. The reference CLAP plugin's `Transport` render mode writes the
//! `clap_event_transport` it was handed into the first frames of its output
//! (`tutti_clap_test_plugin::RenderMode::Transport`), so what the host told it
//! comes back through the audio. Every chunk's echo is compared with
//! `Env::transport_at` of the chunk's first frame, computed here from the very
//! `Env` the executor is handed.
//!
//! The pipeline holds one chunk, so the echo of chunk `k` arrives in the
//! output of chunk `k + 1`.
//!
//! Needs `cargo build -p tutti-plugin-server` first (see `CLAUDE.md`).

#![cfg(feature = "clap")]

#[path = "support/clap_probe.rs"]
mod clap_probe;
use clap_probe::{ProbeEnv, Rig, load_probe, render};
use tutti_fixtures::machine_lock;

use std::time::Duration;

use tutti_graph::{Env, ForkMode, LoopRange, Offset, Transport, TransportChanges};
use tutti_types::{Beat, Bpm, Frame, Latency, SampleRate, Samples};

const SAMPLE_RATE: f64 = 48_000.0;
/// The pipeline's chunk: what the node ships per submission.
const CHUNK: usize = 64;
/// Frames the probe's echo occupies (`TRANSPORT_ECHO_FRAMES`), mirrored.
const ECHO: usize = 6;

/// What the probe writes for a transport: beats, tempo, flags (1 playing,
/// 2 recording, 4 loop), loop start, loop end, tempo slope. The slope of
/// these scenes is always 0: the map behind them (`scene_map`) has steps
/// only (the slope itself is `the_plugin_is_told_the_maps_tempo_and_slope`).
fn echo_of(t: &Transport) -> [f64; ECHO] {
    let flags = f64::from(u8::from(t.playing))
        + 2.0 * f64::from(u8::from(t.recording()))
        + 4.0 * f64::from(u8::from(t.looping.is_some()));
    let (start, end) = t
        .looping
        .map_or((0.0, 0.0), |l| (l.start.get(), l.end.get()));
    [t.beat().get(), t.tempo.get(), flags, start, end, 0.0]
}

/// The echo in `out` at `at`, if the chunk there carried one (a chunk the
/// pipeline had nothing to collect for is silence).
fn echo_at(out: &[f32], at: usize) -> Option<[f64; ECHO]> {
    let e: [f64; ECHO] = std::array::from_fn(|i| f64::from(out[at + i]));
    // Tempo is never zero in these scenarios, so a zero tempo is "nothing".
    (e[1] != 0.0).then_some(e)
}

fn assert_echo(got: [f64; ECHO], want: [f64; ECHO], what: &str) {
    // f32 through the probe's output: a beat to about a millionth.
    let close = got.iter().zip(&want).all(|(g, w)| (g - w).abs() < 1e-4);
    assert!(close, "{what}: the plugin saw {got:?}, Env says {want:?}");
}

/// One scene per block: the transport at its first frame, and the changes
/// inside it. Cycles through a plain roll, a seek, a tempo change inside the
/// block, a loop that wraps inside the block, recording, and a stop, so every
/// kind reaches the plugin whatever block a live run happens to collect.
fn scene(b: usize, block: usize) -> (Transport, TransportChanges) {
    let base = 4.0 + b as f64;
    let mut changes = TransportChanges::NONE;
    let rolling = Transport::new(true, Bpm::of(120.0), Beat(base), None);
    let t = match b % 6 {
        // A plain roll.
        0 => rolling,
        // A seek, far from where the last block left off.
        1 => Transport::new(true, Bpm::of(120.0), Beat(64.0 + base), None),
        // A tempo change inside the block, before the second chunk when the
        // block has one; at the last frame otherwise.
        2 => {
            let at = if block > CHUNK { 40 } else { block - 1 };
            let at = Offset::new(at, Samples(block)).expect("inside");
            changes
                .push(
                    at,
                    Transport::new(true, Bpm::of(90.0), Beat(base + 0.5), None),
                )
                .expect("a change inside the block");
            rolling
        }
        // A loop 32 frames long from here: it wraps 32 frames in, before a
        // second chunk starts.
        3 => Transport::new(
            true,
            Bpm::of(120.0),
            Beat(base),
            Some(LoopRange {
                start: Beat(base),
                end: Beat(base + 32.0 / 24_000.0),
            }),
        ),
        // Recording.
        4 => rolling.with_recording(true),
        // Stopped.
        _ => Transport::new(false, Bpm::of(100.0), Beat(base), None),
    };
    (t, changes)
}

/// The project's tempo map behind [`scene`]: 120 BPM, 90 around the tempo
/// scenes' change and 100 around the stopped scenes' beat, so the tempo the
/// plugin is told from the map is the one the scene's transport carries at
/// each frame. (A tempo point is the map's way to say what a transport's
/// own tempo change says.)
fn scene_map(blocks: usize) -> tutti_tempo::TempoMap {
    use tutti_tempo::{Shape, TempoMap, TempoPoint};
    let point = |beat: f64, bpm: f64| TempoPoint {
        beat: Beat(beat),
        tempo: Bpm::of(bpm),
        shape: Shape::Step,
    };
    let mut points = vec![point(0.0, 120.0)];
    for b in 0..blocks {
        let base = 4.0 + b as f64;
        match b % 6 {
            2 => points.extend([point(base + 0.4, 90.0), point(base + 0.6, 120.0)]),
            5 => points.extend([point(base - 0.1, 100.0), point(base + 0.1, 120.0)]),
            _ => {}
        }
    }
    TempoMap::new(points).expect("a map")
}

/// A reader of [`scene_map`], for the nodes under test.
fn scene_tempo() -> tutti_transport::TempoReader {
    tutti_transport::TempoReader::fixed(std::sync::Arc::new(scene_map(64)))
}

/// Render `blocks` scenes of `block` frames through `rig`, one block per call;
/// return, per block, the `Env` the node was handed and the output channel 0.
fn render_scenes(
    rig: &mut Rig,
    blocks: usize,
    block: usize,
    pace: Duration,
) -> Vec<(Env, Vec<f32>)> {
    let ins = rig.inputs();
    let silence = vec![0.0f32; block];
    let inputs: Vec<&[f32]> = (0..ins).map(|_| &silence[..]).collect();
    let mut out = vec![vec![0.0f32; block]; rig.outputs()];
    let mut seen = Vec::with_capacity(blocks);
    for b in 0..blocks {
        let (transport, changes) = scene(b, block);
        let r = rig.renderer();
        let env = Env {
            frame: r.executor().frame(),
            sample_rate: SampleRate(SAMPLE_RATE),
            block_len: Samples(block),
            transport,
            changes,
            play: tutti_graph::PlayClock::ZERO,
        };
        let mut outs: Vec<&mut [f32]> = out.iter_mut().map(|c| &mut c[..]).collect();
        r.executor_mut()
            .process_with_changes(block, &transport, &changes, &inputs, &mut outs);
        r.editor_mut().collect();
        seen.push((env, out[0].clone()));
        std::thread::sleep(pace);
    }
    seen
}

fn offset(i: usize, block: usize) -> Offset {
    Offset::new(i, Samples(block)).expect("inside the block")
}

/// **Live**: blocks of one chunk, paced to the device. The echo of block
/// `b - 1` is in block `b`, and wherever the pipeline collected it, it is
/// exactly what that block's `Env` said at its first frame. Every kind of
/// scene is collected at least once.
///
/// Mutation: build the snapshot from a default `Transport` in `Snapshot::at`
/// (the node's old "no source installed" answer) → every echo is beat 0,
/// stopped → fails. Mutation: `recording: false` in `Snapshot::at` → the
/// recording scene fails. (Reading `env.transport` instead of `transport_at`
/// is invisible at offset 0; the offline test below catches it.)
#[test]
fn the_transport_a_live_plugin_sees_is_env_at_every_block() {
    let _lock = machine_lock();
    let _env = ProbeEnv::new().render_mode(render::TRANSPORT);
    let probe = load_probe(SAMPLE_RATE);
    let mut rig = Rig::new(probe.client.bind(scene_tempo()), SAMPLE_RATE, CHUNK);

    const BLOCKS: usize = 60;
    let period = Duration::from_nanos((CHUNK as f64 / SAMPLE_RATE * 1e9) as u64);
    let seen = render_scenes(&mut rig, BLOCKS, CHUNK, period.saturating_mul(20));

    let mut kinds = [0usize; 6];
    for b in 1..BLOCKS {
        let (env, _) = &seen[b - 1];
        let Some(got) = echo_at(&seen[b].1, 0) else {
            continue;
        };
        assert_echo(
            got,
            echo_of(&env.transport_at(offset(0, CHUNK))),
            &format!("block {}", b - 1),
        );
        kinds[(b - 1) % 6] += 1;
    }
    assert!(
        kinds.iter().all(|&n| n > 0),
        "every kind of scene reached the plugin at least once: {kinds:?}"
    );
    drop(probe.handle);
}

/// **Offline, and chunked**: an offline fork (it waits for every chunk, so
/// every echo is there) rendered in blocks of two chunks. Each chunk is sent
/// the transport at **its own** first frame — the second chunk after the
/// block's tempo change, and after its loop wrap — not the block's.
///
/// Mutation: send every chunk `Offset::ZERO`'s transport (the block's) → the
/// second chunk of the loop scene reports the unwrapped beat, and of the
/// tempo scene the old tempo → fails. Mutation: ignore `env.changes` in the
/// snapshot → the tempo scene's second chunk reports 120 → fails.
#[test]
fn the_transport_an_offline_fork_sees_is_env_at_every_chunk() {
    let _lock = machine_lock();
    let _env = ProbeEnv::new().render_mode(render::TRANSPORT);
    let probe = load_probe(SAMPLE_RATE);
    let fork = probe
        .client
        .fork_instance(ForkMode::Offline, scene_tempo())
        .expect("the probe forks");
    const BLOCK: usize = 2 * CHUNK;
    // A 64-frame device quantum in 128-frame blocks: two chunks a block.
    let prepare = tutti_graph::Prepare::new(SampleRate(SAMPLE_RATE), Samples(BLOCK))
        .with_quantum(Samples(CHUNK));
    let mut rig = Rig::prepared(fork, prepare);

    const BLOCKS: usize = 24;
    let seen = render_scenes(&mut rig, BLOCKS, BLOCK, Duration::ZERO);

    let mut checked = 0;
    for b in 0..BLOCKS {
        let (env, out) = &seen[b];
        // Chunk 0 of this block, echoed by chunk 1, at frame 64.
        let first = echo_at(out, CHUNK).expect("an offline fork collects every chunk");
        assert_echo(
            first,
            echo_of(&env.transport_at(offset(0, BLOCK))),
            &format!("block {b}, chunk 0"),
        );
        // Chunk 1 of this block, echoed by the next block's chunk 0.
        if let Some((_, next)) = seen.get(b + 1) {
            let second = echo_at(next, 0).expect("an offline fork collects every chunk");
            assert_echo(
                second,
                echo_of(&env.transport_at(offset(CHUNK, BLOCK))),
                &format!("block {b}, chunk 1"),
            );
            checked += 1;
        }
    }
    assert_eq!(checked, BLOCKS - 1);
    // The scenes did move the transport inside a block: the second chunk of
    // the tempo and loop scenes is not the block's own transport.
    let (tempo_env, _) = &seen[2];
    assert_ne!(
        echo_of(&tempo_env.transport_at(offset(CHUNK, BLOCK)))[1],
        echo_of(&tempo_env.transport)[1]
    );
    let (loop_env, _) = &seen[3];
    assert!(
        loop_env.transport_at(offset(CHUNK, BLOCK)).beat().get()
            < loop_env.transport.beat().get() + 0.001,
        "the loop wraps before the second chunk"
    );
    drop(probe.handle);
}

/// What the blocks in `seen` said for timeline frame `f`: `Env::transport_at`
/// in the block that held it, or, for a frame before the first block, the
/// first block's transport, unmoved (`TransportHistory`'s rule).
fn said_at(seen: &[(Env, Vec<f32>)], f: Option<u64>) -> Transport {
    let Some(f) = f else {
        return seen[0].0.transport;
    };
    let (env, _) = seen
        .iter()
        .find(|(e, _)| e.offset_of(Frame(f)).is_some())
        .expect("a block held the frame");
    env.transport_at(env.offset_of(Frame(f)).expect("inside"))
}

/// **Behind a latent path**: the plugin's input arrives 40 frames late (a
/// silent source declaring 40 frames of latency feeds it), so each chunk is
/// sent the transport 40 frames before its own first frame: the transport
/// the audio it processes was rendered at. For the first chunk of a block
/// that is the *previous* block's transport, 88 frames into it, across every
/// seek, tempo change, loop wrap, stop and record toggle the scenes cycle
/// through. For the second chunk it is 24 frames into its own block.
/// Offline, so every chunk is collected.
///
/// Mutation: build `Snapshot::at` from `cx.env.transport_at(offset)` (the
/// chunk's own frame, as before) → the first chunk of every block reports
/// its own block's scene → fails. Mutation: hand the plugin node a `Cx`
/// with `arrival: Latency::ZERO` in `PluginChunks::begin` → fails.
#[test]
fn a_plugin_behind_a_latent_path_sees_the_transport_its_input_was_rendered_at() {
    let _lock = machine_lock();
    let _env = ProbeEnv::new().render_mode(render::TRANSPORT);
    let probe = load_probe(SAMPLE_RATE);
    let fork = probe
        .client
        .fork_instance(ForkMode::Offline, scene_tempo())
        .expect("the probe forks");
    const BLOCK: usize = 2 * CHUNK;
    const ARRIVAL: usize = 40;
    let prepare = tutti_graph::Prepare::new(SampleRate(SAMPLE_RATE), Samples(BLOCK))
        .with_quantum(Samples(CHUNK));
    let mut rig = Rig::behind(fork, prepare, Latency::new(Samples(ARRIVAL)));

    const BLOCKS: usize = 24;
    let seen = render_scenes(&mut rig, BLOCKS, BLOCK, Duration::ZERO);

    let heard = |b: usize, o: usize| (b * BLOCK + o).checked_sub(ARRIVAL).map(|f| f as u64);
    let mut differs = 0;
    for b in 0..BLOCKS {
        let (env, out) = &seen[b];
        let first = echo_at(out, CHUNK).expect("an offline fork collects every chunk");
        let want = said_at(&seen, heard(b, 0));
        assert_echo(first, echo_of(&want), &format!("block {b}, chunk 0"));
        if echo_of(&want) != echo_of(&env.transport_at(offset(0, BLOCK))) {
            differs += 1;
        }
        if let Some((_, next)) = seen.get(b + 1) {
            let second = echo_at(next, 0).expect("an offline fork collects every chunk");
            assert_echo(
                second,
                echo_of(&said_at(&seen, heard(b, CHUNK))),
                &format!("block {b}, chunk 1"),
            );
        }
    }
    // The previous block's transport is not this one's: the test can tell.
    assert!(differs >= BLOCKS - 1, "{differs} chunks could tell");
    drop(probe.handle);
}

/// 120 to 180 BPM over the first four beats, tempo linear in time: it climbs
/// 60 / 76 800 BPM per frame at 48 kHz and is 150 at beat 1.8.
fn ramp_tempo() -> tutti_transport::TempoReader {
    use tutti_tempo::{Shape, TempoMap, TempoPoint};
    let map = TempoMap::new([
        TempoPoint {
            beat: Beat(0.0),
            tempo: Bpm::of(120.0),
            shape: Shape::RampTime,
        },
        TempoPoint {
            beat: Beat(4.0),
            tempo: Bpm::of(180.0),
            shape: Shape::Step,
        },
    ])
    .expect("a map");
    tutti_transport::TempoReader::fixed(std::sync::Arc::new(map))
}

/// **The plugin is told the project map's tempo and CLAP its slope**, through
/// a real `plugin-server`: playing at beat 1.8 of a ramp, its transport event
/// carries the curve's tempo there (150, not the engine's chord 120) and the
/// ramp's own slope in `tempo_inc`; stopped, the slope is zero.
///
/// Mutation: drop `.with_tempo_inc(..)` in the CLAP host's `convert_transport`
/// (the server's copy of the wire's snapshot) -> the echoed slope is 0 ->
/// fails. Mutation: leave `Snapshot::follow` out of `from_cx` -> tempo 120 ->
/// fails.
#[test]
fn the_plugin_is_told_the_maps_tempo_and_slope() {
    let _lock = machine_lock();
    let _env = ProbeEnv::new().render_mode(render::TRANSPORT);
    let probe = load_probe(SAMPLE_RATE);
    let fork = probe
        .client
        .fork_instance(ForkMode::Offline, ramp_tempo())
        .expect("the probe forks");
    let prepare = tutti_graph::Prepare::new(SampleRate(SAMPLE_RATE), Samples(CHUNK))
        .with_quantum(Samples(CHUNK));
    let mut rig = Rig::prepared(fork, prepare);

    let ins = rig.inputs();
    let silence = vec![0.0f32; CHUNK];
    let inputs: Vec<&[f32]> = (0..ins).map(|_| &silence[..]).collect();
    let mut out = vec![vec![0.0f32; CHUNK]; rig.outputs()];
    let mut heard = Vec::new();
    // Rolling, then stopped, at the same beat: each block's echo is in the
    // next block's output (the pipeline holds one chunk).
    for playing in [true, true, false, false] {
        // The engine's own transport says 120 whatever the map says.
        let transport = Transport::new(playing, Bpm::of(120.0), Beat(1.8), None);
        let changes = TransportChanges::NONE;
        let r = rig.renderer();
        let mut outs: Vec<&mut [f32]> = out.iter_mut().map(|c| &mut c[..]).collect();
        r.executor_mut()
            .process_with_changes(CHUNK, &transport, &changes, &inputs, &mut outs);
        r.editor_mut().collect();
        heard.push(echo_at(&out[0], 0));
    }
    // Block 1 hears block 0's chunk (rolling), block 3 block 2's (stopped).
    let rolling = heard[1].expect("an offline fork collects every chunk");
    assert!((rolling[1] - 150.0).abs() < 1e-3, "tempo {}", rolling[1]);
    assert!(
        (rolling[5] - 60.0 / 76_800.0).abs() < 1e-9,
        "slope {}",
        rolling[5]
    );
    let stopped = heard[3].expect("an offline fork collects every chunk");
    assert!((stopped[1] - 150.0).abs() < 1e-3);
    assert_eq!(stopped[5], 0.0);
    drop(probe.handle);
}
