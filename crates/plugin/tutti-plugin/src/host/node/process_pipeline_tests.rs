//! Pins the out-of-process audio pipeline: output lags input by EXACTLY one
//! block, and anything else is silence rather than wrong audio.
//!
//! These began as tests against the shipped bypass. `AudioBridge::process`
//! pushed `Command::Process` and then did ONE non-blocking `ArrayQueue::pop()`
//! for the reply, so nothing waited for the bridge thread. Two failure modes:
//!
//! - block 0: the response queue was empty, so the batcher emitted silence.
//! - steady state: the pop returned the PREVIOUS block's response, and because a
//!   single-bus plugin collapsed to `output_base == 0`, this block's input write
//!   had already overwritten the region the output read draws from — so the host
//!   read its own input back at unity gain. A silent bypass.
//!
//! **The expected result has inverted, and that is deliberate.** The audio
//! thread no longer waits for a reply at all: it submits block N and collects
//! block N-1, because waiting summed across serially-run nodes and overran the
//! callback (see `batcher`'s module docs). One block of lag is now the *correct*
//! answer, declared to PDC so it is compensated rather than heard. What must
//! never happen is wrong audio: a wrong block, or the input echoed back.
//!
//! Two properties do the work here:
//!
//! - the mock server applies an unmistakable `output = input * 2.0`, so "the
//!   plugin was bypassed" is distinguishable from "the plugin ran";
//! - the driving signal is a per-block ramp, so "one block late" is
//!   distinguishable from "two blocks late" and from "correct".
//!
//! A constant signal would separate none of those, which is why the ramp is
//! load-bearing rather than decorative.

use crate::host::ipc_client::audio::BridgeThread;
use crate::host::ipc_client::PluginBridge;
use crate::host::node::batcher::{Batcher, BATCH_SIZE};
use crate::host::node::BlockPayload;
use crate::protocol::{
    BridgeMessage, ChannelLayout, HostMessage, MidiEventVec, SampleFormat, SlabLayout,
    PROTOCOL_VERSION,
};
use crate::util::transport::shm::{AudioSlab, RING_SLOTS};
use smallvec::smallvec;
use std::sync::Arc;
use tutti_core::{BufferVec, F32};

const CHANNELS: usize = 2;
const GAIN: f32 = 2.0;

fn unique_socket_path(label: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "tutti-test-{}-{}-{}.sock",
        label,
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
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

/// `None` on a clean EOF — the host side closing the socket at teardown is
/// normal shutdown, not a failure, so it must not panic the server thread.
fn recv_host_msg(stream: &interprocess::local_socket::Stream) -> Option<HostMessage> {
    use std::io::Read;
    let mut stream = stream;
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).ok()?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).ok()?;
    bincode::deserialize(&buf).ok()
}

fn send_bridge_msg(stream: &interprocess::local_socket::Stream, msg: &BridgeMessage) {
    use std::io::Write;
    let mut stream = stream;
    let data = bincode::serialize(msg).unwrap();
    let len = (data.len() as u32).to_be_bytes();
    stream.write_all(&len).unwrap();
    stream.write_all(&data).unwrap();
}

/// One stereo bus per direction — the common shape, and the one that used to
/// collapse both directions onto offset 0. It is still the shape under test;
/// what changed is that the two directions now get their own regions, so the
/// input-echo failure is structurally impossible rather than merely unlikely.
fn stereo_layout() -> SlabLayout {
    SlabLayout {
        samples_per_channel: BATCH_SIZE,
        format: SampleFormat::Float32,
        slots: RING_SLOTS as u32,
        inputs: smallvec![ChannelLayout::Stereo],
        outputs: smallvec![ChannelLayout::Stereo],
    }
}

/// A bridge wired to a mock plugin server whose `ProcessAudio` step reads each
/// input channel out of the shared slab, multiplies by [`GAIN`], and writes the
/// result back in place before replying `AudioProcessed` — exactly what a real
/// gain plugin's subprocess does.
fn bridge_with_doubling_server() -> (Arc<PluginBridge>, BridgeThread, std::thread::JoinHandle<()>) {
    bridge_with_server_stall(std::time::Duration::ZERO)
}

