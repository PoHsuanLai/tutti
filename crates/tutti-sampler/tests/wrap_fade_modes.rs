//! A looped render with a wrap crossfade live is bit for bit the same on a
//! worker pool as serial, over ragged blocks that cut the fade at every
//! offset. (The differential suite lives in `tutti-graph`, which cannot name
//! a sampler node, so the comparison is made here.)
//!
//! Not covered: offline against live. A memory voice reads the same code on
//! both, and a fork shares nothing else with it; `tutti-export`'s render is
//! a different crate's gate.
//!
//! The voices go through a `VoicePool` each, then as bare `MemorySource`
//! nodes. The fade is asserted to have run: the voice placed before the loop
//! start steps across the wrap by less than a cut's step, which the wave
//! gives analytically.
//!
//! Mutation (run): `Wrap::new` always `None` (no fade) → the step at the wrap
//! is the cut's → fails, for both hosts.

use std::sync::Arc;

use tutti_graph::{Editor, Executor, LoopRange, Prepare, Transport, WorkerPool};
use tutti_sampler::{MemorySource, Playback, SlotId, Voice, VoicePool, VoiceSource, Wave};
use tutti_types::graph::{OutPort, Source};
use tutti_types::{Beat, Bpm, FrameClock, NodeKey, SampleRate, Samples};

const SR: f64 = 48_000.0;
const MAX_BLOCK: usize = 64;
const READS: u64 = 12;
/// Beats 0.125 to 0.376 at 120 BPM: a wrap every 6 024 frames.
const LOOP: (f64, f64) = (0.125, 0.376);

fn tone() -> Arc<Wave> {
    let mut w = Wave::new(2, SR);
    for i in 0..SR as usize {
        let t = i as f32 / SR as f32;
        w.push_frame(&[(t * 2_765.0).sin() * 0.4, (t * 4_148.0).sin() * 0.3]);
    }
    Arc::new(w)
}

/// What a voice is in the graph.
#[derive(Clone, Copy, Debug)]
enum Host {
    Pool,
    Node,
}

fn graph(wave: &Arc<Wave>, workers: usize, host: Host) -> Executor {
    let (mut ed, mut exec) = Editor::new(Prepare::new(SampleRate(SR), Samples(MAX_BLOCK)));
    if workers > 1 {
        exec.set_pool(Some(Arc::new(WorkerPool::new(workers))));
    }
    let mut outputs = Vec::new();
    for k in 0..READS {
        let key = NodeKey(k + 1);
        // Placed before the loop's start, on it, and inside the fade.
        let at = [0.0, LOOP.0, LOOP.0 + 0.002][k as usize % 3] + k as f64 * 0.0001;
        let source = MemorySource::placed(Arc::clone(wave), Beat(at), None);
        match host {
            Host::Pool => {
                let mut pool = VoicePool::new();
                pool.insert_voice(
                    SlotId(1),
                    Voice {
                        source: VoiceSource::Memory(source),
                        play: Playback::default(),
                        channel_index: None,
                    },
                );
                ed.insert(key, "pool", pool);
            }
            Host::Node => {
                ed.insert(key, "source", source);
            }
        }
        for port in 0..2u16 {
            outputs.push(Source::Node(OutPort { node: key, port }));
        }
    }
    ed.spec_mut().topology.outputs = outputs;
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    exec
}

fn render(exec: &mut Executor) -> Vec<Vec<u32>> {
    let channels = 2 * READS as usize;
    let mut all = vec![Vec::new(); channels];
    let region = tutti_types::LoopRange::new(LOOP.0, LOOP.1).expect("a loop");
    let mut clock = FrameClock::new(Beat(0.0), Bpm(120.0), SampleRate(SR));
    let mut wraps = 0;
    for i in 0..600 {
        let n = 1 + (i * 37) % MAX_BLOCK;
        let mut chunk = vec![vec![0.0f32; n]; channels];
        {
            let mut outs: Vec<&mut [f32]> = chunk.iter_mut().map(Vec::as_mut_slice).collect();
            let looping = LoopRange {
                start: Beat(LOOP.0),
                end: Beat(LOOP.1),
            };
            let t = Transport::new(true, Bpm(120.0), clock.beat(), Some(looping));
            exec.process(n, &t, &[], &mut outs);
        }
        let before = clock.beat();
        clock.advance(Samples(n), Some(region));
        wraps += usize::from(clock.beat() < before);
        for (a, c) in all.iter_mut().zip(chunk) {
            a.extend(c.iter().map(|x| x.to_bits()));
        }
    }
    assert!(wraps >= 2, "the run wraps ({wraps})");
    // Voice 0 is placed before the loop's start. At the first wrap (the
    // loop's end, 9 000 frames) a cut steps from the source at the end to
    // the one at the start; the crossfade steps by a tone's own step and
    // the fade's, far below it.
    let wave = tone();
    let at = |i: usize| wave.at(0, i);
    let cut = (at(3_000) - at(9_023)).abs();
    assert!(cut > 0.2, "the wave makes a cut a real step ({cut})");
    let lane: Vec<f32> = all[0].iter().map(|&b| f32::from_bits(b)).collect();
    let step = (9_014..9_034)
        .map(|i| (lane[i] - lane[i - 1]).abs())
        .fold(0.0, f32::max);
    assert!(step < 0.05, "step {step} at the wrap, a cut is {cut}");
    all
}

#[test]
#[cfg_attr(miri, ignore = "spawns a worker pool")]
fn a_wrap_crossfade_renders_bit_identically_on_a_pool() {
    let wave = tone();
    for host in [Host::Pool, Host::Node] {
        let serial = render(&mut graph(&wave, 1, host));
        assert!(
            serial
                .iter()
                .all(|c| c.iter().any(|&b| f32::from_bits(b) != 0.0)),
            "{host:?}"
        );
        for workers in [2, 4] {
            let mut exec = graph(&wave, workers, host);
            assert!(exec.is_parallel());
            assert_eq!(serial, render(&mut exec), "{host:?}, {workers} workers");
        }
    }
}
