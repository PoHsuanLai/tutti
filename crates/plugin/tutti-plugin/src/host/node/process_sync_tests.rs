//! Pins the out-of-process audio round-trip to the CURRENT block.
//!
//! These tests were written against the bug: `AudioBridge::process` pushed
//! `Command::Process` and then did ONE non-blocking `ArrayQueue::pop()` for the
//! reply, so nothing waited for the bridge thread (which additionally slept a
//! fixed poll interval between command polls before its blocking socket
//! round-trip). The two failure modes that produced:
//!
//! - block 0: the response queue was empty → `process` returned `false` → the
//!   batcher emitted `silence_block`.
//! - steady state: the pop returned the PREVIOUS block's response, and because
//!   a single-bus plugin collapses to `output_base == 0` (see
//!   `SlabLayout::output_base`), the current block's input write had already
//!   overwritten the region the output read draws from — so the host read its
//!   own input back at unity gain: a silent bypass.
//!
//! Both are now ruled out by matching the server-echoed `buffer_id` (see
//! `AudioBridge::process`): a reply is only accepted for the block that
//! produced it, and anything else means silence rather than wrong audio.
//!
//! The mock server applies an unmistakable `output = input * 2.0`, and the
//! driving signal is a per-block ramp. Both matter: a constant signal cannot
//! separate "correct" from "one block late", and the x2 gain separates
//! "correct" from "input echoed back".
//!
//! These tests assume the machine can actually schedule the bridge thread
//! within the block's wait budget. That holds in normal conditions; under
//! deliberate CPU saturation the round-trip genuinely exceeds any RT-legal
//! budget and the correct result becomes silence, not doubled audio.

use crate::host::ipc_client::audio::BridgeThread;
use crate::host::ipc_client::PluginBridge;
use crate::host::node::batcher::{Batcher, BATCH_SIZE};
use crate::host::node::BlockPayload;
use crate::protocol::{
    BridgeMessage, ChannelLayout, HostMessage, MidiEventVec, SampleFormat, SlabLayout,
    PROTOCOL_VERSION,
};
use crate::util::transport::shm::AudioSlab;
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

/// The single-bus legacy layout — empty bus lists, so `output_base() == 0`
/// and the input/output directions share one flat channel range in place.
/// This is the common case (`launch.rs` collapses `<= 1` bus per direction to
/// it) and the one where the input-echo manifests.
fn single_bus_layout() -> SlabLayout {
    SlabLayout {
        channels: ChannelLayout::Stereo,
        samples_per_channel: BATCH_SIZE,
        format: SampleFormat::Float32,
        inputs: smallvec![],
        outputs: smallvec![],
    }
}