/// As above, but the server sleeps `stall` before every reply — a plugin under
/// load, or a subprocess the scheduler has not run yet.
fn bridge_with_server_stall(
    stall: std::time::Duration,
) -> (Arc<PluginBridge>, BridgeThread, std::thread::JoinHandle<()>) {
    use interprocess::local_socket::{traits::Listener as _, ListenerOptions, ToFsName as _};

    let path = unique_socket_path("process-sync");
    let _ = std::fs::remove_file(&path);
    let name = path
        .clone()
        .to_fs_name::<interprocess::local_socket::GenericFilePath>()
        .unwrap();
    let listener = ListenerOptions::new().name(name).create_sync().unwrap();

    let layout = stereo_layout();
    let shm_name = unique_shm_name("process-sync");
    let host_slab = Arc::new(AudioSlab::create(shm_name.clone(), layout.clone()).unwrap());
    // The server side maps the SAME backing file as a view, mirroring the real
    // subprocess: audio travels in shared memory, not over the socket.
    let server_slab = AudioSlab::open(shm_name, layout).unwrap();

    let (bridge, bridge_thread) = PluginBridge::new(
        path.clone(),
        Arc::clone(&host_slab),
        std::path::PathBuf::from("test.vst3"),
        48_000.0,
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
        .name("mock-doubling-server".to_string())
        .spawn(move || {
            struct Cleanup(std::path::PathBuf);
            impl Drop for Cleanup {
                fn drop(&mut self) {
                    let _ = std::fs::remove_file(&self.0);
                }
            }
            let _cleanup = Cleanup(path_cleanup);
            let mut scratch = vec![0.0f32; BATCH_SIZE];
            loop {
                let msg = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    recv_host_msg(&server_stream)
                })) {
                    Ok(Some(msg)) => msg,
                    // Clean EOF or a panicking read: the host is gone, stop.
                    Ok(None) | Err(_) => break,
                };
                let reply = match msg {
                    HostMessage::ProcessAudio(data) => {
                        if !stall.is_zero() {
                            std::thread::sleep(stall);
                        }
                        let n = data.num_samples;
                        let seq = data.seq;
                        // Mirror the real server (`AudioPipeline::process`): read
                        // this block's INPUT slot, and write into its OUTPUT slot
                        // — two disjoint regions, not one shared in place.
                        if server_slab.has_input(seq) {
                            for ch in 0..CHANNELS {
                                let got = server_slab
                                    .read_input_into(seq, ch, &mut scratch[..n])
                                    .unwrap_or(0);
                                for s in scratch[..got].iter_mut() {
                                    *s *= GAIN;
                                }
                                server_slab.write_output(seq, ch, &scratch[..got]).unwrap();
                            }
                            // Exactly once, after the last channel. This store is
                            // the host's evidence that the slot holds this block.
                            server_slab.publish_output(seq);
                        }
                        BridgeMessage::AudioProcessed {
                            latency_us: 0,
                            seq,
                            midi_out: Default::default(),
                        }
                    }
                    _ => continue,
                };
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    send_bridge_msg(&server_stream, &reply)
                }))
                .is_err()
                {
                    break;
                }
            }
        })
        .unwrap();

    // Let the bridge thread start up and register I/O.
    std::thread::sleep(std::time::Duration::from_millis(50));

    (bridge, bridge_thread, server_thread)
}

/// Distinct non-constant ramp per block: block `b` channel `ch` sample `i`
/// carries `1 + b*10 + ch*100 + i*0.1`. Every (block, channel, sample) value is
/// unique, so a one-block-late or echoed-input output is unmistakable. The
/// leading `+1` keeps every sample nonzero — without it `ramp_sample(0,0,0)`
/// would be `0.0` and would spuriously match a silence-filled output.
fn ramp_sample(block: usize, ch: usize, i: usize) -> f32 {
    1.0 + block as f32 * 10.0 + ch as f32 * 100.0 + i as f32 * 0.1
}

