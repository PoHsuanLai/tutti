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
        inputs: smallvec![ChannelLayout::STEREO],
        outputs: smallvec![ChannelLayout::STEREO],
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
            wait_for_reply(&bridge, block as u64 + 1, gap);
        }
    }

    captured
}

/// Wait until block `seq`'s output has been published, or `budget` elapses.
///
/// Replaces a bare `sleep(gap)`. The sleep encoded a *hope* — that 5 ms is
/// always enough for the mock server to reply — and on a loaded machine it is
/// not: the reply lands after the sleep, the next block reads its slot as
/// unpublished, and the test fails with "block N: got silence". That is a defect
/// in the harness's premise, not in the pipeline, and it was reproducible at
/// roughly 1 run in 8 under heavy CPU load.
///
/// Waiting on the condition rather than on the clock removes the guess. The
/// budget stays only so a genuinely broken pipeline fails the assertion instead
/// of hanging, and it is spent in short slices so the common case still returns
/// promptly.
fn wait_for_reply(bridge: &PluginBridge, seq: u64, budget: std::time::Duration) {
    const SLICE: std::time::Duration = std::time::Duration::from_micros(200);
    let deadline = std::time::Instant::now() + budget;
    while std::time::Instant::now() < deadline {
        if bridge.audio_buffer().has_output(seq) {
            return;
        }
        std::thread::sleep(SLICE);
    }
}

/// Upper bound on how long to wait for a block's reply before giving up and
/// letting the assertion speak.
///
/// Was a fixed inter-block sleep chosen to exceed a 64-sample block's ~1.33 ms
/// at 48 kHz. It is now a *timeout* on [`wait_for_reply`] rather than a
/// duration that is always spent: the common case returns as soon as the server
/// publishes, and a loaded machine gets as much of the budget as it needs
/// instead of failing because 5 ms happened not to be enough.
const CALLBACK_GAP: std::time::Duration = std::time::Duration::from_millis(500);