/// A bridge wired to a mock plugin server whose `ProcessAudio` step reads each
/// input channel out of the shared slab, multiplies by [`GAIN`], and writes the
/// result back in place before replying `AudioProcessed` — exactly what a real
/// gain plugin's subprocess does.
fn bridge_with_doubling_server() -> (Arc<PluginBridge>, BridgeThread, std::thread::JoinHandle<()>) {
    use interprocess::local_socket::{traits::Listener as _, ListenerOptions, ToFsName as _};

    let path = unique_socket_path("process-sync");
    let _ = std::fs::remove_file(&path);
    let name = path
        .clone()
        .to_fs_name::<interprocess::local_socket::GenericFilePath>()
        .unwrap();
    let listener = ListenerOptions::new().name(name).create_sync().unwrap();

    let layout = single_bus_layout();
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
                        let n = data.num_samples;
                        for ch in 0..CHANNELS {
                            let got = server_slab
                                .read_channel_into(ch, &mut scratch[..n])
                                .unwrap_or(0);
                            for s in scratch[..got].iter_mut() {
                                *s *= GAIN;
                            }
                            server_slab.write_channel(ch, &scratch[..got]).unwrap();
                        }
                        // Echo the request's buffer_id, exactly as the real
                        // server does (`Session::handle_process`). This is the
                        // host's only evidence that the slab now holds THIS
                        // block's output.
                        BridgeMessage::AudioProcessed {
                            latency_us: 0,
                            buffer_id: data.buffer_id,
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

/// Drive `blocks` consecutive ramp blocks through a fresh single-bus batcher,
/// returning each block's output as `[block][channel][sample]`.
///
/// `gap` is the pause between blocks, mimicking the spacing of a real audio
/// callback. It was load-bearing against the original bug: with a realistic gap
/// the bridge had time to push the PREVIOUS block's response, so the old
/// non-blocking pop returned a stale reply, whereas with a zero gap the bridge
/// thread never got scheduled and every block saw an empty queue. The two
/// regimes failed differently and both were wrong, so the gap is kept as a
/// parameter to pin the realistic one.
fn drive_blocks_with_gap(blocks: usize, gap: std::time::Duration) -> Vec<Vec<Vec<f32>>> {
    let (bridge, _bridge_thread, _server) = bridge_with_doubling_server();

    // Single-bus: inputs and outputs share the flat range, output_base = 0 —
    // the case where the in-place input write can clobber the output region.
    let layout = single_bus_layout();
    assert_eq!(
        layout.output_base(),
        0,
        "these tests pin the single-bus in-place case"
    );
    let mut batcher = Batcher::new(
        CHANNELS,
        CHANNELS,
        layout.output_base(),
        SampleFormat::Float32,
        BATCH_SIZE,
    );

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
fn diagnose(block: usize, got: &[Vec<f32>]) -> String {
    let approx = |a: f32, b: f32| (a - b).abs() < 1e-4;
    let all = |f: &dyn Fn(usize, usize) -> f32| {
        (0..CHANNELS).all(|ch| (0..BATCH_SIZE).all(|i| approx(got[ch][i], f(ch, i))))
    };

    if all(&|_, _| 0.0) {
        "silence (process returned false — no reply was waiting)".to_string()
    } else if all(&|ch, i| ramp_sample(block, ch, i)) {
        "this block's input at UNITY gain — the host read its own input back \
         (in-place slab echo, output_base == 0)"
            .to_string()
    } else if block > 0 && all(&|ch, i| ramp_sample(block - 1, ch, i) * GAIN) {
        "the PREVIOUS block's processed audio — one block late".to_string()
    } else {
        format!("unrecognised: first sample {}", got[0][0])
    }
}

/// The core contract: for every block, the output is THAT SAME block's input
/// times [`GAIN`]. The ramp makes "one block late" detectable and the x2 gain
/// makes "input echoed back" detectable.
///
/// Fails at HEAD: `AudioBridge::process` pushes `Command::Process` then does a
/// single non-blocking pop for the reply, so nothing waits for the bridge
/// thread.
#[test]
fn plugin_process_returns_current_block() {
    let blocks = 4;
    let captured = drive_blocks_with_gap(blocks, CALLBACK_GAP);

    let mut failures = Vec::new();
    for (block, got) in captured.iter().enumerate() {
        let ok = (0..CHANNELS).all(|ch| {
            (0..BATCH_SIZE).all(|i| (got[ch][i] - ramp_sample(block, ch, i) * GAIN).abs() < 1e-4)
        });
        if !ok {
            failures.push(format!("  block {block}: got {}", diagnose(block, got)));
        }
    }

    assert!(
        failures.is_empty(),
        "each block's output must be that block's input x{GAIN}, but:\n{}",
        failures.join("\n")
    );
}

/// The steady-state half, pinned separately so it cannot hide behind the
/// block-0 failure above: even after the pipeline has been running for several
/// blocks, block N's output must be block N's input — never block N-1's audio,
/// and never block N's own input echoed back at unity gain.
#[test]
fn plugin_process_steady_state_is_not_stale_or_echoed() {
    let blocks = 6;
    let captured = drive_blocks_with_gap(blocks, CALLBACK_GAP);

    // Skip the first two blocks: this test is about steady state, not startup.
    for block in 2..blocks {
        let got = &captured[block];

        let echoed = (0..CHANNELS)
            .all(|ch| (0..BATCH_SIZE).all(|i| (got[ch][i] - ramp_sample(block, ch, i)).abs() < 1e-4));
        assert!(
            !echoed,
            "block {block}: the host read its own input back at unity gain — \
             the plugin was bypassed entirely (in-place slab echo)"
        );

        let stale = (0..CHANNELS).all(|ch| {
            (0..BATCH_SIZE).all(|i| (got[ch][i] - ramp_sample(block - 1, ch, i) * GAIN).abs() < 1e-4)
        });
        assert!(
            !stale,
            "block {block}: got the PREVIOUS block's processed audio — one block late"
        );

        for ch in 0..CHANNELS {
            for i in 0..BATCH_SIZE {
                let expected = ramp_sample(block, ch, i) * GAIN;
                assert!(
                    (got[ch][i] - expected).abs() < 1e-4,
                    "block {block} ch {ch} sample {i}: expected {expected}, \
                     got {} — {}",
                    got[ch][i],
                    diagnose(block, got)
                );
            }
        }
    }
}