/// Drive `blocks` consecutive ramp blocks through a fresh batcher, returning
/// each block's output as `[block][channel][sample]`.
///
/// `gap` is the pause between blocks, mimicking the spacing of a real audio
/// callback, and it stays a parameter because the two regimes still probe
/// different things. With a realistic gap the server's reply has landed, so the
/// steady state is observable and the lag must be exactly one block. With no gap
/// the bridge thread may not be scheduled at all, so a block's output may
/// genuinely be absent — and the requirement becomes that the result is
/// *silence* rather than whatever the ring slot last held.
///
/// Under the original bug the same two regimes failed in two different ways (a
/// stale reply with a gap, an empty queue without), which is why both are kept.
fn drive_blocks_with_gap(blocks: usize, gap: std::time::Duration) -> Vec<Vec<Vec<f32>>> {
    let (bridge, _bridge_thread, _server) = bridge_with_doubling_server();

    // The two directions are separately sized and separately addressed. The old
    // version of this asserted `output_base() == 0` — it pinned the *cause of
    // the bug* as a precondition, so the tests could only ever confirm the
    // aliasing was still there.
    let layout = stereo_layout();
    assert!(
        layout.input_ring_bytes() > 0 && layout.output_ring_bytes() > 0,
        "both directions must have their own region"
    );
    let mut batcher = Batcher::new(CHANNELS, CHANNELS, SampleFormat::Float32, BATCH_SIZE);

    let mut midi_out = MidiEventVec::new();
    let mut input = BufferVec::<F32>::new(CHANNELS);
    let mut output = BufferVec::<F32>::new(CHANNELS);
    let mut captured = Vec::with_capacity(blocks);

    for block in 0..blocks {
        for ch in 0..CHANNELS {
            for i in 0..BATCH_SIZE {
                input.set_scalar(ch, i, ramp_sample(block, ch, i));
            }
        }
        output.clear();

        batcher.process::<f32>(
            &bridge,
            BATCH_SIZE,
            &input.buffer_ref(),
            &mut output.buffer_mut(),
            BlockPayload::default(),
            &mut midi_out,
        );

        captured.push(
            (0..CHANNELS)
                .map(|ch| (0..BATCH_SIZE).map(|i| output.at_scalar(ch, i)).collect())
                .collect(),
        );

        if !gap.is_zero() {
            std::thread::sleep(gap);
        }
    }

    captured
}

/// Realistic spacing: a 64-sample block at 48 kHz is ~1.33 ms; 5 ms leaves the
/// bridge thread unambiguously idle between blocks, so a stale reply would have
/// had every chance to be sitting in the queue.
const CALLBACK_GAP: std::time::Duration = std::time::Duration::from_millis(5);

/// Classify what a block's output actually is, so a failure names the defect
/// instead of just printing two numbers.
///
/// Gained two arms with the pipelining change. "One block late" is now the
/// *correct* answer rather than a defect, and "two blocks late" and "collapsed
/// back to synchronous" became the failures worth naming — a synchronous
/// regression would otherwise show up only as an unexplained numeric mismatch.
fn diagnose(block: usize, got: &[Vec<f32>]) -> String {
    let approx = |a: f32, b: f32| (a - b).abs() < 1e-4;
    let all = |f: &dyn Fn(usize, usize) -> f32| {
        (0..CHANNELS).all(|ch| (0..BATCH_SIZE).all(|i| approx(got[ch][i], f(ch, i))))
    };

    if all(&|_, _| 0.0) {
        "silence (nothing was collectable for this block)".to_string()
    } else if all(&|ch, i| ramp_sample(block, ch, i)) {
        "this block's input at UNITY gain — the host read its own input back \
         (the plugin was bypassed)"
            .to_string()
    } else if all(&|ch, i| ramp_sample(block, ch, i) * GAIN) {
        "THIS block's processed audio — the pipeline collapsed back to \
         synchronous, which is the overrun this design exists to avoid"
            .to_string()
    } else if block > 0 && all(&|ch, i| ramp_sample(block - 1, ch, i) * GAIN) {
        "the previous block's processed audio (the expected pipelined result)".to_string()
    } else if block > 1 && all(&|ch, i| ramp_sample(block - 2, ch, i) * GAIN) {
        "TWO blocks late — the pipeline is deeper than declared, so PDC \
         under-compensates"
            .to_string()
    } else {
        format!("unrecognised: first sample {}", got[0][0])
    }
}