/// Serialises the tests whose assertions are about wall clock.
///
/// `cargo test` runs test functions on parallel threads, and the tests below
/// pace themselves to the audio callback rate: [`CALLBACK_GAP`] is only long
/// enough for the mock server's reply if the machine is not simultaneously
/// running several other mock servers. Under load a reply lands *after* the gap
/// and the block reads as silence — a real failure of the harness's premise, not
/// of the pipeline, and it surfaces as an unexplained "block N was silent".
///
/// A lock rather than a `--test-threads=1` note: a note is something a future
/// runner has to know, and its absence shows up as a mystifying failure. Same
/// reasoning and shape as `real_plugin_pressure.rs`'s `EXCLUSIVE`.
static EXCLUSIVE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Take the machine for the duration of a timing-sensitive test. Poisoning is
/// irrelevant — the guard protects wall clock, not data — so one panicking test
/// must not wedge every later one.
fn exclusive() -> std::sync::MutexGuard<'static, ()> {
    EXCLUSIVE.lock().unwrap_or_else(|e| e.into_inner())
}

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
    let _lock = exclusive();
    let blocks = 5;
    let captured = drive_blocks_with_gap(blocks, CALLBACK_GAP);

    assert!(
        captured[0].iter().all(|ch| ch.iter().all(|&s| s == 0.0)),
        "block 0 must be silence — nothing has been submitted yet, and emitting \
         anything else means reading a slot nobody published: {}",
        diagnose(0, &captured[0])
    );

    let mut failures = Vec::new();
    for (block, got) in captured.iter().enumerate().skip(1) {
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
    let _lock = exclusive();
    let blocks = 6;
    let captured = drive_blocks_with_gap(blocks, CALLBACK_GAP);

    for (block, got) in captured.iter().enumerate().skip(2) {
        let matches = |b: usize, gain: f32| {
            (0..CHANNELS).all(|ch| {
                (0..BATCH_SIZE).all(|i| (got[ch][i] - ramp_sample(b, ch, i) * gain).abs() < 1e-4)
            })
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

/// The ring slot the host is about to read holds a **different block's real
/// audio**, and the host must emit silence rather than that audio.
///
/// This is the single test that can detect the removal of the sequence check,
/// and it is built to be detectable by construction rather than by luck.
///
/// # Why the obvious version of this test cannot work
///
/// Driving blocks back-to-back and asserting "silence or the correct block"
/// looks like it tests this, but it does not. The mock server only ever
/// publishes the block it was asked for, so the slot a missing reply leaves
/// behind is either untouched (zeros) or already correct. Deleting the sequence
/// check entirely still passes, because there is never a wrong block present to
/// read. The distinction the test claims to make — substituted silence versus a
/// stale slot — is not observable in that setup.
///
/// So the decoy is planted deliberately instead of hoped for. There is no
/// server thread and no timing: the slot's contents are set up directly, which
/// makes the outcome the same on every run and on every machine.
///
/// # The construction
///
/// Sequences start at 1, so block 0 submits seq 1 and collects nothing
/// (`expect_seq` is `None`), and block 1 submits seq 2 and collects seq 1. At
/// `RING_SLOTS == 2`, seq 1 and seq 3 share a slot. Publishing seq 3's audio
/// therefore leaves the slot that block 1 is about to read full of loud,
/// recognisable, *wrong* audio, stamped with a sequence that does not match.
///
/// A host that consults the sequence emits silence. A host that trusts the slot
/// emits [`DECOY`] and fails. The `DECOY` value is far outside the ramp's range
/// so the failure message cannot be confused with an off-by-one.
#[test]
fn a_stale_slot_holding_real_audio_still_yields_silence() {
    /// Unmistakable, and nothing the ramp or the gain can produce.
    const DECOY: f32 = -99.0;
    /// Shares a ring slot with seq 1 — the block the host collects second.
    const DECOY_SEQ: u64 = 1 + RING_SLOTS as u64;

    let (bridge, mut bridge_thread) = bridge_with_no_server();
    let mut batcher = Batcher::new(CHANNELS, CHANNELS, SampleFormat::Float32, BATCH_SIZE);

    // The premise, asserted rather than assumed: if the ring depth changes and
    // these stop sharing a slot, the decoy lands somewhere harmless and the
    // test would silently stop testing anything.
    assert_eq!(
        DECOY_SEQ % RING_SLOTS as u64,
        1 % RING_SLOTS as u64,
        "the decoy must occupy the same ring slot as seq 1"
    );

    let slab = bridge.audio_buffer();
    for ch in 0..CHANNELS {
        slab.write_output(DECOY_SEQ, ch, &[DECOY; BATCH_SIZE])
            .unwrap();
    }
    slab.publish_output(DECOY_SEQ);

    let mut input = BufferVec::<F32>::new(CHANNELS);
    let mut output = BufferVec::<F32>::new(CHANNELS);
    let mut midi_out = MidiEventVec::new();

    // Block 0 submits seq 1 and collects nothing; block 1 submits seq 2 and
    // collects seq 1 — the slot now holding the decoy.
    for block in 0..2 {
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

        for ch in 0..CHANNELS {
            for i in 0..BATCH_SIZE {
                let got = output.at_scalar(ch, i);
                assert_eq!(
                    got,
                    0.0,
                    "block {block} ch {ch} sample {i}: expected silence, got {got}{}",
                    if got == DECOY {
                        " — this is the decoy, so the sequence check is not being \
                         consulted and a stale ring slot is being played as though \
                         it were current audio"
                    } else {
                        ""
                    }
                );
            }
        }
    }

    // The decoy must still be sitting there: if something overwrote it, the
    // silence above proves nothing about the sequence check.
    let mut probe = [0.0f32; BATCH_SIZE];
    let got = slab.read_output_into(DECOY_SEQ, 0, &mut probe).unwrap_or(0);
    assert!(
        got == BATCH_SIZE && probe.iter().all(|&s| s == DECOY),
        "the decoy was overwritten during the run, so this test no longer \
         distinguishes substituted silence from a stale slot read"
    );

    bridge_thread.shutdown();
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
/// The limit is expressed **per block-plugin step**, and derived from the wait
/// budget this test exists to detect rather than picked as a round number.
///
/// An earlier version asserted a 300 ms wall-clock bound over the whole test.
/// That could not do its job: rig setup alone (three mock servers, each with a
/// 50 ms startup settle) accounted for ~150 ms of it, so the budget left for
/// the measured work was enormous relative to the ~20 us/step it actually
/// costs. Re-inserting the old synchronous wait — 667 us per plugin per block,
/// half the block period — still came in under the bound and still passed.
///
/// So only the driving loop is timed, and the bound is
/// [`SYNC_WAIT_BUDGET`] / 4: comfortably above the real cost (~30x headroom,
/// measured), and comfortably below a single re-inserted wait. The margin is
/// what keeps it from flaking on a loaded box; the derivation is what keeps it
/// meaningful. If the pipelining regresses, ONE waited block trips it.
#[test]
fn stalled_plugins_do_not_stall_the_audio_thread() {
    let _lock = exclusive();
    use std::time::{Duration, Instant};

    const PLUGINS: usize = 3;
    const BLOCKS: usize = 20;
    /// Far beyond any block period, so a design that waits cannot hide it.
    const SERVER_STALL: Duration = Duration::from_millis(10);
    /// What the synchronous design spent per plugin per block: half the block
    /// period at 64 samples / 48 kHz. This is the quantity under test.
    const SYNC_WAIT_BUDGET: Duration = Duration::from_micros(667);
    /// Per block-plugin step. See the note on the threshold above.
    const STEP_LIMIT: Duration = Duration::from_micros(SYNC_WAIT_BUDGET.as_micros() as u64 / 4);

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

    let steps = (BLOCKS * PLUGINS) as u32;
    let per_step = elapsed / steps;
    // What the old design would have spent: every plugin, every block, waiting
    // out its budget before giving up.
    let synchronous_floor = SYNC_WAIT_BUDGET * steps;
    assert!(
        per_step < STEP_LIMIT,
        "{PLUGINS} stalled plugins x {BLOCKS} blocks took {elapsed:?} — \
         {per_step:?} per block-plugin step, over the {STEP_LIMIT:?} limit. \
         A synchronous design waits {SYNC_WAIT_BUDGET:?} per step \
         (~{synchronous_floor:?} total), so this looks like the audio thread is \
         waiting for replies again."
    );

    for (_, thread, _) in rigs.iter_mut() {
        thread.shutdown();
    }
}

/// A bridge whose socket has no server behind it: the listener is created,
/// dropped, and never accepts. Every dispatch therefore fails, which is how the
/// bridge marks itself crashed through its ordinary path rather than through a
/// test-only setter.
///
/// Returned deliberately without a server thread — the point is a bridge that
/// never publishes to the slab.
fn bridge_with_no_server() -> (Arc<PluginBridge>, BridgeThread) {
    let path = unique_socket_path("no-server");
    let _ = std::fs::remove_file(&path);

    let layout = stereo_layout();
    let shm_name = unique_shm_name("no-server");
    let host_slab = Arc::new(AudioSlab::create(shm_name, layout).unwrap());

    let (bridge, bridge_thread) = PluginBridge::new(
        path,
        host_slab,
        std::path::PathBuf::from("test.vst3"),
        48_000.0,
    )
    .unwrap();

    (bridge, bridge_thread)
}

/// Drive `blocks` blocks through `batcher`, asserting nothing but that it runs.
/// Split out so the warm-up and the guarded region execute *identical* code —
/// if they diverged, the guarded region could take a colder path and the
/// one-shot allocations would land inside the assertion.
fn drive_one_block(
    batcher: &mut Batcher,
    bridge: &PluginBridge,
    input: &BufferVec<F32>,
    output: &mut BufferVec<F32>,
    midi_out: &mut MidiEventVec,
) {
    batcher.process::<f32>(
        bridge,
        BATCH_SIZE,
        &input.buffer_ref(),
        &mut output.buffer_mut(),
        BlockPayload::default(),
        midi_out,
    );
}

/// The silence substitutions are audio-thread code, so they must not allocate.
///
/// Both branches are reached through `Batcher::collectable` returning `None`,
/// and both are new with the pipelined design — under the synchronous one the
/// audio thread waited for a reply, so "nothing to collect" was not a per-block
/// path. They are the branches taken whenever a plugin is late, absent or dead,
/// which is exactly when the callback can least afford a malloc.
///
/// # What this does and does not prove
///
/// [`assert_no_alloc`] arms a *thread-local* counter, and the detection lives in
/// the `AllocDisabler` global allocator registered below. A violation calls
/// `handle_alloc_error`, which aborts rather than unwinds — so a regression here
/// kills the test process rather than reporting a tidy failure. That is the
/// harness working, not a broken test.
///
/// Registering a global allocator is process-wide, but the forbid counter is
/// not: unrelated tests in this binary allocate freely, and only the closures
/// below are guarded.
#[global_allocator]
static ALLOC: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

/// A live-but-slow plugin must not push allocations onto the audio thread.
///
/// This is the case the pipelining exists for, and the one that found the bug:
/// the payload pool held 4 entries against a 128-deep command queue, and a
/// payload is recycled only when the bridge thread dequeues its command. So the
/// audio thread outrunning a stalled bridge — the definition of this scenario —
/// starved the pool from block 5 on and allocated ~9 KiB per block thereafter,
/// exactly when the plugin was already failing to keep up.
///
/// Kept separate from the two silence tests below because it fails for a
/// different reason: those cover branches where no audio arrives at all, while
/// this one covers the path where everything is nominally working and the
/// bridge is merely behind.
#[test]
fn a_stalled_but_live_server_does_not_allocate_on_the_audio_thread() {
    let (bridge, mut bridge_thread, _server) =
        bridge_with_server_stall(std::time::Duration::from_millis(10));
    let mut batcher = Batcher::new(CHANNELS, CHANNELS, SampleFormat::Float32, BATCH_SIZE);

    let mut input = BufferVec::<F32>::new(CHANNELS);
    let mut output = BufferVec::<F32>::new(CHANNELS);
    for ch in 0..CHANNELS {
        for i in 0..BATCH_SIZE {
            input.set_scalar(ch, i, ramp_sample(0, ch, i));
        }
    }
    let mut midi_out = MidiEventVec::new();

    for _ in 0..8 {
        drive_one_block(&mut batcher, &bridge, &input, &mut output, &mut midi_out);
    }

    // Back-to-back blocks with no gap: the audio thread outruns a stalled
    // bridge thread, which is exactly the scenario the pipelining exists for.
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..64 {
            drive_one_block(&mut batcher, &bridge, &input, &mut output, &mut midi_out);
        }
    });

    bridge_thread.shutdown();
}

/// Nothing has ever been published, so every block takes the sequence-mismatch
/// path into `silence_block`.
#[test]
fn silence_on_a_missing_reply_does_not_allocate() {
    let (bridge, mut bridge_thread) = bridge_with_no_server();
    let mut batcher = Batcher::new(CHANNELS, CHANNELS, SampleFormat::Float32, BATCH_SIZE);

    let mut input = BufferVec::<F32>::new(CHANNELS);
    let mut output = BufferVec::<F32>::new(CHANNELS);
    for ch in 0..CHANNELS {
        for i in 0..BATCH_SIZE {
            input.set_scalar(ch, i, ramp_sample(0, ch, i));
        }
    }
    let mut midi_out = MidiEventVec::new();

    // Warm up OUTSIDE the guard. The batcher's tick/wire storage and the
    // payload pool allocate on first use by design; the contract under test is
    // steady state, not first block.
    for _ in 0..8 {
        drive_one_block(&mut batcher, &bridge, &input, &mut output, &mut midi_out);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..64 {
            drive_one_block(&mut batcher, &bridge, &input, &mut output, &mut midi_out);
        }
    });

    // The branch under test is only meaningful if it actually ran: a bridge
    // that somehow produced audio would make the assertion above vacuous.
    assert!(
        (0..CHANNELS).all(|ch| (0..BATCH_SIZE).all(|i| output.at_scalar(ch, i) == 0.0)),
        "expected the silence branch, but the output was not silent — this test \
         no longer covers what it claims to"
    );

    bridge_thread.shutdown();
}

/// A crashed bridge must reach silence without allocating.
///
/// **This does not prove the crash short-circuit is separately covered**, and an
/// earlier version of this comment claimed it did. Deleting the `is_crashed()`
/// check in `Batcher::collectable` leaves the entire suite green: a crashed
/// bridge never publishes, so the sequence check below it returns `None` anyway
/// — the same silence, by a longer route. The check earns its place by making
/// that outcome a decision rather than a coincidence of the numbering, which is
/// a design argument rather than a tested one.
///
/// What this test does cover is real: the crashed path is the one where every
/// dispatch fails, and it must still not allocate on the audio thread. That is
/// the claim in the name, and it is the claim being checked.
#[test]
fn silence_on_a_crashed_bridge_does_not_allocate() {
    let (bridge, mut bridge_thread) = bridge_with_no_server();
    let mut batcher = Batcher::new(CHANNELS, CHANNELS, SampleFormat::Float32, BATCH_SIZE);

    let mut input = BufferVec::<F32>::new(CHANNELS);
    let mut output = BufferVec::<F32>::new(CHANNELS);
    for ch in 0..CHANNELS {
        for i in 0..BATCH_SIZE {
            input.set_scalar(ch, i, ramp_sample(0, ch, i));
        }
    }
    let mut midi_out = MidiEventVec::new();

    // Drive until the failed dispatches have marked the bridge crashed, so the
    // guarded region below exercises the crash branch rather than the mismatch
    // one. Bounded rather than unbounded: if the flag never sets, the test
    // should say so instead of hanging.
    let mut crashed = false;
    for _ in 0..200 {
        drive_one_block(&mut batcher, &bridge, &input, &mut output, &mut midi_out);
        if bridge.is_crashed() {
            crashed = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(
        crashed,
        "the bridge never marked itself crashed, so this test would have \
         measured the sequence-mismatch branch instead"
    );

    // Warm up again post-crash: the first block through the short-circuit may
    // still touch cold storage.
    for _ in 0..8 {
        drive_one_block(&mut batcher, &bridge, &input, &mut output, &mut midi_out);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..64 {
            drive_one_block(&mut batcher, &bridge, &input, &mut output, &mut midi_out);
        }
    });

    assert!(
        (0..CHANNELS).all(|ch| (0..BATCH_SIZE).all(|i| output.at_scalar(ch, i) == 0.0)),
        "a crashed bridge must emit silence"
    );

    bridge_thread.shutdown();
}
