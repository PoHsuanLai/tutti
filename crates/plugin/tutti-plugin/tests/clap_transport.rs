//! **The transport a hosted plugin sees is the graph's `Env`**, at every
//! chunk it is sent — through a seek, a loop wrap, a tempo change and a stop,
//! live and in an offline fork — read back out of a real `plugin-server`.
//!
//! Doc 013 (Verdicts, `TransportSource`): the plugin node's transport is a
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
use clap_probe::{exclusive, load_probe, render, ProbeEnv, Rig};

use std::sync::Arc;
use std::time::Duration;

use tutti_core::transport::{OfflineTimeline, OfflineTimelineConfig, OfflineTransport};
use tutti_graph::{Env, ForkMode, LoopRange, Offset, Transport, TransportChanges};
use tutti_types::{Beat, Bpm, SampleRate, Samples};

const SAMPLE_RATE: f64 = 48_000.0;
/// The pipeline's chunk: what the node ships per submission.
const CHUNK: usize = 64;
/// Frames the probe's echo occupies (`TRANSPORT_ECHO_FRAMES`), mirrored.
const ECHO: usize = 5;

/// What the probe writes for a transport: beats, tempo, flags (1 playing,
/// 2 recording, 4 loop), loop start, loop end.
fn echo_of(t: &Transport) -> [f64; ECHO] {
    let flags = f64::from(u8::from(t.playing))
        + 2.0 * f64::from(u8::from(t.recording()))
        + 4.0 * f64::from(u8::from(t.looping.is_some()));
    let (start, end) = t
        .looping
        .map_or((0.0, 0.0), |l| (l.start.get(), l.end.get()));
    [t.beat().get(), t.tempo.get(), flags, start, end]
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
    let rolling = Transport::new(true, Bpm(120.0), Beat(base), None);
    let t = match b % 6 {
        // A plain roll.
        0 => rolling,
        // A seek, far from where the last block left off.
        1 => Transport::new(true, Bpm(120.0), Beat(64.0 + base), None),
        // A tempo change inside the block, before the second chunk when the
        // block has one; at the last frame otherwise.
        2 => {
            let at = if block > CHUNK { 40 } else { block - 1 };
            let at = Offset::new(at, Samples(block)).expect("inside");
            changes
                .push(at, Transport::new(true, Bpm(90.0), Beat(base + 0.5), None))
                .expect("a change inside the block");
            rolling
        }
        // A loop 32 frames long from here: it wraps 32 frames in, before a
        // second chunk starts.
        3 => Transport::new(
            true,
            Bpm(120.0),
            Beat(base),
            Some(LoopRange {
                start: Beat(base),
                end: Beat(base + 32.0 / 24_000.0),
            }),
        ),
        // Recording.
        4 => rolling.with_recording(true),
        // Stopped.
        _ => Transport::new(false, Bpm(100.0), Beat(base), None),
    };
    (t, changes)
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
    let _lock = exclusive();
    let _env = ProbeEnv::new().render_mode(render::TRANSPORT);
    let probe = load_probe(SAMPLE_RATE);
    let mut rig = Rig::new(probe.client.bind(), SAMPLE_RATE, CHUNK);

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
    let _lock = exclusive();
    let _env = ProbeEnv::new().render_mode(render::TRANSPORT);
    let probe = load_probe(SAMPLE_RATE);
    let offline = OfflineTransport::new(Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        sample_rate: SampleRate(SAMPLE_RATE),
        ..Default::default()
    })));
    let fork = probe
        .client
        .fork_instance(ForkMode::Offline(&offline))
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