/// **The core contract.** Block 0 is silence (nothing is in flight yet), and
/// from then on block N's output is block N-1's input times [`GAIN`].
///
/// Asserting the lag is *exactly* one is the point: a two-block-late
/// implementation would satisfy a vaguer "the audio eventually arrives" check
/// while silently making PDC under-compensate by 1.33 ms.
#[test]
fn pipeline_output_lags_input_by_exactly_one_block() {
    let blocks = 5;
    let captured = drive_blocks_with_gap(blocks, CALLBACK_GAP);

    assert!(
        captured[0].iter().all(|ch| ch.iter().all(|&s| s == 0.0)),
        "block 0 must be silence — nothing has been submitted yet, and emitting \
         anything else means reading a slot nobody published: {}",
        diagnose(0, &captured[0])
    );

    let mut failures = Vec::new();
    for block in 1..blocks {
        let got = &captured[block];
        let ok = (0..CHANNELS).all(|ch| {
            (0..BATCH_SIZE)
                .all(|i| (got[ch][i] - ramp_sample(block - 1, ch, i) * GAIN).abs() < 1e-4)
        });
        if !ok {
            failures.push(format!("  block {block}: got {}", diagnose(block, got)));
        }
    }

    assert!(
        failures.is_empty(),
        "block N's output must be block N-1's input x{GAIN}, but:\n{}",
        failures.join("\n")
    );
}

/// The steady-state half, pinned separately so it cannot hide behind a
/// start-up failure: several blocks in, the output must still be exactly one
/// block behind — never this block's own input echoed back, never two behind.
#[test]
fn pipeline_steady_state_is_not_echoed_or_doubly_stale() {
    let blocks = 6;
    let captured = drive_blocks_with_gap(blocks, CALLBACK_GAP);

    for block in 2..blocks {
        let got = &captured[block];
        let matches = |b: usize, gain: f32| {
            (0..CHANNELS)
                .all(|ch| (0..BATCH_SIZE).all(|i| (got[ch][i] - ramp_sample(b, ch, i) * gain).abs() < 1e-4))
        };

        assert!(
            !matches(block, 1.0),
            "block {block}: the host read its own input back at unity gain — \
             the plugin was bypassed entirely"
        );
        assert!(
            !matches(block, GAIN),
            "block {block}: got THIS block's processed audio — the pipeline \
             collapsed back to waiting for the reply"
        );
        assert!(
            !matches(block - 2, GAIN),
            "block {block}: got audio from two blocks ago — deeper than the \
             one block declared to PDC"
        );

        for ch in 0..CHANNELS {
            for i in 0..BATCH_SIZE {
                let expected = ramp_sample(block - 1, ch, i) * GAIN;
                assert!(
                    (got[ch][i] - expected).abs() < 1e-4,
                    "block {block} ch {ch} sample {i}: expected {expected}, got {} — {}",
                    got[ch][i],
                    diagnose(block, got)
                );
            }
        }
    }
}

/// With no gap at all the bridge thread may not be scheduled between blocks, so
/// a block's reply can genuinely be absent. The result must then be **silence**,
/// not stale audio from an earlier block sitting in the ring.
///
/// This is the case the sequence check exists for. Without it the host would
/// read whichever block last occupied the slot and emit it as though it were
/// current — quieter than the original bypass, and the same class of defect.
#[test]
fn a_missing_reply_yields_silence_never_stale_audio() {
    let blocks = 5;
    let captured = drive_blocks_with_gap(blocks, std::time::Duration::ZERO);

    for (block, got) in captured.iter().enumerate() {
        let silent = got.iter().all(|ch| ch.iter().all(|&s| s == 0.0));
        if silent {
            continue; // The reply had not landed. Correct.
        }
        // Otherwise it must be exactly the previous block — never any other.
        let ok = (0..CHANNELS).all(|ch| {
            (0..BATCH_SIZE).all(|i| {
                block > 0 && (got[ch][i] - ramp_sample(block - 1, ch, i) * GAIN).abs() < 1e-4
            })
        });
        assert!(
            ok,
            "block {block}: non-silent output that is not block {}'s audio — {}",
            block.wrapping_sub(1),
            diagnose(block, got)
        );
    }
}

/// **The test that justifies the whole change.** Several stalled plugins driven
/// in series must cost the audio thread no waiting, however many there are.
///
/// # What this replaces
///
/// The synchronous design was individually defensible — each plugin waited at
/// most half its own block period before giving up and emitting silence. But
/// fundsp runs nodes *serially* within one callback (`for &node_index in
/// self.order`), so the budgets summed:
///
/// | Stalled plugins | Spent waiting | vs the 1333 us period @ 64/48k |
/// |---|---|---|
/// | 1 | 667 us | 0.50x — fine |
/// | 2 | 1333 us | 1.00x — at the edge |
/// | **3** | **2000 us** | **1.50x — overrun** |
/// | 8 | 5333 us | 4.00x — overrun |
///
/// Three concurrently-stalled plugins blew the callback; measured under 24x CPU
/// load, 4 of 12 blocks made their deadline. Parallelising fundsp would not have
/// helped — plugins in series on one track are a dependency chain, and that is
/// the common arrangement. The defect was the waiting, not the serialism.
///
/// The loop below drives the plugins one after another within each block, which
/// is exactly the arrangement whose budgets used to sum.
///
/// # On the threshold
///
/// Deliberately loose — orders of magnitude above what the work costs — because
/// this is not a benchmark and must not flake on a loaded CI box. It does not
/// need to be tight: a regression to synchronous waiting would exceed it by
/// ~100x, not by a few percent. A tight bound would buy nothing and cost
/// intermittent failures.
#[test]
fn stalled_plugins_do_not_stall_the_audio_thread() {
    use std::time::{Duration, Instant};

    const PLUGINS: usize = 3;
    const BLOCKS: usize = 20;
    /// Far beyond any block period, so a design that waits cannot hide it.
    const SERVER_STALL: Duration = Duration::from_millis(10);
    const TIME_LIMIT: Duration = Duration::from_millis(300);

    let mut rigs: Vec<_> = (0..PLUGINS)
        .map(|_| bridge_with_server_stall(SERVER_STALL))
        .collect();
    let mut batchers: Vec<_> = (0..PLUGINS)
        .map(|_| Batcher::new(CHANNELS, CHANNELS, SampleFormat::Float32, BATCH_SIZE))
        .collect();

    let mut input = BufferVec::<F32>::new(CHANNELS);
    let mut output = BufferVec::<F32>::new(CHANNELS);
    for ch in 0..CHANNELS {
        for i in 0..BATCH_SIZE {
            input.set_scalar(ch, i, ramp_sample(0, ch, i));
        }
    }
    let mut midi_out = MidiEventVec::new();

    let start = Instant::now();
    for _ in 0..BLOCKS {
        for (batcher, (bridge, _, _)) in batchers.iter_mut().zip(rigs.iter()) {
            output.clear();
            batcher.process::<f32>(
                bridge,
                BATCH_SIZE,
                &input.buffer_ref(),
                &mut output.buffer_mut(),
                BlockPayload::default(),
                &mut midi_out,
            );
        }
    }
    let elapsed = start.elapsed();

    // What the old design would have spent: every plugin, every block, waiting
    // out its budget before giving up.
    let synchronous_floor = SERVER_STALL * (BLOCKS * PLUGINS) as u32;
    assert!(
        elapsed < TIME_LIMIT,
        "{PLUGINS} stalled plugins x {BLOCKS} blocks took {elapsed:?}, over the \
         {TIME_LIMIT:?} limit. A synchronous design would need ~{synchronous_floor:?} — \
         this looks like the audio thread is waiting for replies again."
    );

    for (_, thread, _) in rigs.iter_mut() {
        thread.shutdown();
    }
}
