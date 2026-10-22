//! Pins the out-of-process audio pipeline: output lags input by EXACTLY one
//! block, and anything else is silence rather than wrong audio.
//!
//! The failure these guard against: if a single-bus plugin's input and output
//! regions alias (`output_base == 0`), a block's input write overwrites the
//! region the output read draws from, so the host reads its own input back at
//! unity gain — a silent bypass.
//!
//! **One block of lag is the correct answer here.** The audio thread never
//! waits for a reply: it submits block N and collects block N-1, because
//! waiting sums across serially-run nodes and overruns the callback (see
//! `batcher`'s module docs). That lag is declared to PDC, so it is compensated
//! rather than heard. What must never happen is wrong audio: a wrong block, or
//! the input echoed back.
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
use crate::host::node::batcher::{Batcher, ChunkCounters};

/// The chunk these tests ship: the slab ceiling they build each batcher and
/// slab with, and the block they drive (so every call is one whole chunk).
const BATCH_SIZE: usize = 64;
use crate::host::node::BlockPayload;
use crate::protocol::{
    BridgeMessage, ChannelLayout, HostMessage, MidiEventVec, SampleFormat, SlabLayout,
    PROTOCOL_VERSION,
};
use crate::util::transport::shm::{AudioSlab, RING_SLOTS};
use std::sync::Arc;

const CHANNELS: usize = 2;
const GAIN: f32 = 2.0;

/// The tests' side of each chunk: an empty payload, and the plugin's MIDI-out
/// kept as collected, so a test reads what the bridge returned.
impl super::batcher::Chunks for MidiEventVec {
    fn begin(&mut self, _at: usize, _chunk: usize) {}
    fn take(&mut self, _from: usize, _n: usize, _at: usize) {}

    fn payload(&mut self, _frames: usize) -> BlockPayload {
        BlockPayload::default()
    }

    fn midi_out(&mut self, events: &MidiEventVec) {
        self.clear();
        self.try_extend_from_slice(events)
            .expect("an emptied list holds a full one");
    }

    fn emit(&mut self, _frame: usize, _event: crate::protocol::MidiEvent) {}
}

/// `CHANNELS` planar channels of `BATCH_SIZE` frames: the buffers the
/// batcher is handed, as the plugin node hands them. Fixed-size arrays so a
/// test can borrow them as the batcher's slice-of-slices without allocating
/// (the no-alloc tests run the borrow inside the guard).
struct Planar([Vec<f32>; CHANNELS]);

impl Planar {
    fn new() -> Self {
        Self(std::array::from_fn(|_| vec![0.0; BATCH_SIZE]))
    }

    fn set_scalar(&mut self, ch: usize, i: usize, v: f32) {
        self.0[ch][i] = v;
    }

    fn at_scalar(&self, ch: usize, i: usize) -> f32 {
        self.0[ch][i]
    }

    fn clear(&mut self) {
        for c in &mut self.0 {
            c.fill(0.0);
        }
    }

    fn ins(&self) -> [&[f32]; CHANNELS] {
        self.0.each_ref().map(Vec::as_slice)
    }

    fn outs(&mut self) -> [&mut [f32]; CHANNELS] {
        self.0.each_mut().map(Vec::as_mut_slice)
    }
}

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

/// One stereo bus per direction — the common shape, and the one most easily
/// collapsed onto offset 0. Each direction gets its own region, which makes the
/// input-echo failure structurally impossible rather than merely unlikely.
fn stereo_layout() -> SlabLayout {
    SlabLayout {
        samples_per_channel: BATCH_SIZE,
        format: SampleFormat::Float32,
        slots: RING_SLOTS as u32,
        inputs: vec![ChannelLayout::STEREO],
        outputs: vec![ChannelLayout::STEREO],
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
    bridge_with_server(stereo_layout(), stall, |out| out)
}

/// The mock server over `layout`: output channel `ch` is input channel
/// `route(ch)` times [`GAIN`], after sleeping `stall`.
fn bridge_with_server(
    layout: SlabLayout,
    stall: std::time::Duration,
    route: fn(usize) -> usize,
) -> (Arc<PluginBridge>, BridgeThread, std::thread::JoinHandle<()>) {
    use interprocess::local_socket::{traits::Listener as _, ListenerOptions};

    let path = unique_socket_path("process-sync");
    let _ = std::fs::remove_file(&path);
    let name = crate::util::transport::control::socket_name(&path).unwrap();
    let listener = ListenerOptions::new().name(name).create_sync().unwrap();

    let out_channels: usize = layout.outputs.iter().map(|l| l.count() as usize).sum();
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
                            for ch in 0..out_channels {
                                let got = server_slab
                                    .read_input_into(seq, route(ch), &mut scratch[..n])
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

/// **A sidechain input reaches the plugin.** A stereo main bus plus a mono
/// sidechain is three input ports; the batcher stages every one into the
/// matching channel of the slab's input region, so a graph edge into the
/// node's port 2 is what the plugin reads on its sidechain. The mock server
/// here outputs the sidechain (input 2) on output 0 and the main left
/// (input 0) on output 1, one chunk late.
///
/// Mutation: stage `input.iter().take(2)` (the main bus only) in
/// `Batcher::process` → output 0 reads silence → fails.
#[test]
fn a_sidechain_port_reaches_the_plugin() {
    let _lock = exclusive();
    let layout = SlabLayout {
        inputs: vec![ChannelLayout::STEREO, ChannelLayout::MONO],
        ..stereo_layout()
    };
    let (bridge, _thread, _server) =
        bridge_with_server(layout, std::time::Duration::ZERO, |out| [2, 0][out]);
    let mut batcher = Batcher::new(3, CHANNELS, SampleFormat::Float32, BATCH_SIZE);
    let main = vec![1.0f32; BATCH_SIZE];
    let side = vec![5.0f32; BATCH_SIZE];
    let mut out = Planar::new();
    let mut midi_out = MidiEventVec::new();
    for block in 0..2u64 {
        out.clear();
        batcher.process(
            &bridge,
            BATCH_SIZE,
            &[&main, &main, &side],
            &mut out.outs(),
            &mut midi_out,
        );
        wait_for_reply(&bridge, block + 1, std::time::Duration::from_secs(2));
    }
    assert_eq!(
        out.at_scalar(0, 0),
        5.0 * GAIN,
        "the sidechain, on output 0"
    );
    assert_eq!(out.at_scalar(1, 0), 1.0 * GAIN, "the main bus, on output 1");
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

    // The two directions are separately sized and separately addressed; the
    // output region must not alias the input region.
    let layout = stereo_layout();
    assert!(
        layout.input_ring_bytes() > 0 && layout.output_ring_bytes() > 0,
        "both directions must have their own region"
    );
    let mut batcher = Batcher::new(CHANNELS, CHANNELS, SampleFormat::Float32, BATCH_SIZE);

    let mut midi_out = MidiEventVec::new();
    let mut input = Planar::new();
    let mut output = Planar::new();
    let mut captured = Vec::with_capacity(blocks);

    for block in 0..blocks {
        for ch in 0..CHANNELS {
            for i in 0..BATCH_SIZE {
                input.set_scalar(ch, i, ramp_sample(block, ch, i));
            }
        }
        output.clear();

        batcher.process(
            &bridge,
            BATCH_SIZE,
            &input.ins(),
            &mut output.outs(),
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

/// Wait until block `seq`'s output has been published, **panicking by name if
/// the budget is spent instead**.
///
/// Replaces a bare `sleep(gap)`. The sleep encoded a *hope* — that 5 ms is
/// always enough for the mock server to reply — and on a loaded machine it is
/// not: the reply lands after the sleep, the next block reads its slot as
/// unpublished, and the test fails with "block N: got silence". That is a defect
/// in the harness's premise, not in the pipeline, and it was reproducible at
/// roughly 1 run in 8 under heavy CPU load.
///
/// Waiting on the condition rather than on the clock removed the guess for the
/// common case. What it did *not* remove is what happens when the budget runs
/// out anyway: the function returned quietly, the caller drove the next block
/// against an unpublished slot, and the failure surfaced downstream as
/// "block N: got silence (nothing was collectable for this block)" — the
/// diagnosis for a *broken pipeline*. So a slow machine was reported as a
/// pipeline defect, which is precisely the misattribution the condition-wait
/// was introduced to end. The two outcomes have to be distinguishable at the
/// point where they differ, and only this function knows which one happened.
///
/// It now panics, naming the budget and the block. That is not a stricter
/// test — the caller was going to fail either way — it is the same failure with
/// the right cause on it.
fn wait_for_reply(bridge: &PluginBridge, seq: u64, budget: std::time::Duration) {
    const SLICE: std::time::Duration = std::time::Duration::from_micros(200);
    let deadline = std::time::Instant::now() + budget;
    while std::time::Instant::now() < deadline {
        if bridge.audio_buffer().has_output(seq) {
            return;
        }
        std::thread::sleep(SLICE);
    }
    panic!(
        "the mock server did not publish block {seq}'s output within {budget:?}. \
         This is a HARNESS timeout, not a pipeline defect: the assertions that \
         follow would have reported it as \"block N: got silence\", which is the \
         diagnosis for a pipeline that collected nothing. If this fires, the \
         machine is slower than the budget assumes — raise WAIT_BUDGET — or the \
         mock server thread died, which would also stall every later block."
    );
}

/// How long [`wait_for_reply`] will wait for a block's reply before declaring
/// the harness stuck.
///
/// Was a fixed inter-block sleep chosen to exceed a 64-sample block's ~1.33 ms
/// at 48 kHz. It is now a *timeout* rather than a duration that is always
/// spent: the common case returns as soon as the server publishes, in
/// microseconds.
///
/// Generous on purpose, and it costs nothing to be. Since the wait is on a
/// condition, this bound is only reached when the reply never comes at all — a
/// dead server thread, or a pipeline that stopped publishing — and in that case
/// the test is failing regardless of how long it waited first. Sizing it to
/// "how slow could a loaded machine plausibly be?" is the wrong question: the
/// answer has no upper bound anyone can name, and every value that is not
/// generous enough converts a slow machine into a false failure. Ten seconds is
/// far beyond any real scheduling delay for a mock server that does two memcpys,
/// and still bounded enough that a genuinely wedged run fails rather than hangs
/// a CI job.
///
/// It was 500 ms, and a 500 ms budget silently expiring is exactly what
/// produced the "block N was silent" reports this file's history records.
const WAIT_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

/// Serialises the tests that each stand up their own mock server.
///
/// `cargo test` runs test functions on parallel threads, and every test below
/// spawns a socket listener, a bridge thread and a server thread. Running
/// several at once is not *incorrect* — the socket paths and shm names are
/// per-process unique — but it multiplies the scheduling pressure each one's
/// reply has to get through.
///
/// [`WAIT_BUDGET`] is a deadlock guard rather than a deadline anything races,
/// so this lock is not what keeps the tests honest — [`wait_for_reply`] is. It
/// stays because there is no reason to pay the contention: these tests are
/// milliseconds each, and running them in series costs nothing while keeping
/// any future timing assertion safe from scheduling pressure.
///
/// A lock rather than a `--test-threads=1` note: a note is something a future
/// runner has to know, and its absence shows up as a mystifying failure.
///
/// **It is the machine-wide lock ([`machine_lock`](super::machine_lock)), not
/// a `static Mutex`.** It was a `Mutex`, which under `cargo nextest` (one
/// process per test) serialized nothing — not these tests against each other,
/// and not against the suites that spin a real `plugin-server` for seconds.
/// The drain test's exact count of abandoned replies is only as good as the
/// wall clock each block gets; see that module's docs for the failure it
/// produced.
fn exclusive() -> super::machine_lock::Guard {
    super::machine_lock::acquire()
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
    let captured = drive_blocks_with_gap(blocks, WAIT_BUDGET);

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
    let captured = drive_blocks_with_gap(blocks, WAIT_BUDGET);

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

    let mut input = Planar::new();
    let mut output = Planar::new();
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
        batcher.process(
            &bridge,
            BATCH_SIZE,
            &input.ins(),
            &mut output.outs(),
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
/// in series must not make the audio thread wait for their replies: not on
/// most steps, not on most of any one plugin's steps, and not on most blocks.
/// A long wait on only a few steps is not covered; see the end of this comment.
///
/// # Why waiting fails
///
/// A synchronous design is individually defensible — each plugin waits at most
/// half its own block period before giving up and emitting silence. But the
/// graph's serial executor runs nodes one after another within one callback,
/// so the budgets sum:
///
/// | Stalled plugins | Spent waiting | vs the 1333 us period @ 64/48k |
/// |---|---|---|
/// | 1 | 667 us | 0.50x — fine |
/// | 2 | 1333 us | 1.00x — at the edge |
/// | **3** | **2000 us** | **1.50x — overrun** |
/// | 8 | 5333 us | 4.00x — overrun |
///
/// Three concurrently-stalled plugins blow the callback; measured under 24x CPU
/// load, 4 of 12 blocks made their deadline. Parallelising the executor would
/// not help — plugins in series on one track are a dependency chain, and that
/// is the common arrangement. The problem is the waiting, not the serialism.
///
/// The loop below drives the plugins one after another within each block, which
/// is exactly the arrangement whose budgets would sum if the thread waited.
///
/// # On the threshold
///
/// The limit is expressed **per block-plugin step**, and derived from the wait
/// budget this test exists to detect rather than picked as a round number:
/// [`SYNC_WAIT_BUDGET`] / 4, far above the real cost (a 7-11 us median step
/// measured, debug build) and far below a single re-inserted wait.
///
/// A wall-clock bound over the *whole* test cannot do this job: rig setup alone
/// (three mock servers, each with a 50 ms startup settle) accounts for ~150 ms,
/// so the budget left for the measured work is enormous relative to what it
/// actually costs. Only the driving loop is timed.
///
/// # Why medians, not the total
///
/// The two errors the clock can make are not symmetric, and the assertions are
/// built on that:
///
/// - **A wait is a lower bound.** A design that waits for the reply sleeps
///   until the reply comes or it gives up, and no scheduler makes a sleep
///   shorter. The server is single-threaded and stalls 10 ms per block, so it
///   falls further behind with every block, and a step that waits for it takes
///   at least [`SYNC_WAIT_BUDGET`].
/// - **Load only adds time, and only to the steps it lands on.** A descheduled
///   thread makes the step it is in longer; it cannot touch the others.
///
/// So each assertion is a median, which a minority of slow samples cannot
/// move, over a population in which a waiting design makes at least half the
/// samples slow:
///
/// - **every step** (60): a design that waits on every step;
/// - **each plugin's steps** (20 each): one plugin that waits on every block,
///   such as a per-callback budget spent by whichever plugin runs first;
/// - **each block's summed steps** (20), against `PLUGINS` limits: waits spread
///   thinly, such as one on every 4th step, which put a wait in 3 of 4 blocks.
///
/// A correct design fails one only if the scheduler takes the core away for
/// over the limit in at least half of that population's samples individually.
///
/// An earlier version asserted the *mean* (total / 60) and failed on macOS CI
/// at 285 us/step (17 ms in all, on a docs-only PR). The log shows only the
/// total; the likely cause is a few long deschedulings, which the division
/// spreads across all 60 steps until they read like a wait on each. Bursty
/// load pinned to one core reproduced that shape (a ~10 us median, a handful
/// of steps of 10-190 ms) and failed the mean every run.
///
/// **Not covered:** a long wait on only a few steps, such as one 10 ms wait
/// per plugin. The mean caught that, but it is exactly what a descheduling
/// looks like, and no wall-clock measurement here can tell the two apart.
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

    // Block 1 has nothing in flight, so nothing to wait for; every later block
    // collects one the 10 ms server is behind on. A median of `n` samples is
    // the `n / 2`-th smallest, so it is over the limit once `n - n / 2` samples
    // are: a waiting design must be able to make that many slow in each
    // population. Per plugin and per block, that is `BLOCKS - 1` of `BLOCKS`;
    // over every step, `PLUGINS` times as many of `PLUGINS` times as many.
    // (`BLOCKS - 1 >= BLOCKS - BLOCKS / 2`, written as clippy prefers it.)
    const _: () = assert!(
        BLOCKS > BLOCKS - BLOCKS / 2,
        "too few blocks for a waiting design to own each median"
    );

    let mut rigs: Vec<_> = (0..PLUGINS)
        .map(|_| bridge_with_server_stall(SERVER_STALL))
        .collect();
    let mut batchers: Vec<_> = (0..PLUGINS)
        .map(|_| Batcher::new(CHANNELS, CHANNELS, SampleFormat::Float32, BATCH_SIZE))
        .collect();

    let mut input = Planar::new();
    let mut output = Planar::new();
    for ch in 0..CHANNELS {
        for i in 0..BATCH_SIZE {
            input.set_scalar(ch, i, ramp_sample(0, ch, i));
        }
    }
    let mut midi_out = MidiEventVec::new();

    // Each step timed on its own, so one descheduling costs one sample of the
    // distribution rather than the whole measurement. Allocated up front: the
    // loop under test pushes into it. Block-major: step `b * PLUGINS + p` is
    // plugin `p` in block `b`.
    let mut step_times = Vec::with_capacity(BLOCKS * PLUGINS);
    for _ in 0..BLOCKS {
        for (batcher, (bridge, _, _)) in batchers.iter_mut().zip(rigs.iter()) {
            output.clear();
            let step = Instant::now();
            batcher.process(
                bridge,
                BATCH_SIZE,
                &input.ins(),
                &mut output.outs(),
                &mut midi_out,
            );
            step_times.push(step.elapsed());
        }
    }

    fn median(samples: impl Iterator<Item = Duration>) -> Duration {
        let mut sorted: Vec<Duration> = samples.collect();
        sorted.sort_unstable();
        sorted[sorted.len() / 2]
    }
    let total: Duration = step_times.iter().sum();
    let slow = step_times.iter().filter(|&&t| t >= STEP_LIMIT).count();
    let slowest = step_times.iter().max().copied().unwrap_or_default();
    let steps = step_times.len();
    let shape = format!(
        "{slow} of {steps} steps over {STEP_LIMIT:?}, slowest {slowest:?}, \
         {total:?} in all"
    );
    let regressed = format!(
        "A synchronous design waits {SYNC_WAIT_BUDGET:?} per plugin per block, \
         so this looks like the audio thread is waiting for replies again."
    );

    // Mutation: make the live path wait — in `Batcher::collect`, before
    // `collectable`, poll `has_output(expect_seq)` every 50 us until a 667 us
    // deadline. Idle, every step from block 2 on then takes >= 667 us (57 of
    // 60 over the limit); under bursty load on one core, 45-56 of 60. Either
    // way the median is one of them: it failed 6/6 runs, idle and loaded.
    let every_step = median(step_times.iter().copied());
    assert!(
        every_step < STEP_LIMIT,
        "{PLUGINS} stalled plugins x {BLOCKS} blocks: the median step took \
         {every_step:?}, over the {STEP_LIMIT:?} limit ({shape}). {regressed}"
    );

    // Mutation: the same wait on plugin 0's steps only (a call counter shared
    // by the three batchers, waiting when it is 0 mod 3) → 19-20 of 60 steps
    // slow, so the median above stays fast, and plugin 0's median is ~750 us
    // → fails here, 4/4 runs, idle and loaded. (Its blocks would also fail
    // the block median below; a 300 us wait, steps of ~450 us, fails here
    // too.)
    for p in 0..PLUGINS {
        let plugin = median(step_times.iter().skip(p).step_by(PLUGINS).copied());
        assert!(
            plugin < STEP_LIMIT,
            "plugin {p}'s median step took {plugin:?}, over the {STEP_LIMIT:?} \
             limit ({shape}). {regressed}"
        );
    }

    // Mutation: the same wait on every 4th step only (the shared counter, 0
    // mod 4) → 15 of 60 steps slow and 5 of each plugin's 20, so neither
    // median above moves, but 15 of 20 blocks hold a wait and the median
    // block takes ~740 us → fails here, 4/4 runs, idle and loaded.
    let block_limit = STEP_LIMIT * PLUGINS as u32;
    let block = median(
        step_times
            .chunks(PLUGINS)
            .map(|b| b.iter().sum::<Duration>()),
    );
    assert!(
        block < block_limit,
        "the median block of {PLUGINS} steps took {block:?}, over the \
         {block_limit:?} limit ({shape}). {regressed}"
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
    input: &Planar,
    output: &mut Planar,
    midi_out: &mut MidiEventVec,
) {
    batcher.process(
        bridge,
        BATCH_SIZE,
        &input.ins(),
        &mut output.outs(),
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

    let mut input = Planar::new();
    let mut output = Planar::new();
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

/// The pipeline's counters report what the node played: a chunk in flight
/// reads unanswered until the server answers it, a block collected after the
/// answer is no miss, and a block that outruns a stalled server is counted.
///
/// Mutations, each run and each failing: dropping the `missed` `fetch_add` in
/// `collect` (0 missed after the back-to-back blocks); storing 0 for
/// `in_flight` in `submit`, or answering `in_flight_answered` without asking
/// the slab (answered while the server still sleeps); skipping the wait for
/// the answer (the next block is counted a miss).
#[test]
fn the_chunk_counters_report_misses_and_answers() {
    let (bridge, mut bridge_thread, _server) =
        bridge_with_server_stall(std::time::Duration::from_millis(50));
    let counters = Arc::new(ChunkCounters::default());
    counters.attach(bridge.audio_buffer());
    let mut batcher = Batcher::new(CHANNELS, CHANNELS, SampleFormat::Float32, BATCH_SIZE)
        .with_counters(Arc::clone(&counters));

    let mut input = Planar::new();
    let mut output = Planar::new();
    for ch in 0..CHANNELS {
        for i in 0..BATCH_SIZE {
            input.set_scalar(ch, i, ramp_sample(0, ch, i));
        }
    }
    let mut midi_out = MidiEventVec::new();

    assert!(counters.in_flight_answered(), "nothing in flight yet");
    drive_one_block(&mut batcher, &bridge, &input, &mut output, &mut midi_out);
    assert!(
        !counters.in_flight_answered(),
        "a chunk the stalled server has not answered read as answered"
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !counters.in_flight_answered() {
        assert!(
            std::time::Instant::now() < deadline,
            "the server never answered"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    drive_one_block(&mut batcher, &bridge, &input, &mut output, &mut midi_out);
    assert_eq!(counters.missed(), 0, "an answered chunk was counted missed");

    // Back to back: each block collects a chunk the server is still sleeping on.
    for _ in 0..4 {
        drive_one_block(&mut batcher, &bridge, &input, &mut output, &mut midi_out);
    }
    assert!(
        counters.missed() > 0,
        "blocks that outran a stalled server were not counted"
    );

    bridge_thread.shutdown();
}

/// Nothing has ever been published, so every block takes the sequence-mismatch
/// path into `silence_block`.
#[test]
fn silence_on_a_missing_reply_does_not_allocate() {
    let (bridge, mut bridge_thread) = bridge_with_no_server();
    let mut batcher = Batcher::new(CHANNELS, CHANNELS, SampleFormat::Float32, BATCH_SIZE);

    let mut input = Planar::new();
    let mut output = Planar::new();
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

    let mut input = Planar::new();
    let mut output = Planar::new();
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

/// **A reply that arrives late must cost one block, not the session.**
///
/// `dispatch::process_timeout` is documented as the budget after which the
/// bridge thread "abandons the block" — a per-block decision, and the right one:
/// a plugin that misses one deadline is slow, not dead, and the slab's sequence
/// check already turns a missing reply into silence for that block alone.
///
/// But the timeout surfaces as `BridgeError::Timeout` out of `recv_reply`, and
/// `handle` returns it through `?` into `pump`, which treats *every* `handle`
/// error as connection-level: it calls `crash()`, latches the cause, drains the
/// queue with errors and **returns**, ending the thread. From then on
/// `Batcher::collectable` short-circuits on `is_crashed()` and every later block
/// is silence, permanently — for a server that is still running, still reading
/// the socket, and still publishing correct audio.
///
/// The budget is `PROCESS_TIMEOUT_PERIODS` block periods clamped to a 2 ms
/// floor. At 64 frames / 48 kHz that is 2 ms of wall clock, on a thread with no
/// scheduling priority, so on a loaded machine the reply losing that race is a
/// property of the machine rather than of the plugin. That is the mechanism
/// behind this file's intermittent failures: the run that fails is the one where
/// the mock server thread was not scheduled inside 2 ms, and the host answered
/// by declaring a working plugin dead.
///
/// The stall here is deliberately far above the budget so the timeout is
/// certain, which makes this a deterministic test of a race — the outcome does
/// not depend on the machine, only on whether one timeout is fatal.
#[test]
fn a_late_reply_does_not_permanently_crash_the_bridge() {
    let _lock = exclusive();
    // Well past `MAX_PROCESS_TIMEOUT`, so the first block's reply is guaranteed
    // to miss its budget however fast the machine is.
    let (bridge, _bridge_thread, _server) =
        bridge_with_server_stall(std::time::Duration::from_millis(120));
    let mut batcher = Batcher::new(CHANNELS, CHANNELS, SampleFormat::Float32, BATCH_SIZE);

    let mut input = Planar::new();
    let mut output = Planar::new();
    let mut midi_out = MidiEventVec::new();

    for ch in 0..CHANNELS {
        for i in 0..BATCH_SIZE {
            input.set_scalar(ch, i, ramp_sample(0, ch, i));
        }
    }
    output.clear();
    batcher.process(
        &bridge,
        BATCH_SIZE,
        &input.ins(),
        &mut output.outs(),
        &mut midi_out,
    );

    // Give the stalled reply time to land, so the only thing under test is what
    // the host did with the missed deadline — not whether the server ever
    // answered.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline && !bridge.audio_buffer().has_output(1) {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }

    assert!(
        bridge.audio_buffer().has_output(1),
        "the mock server never published block 1 at all — this test cannot say \
         anything about the timeout until the server has answered"
    );
    assert!(
        !bridge.is_crashed(),
        "one reply that missed its per-block budget marked the bridge crashed. \
         The server is alive and published block 1's audio correctly; the host \
         has nonetheless latched a crash, so `Batcher::collectable` will return \
         `None` for every future block and this plugin is silent forever. A \
         missed block budget must cost that block, not the connection"
    );
}

/// **After a block whose reply timed out, the socket's replies stay paired with
/// the blocks that asked for them.**
///
/// Not crashing is only half the fix. Abandoning a block does not *cancel* it:
/// the server is mid-`process` and will write that frame onto the stream when it
/// finishes. Leave it there and the next command's `recv_reply` consumes the
/// abandoned block's answer, and every later block is paired with its
/// predecessor's — permanently one behind. `Owed::settle` drains it instead.
///
/// # Why this asserts on MIDI rather than on audio
///
/// Audio cannot detect this, and a version of this test that drove the ramp and
/// checked the one-block lag passed with `settle` deleted. That is not a weak
/// assertion, it is the design: audio travels through the shared slab in both
/// directions and is gated on the slot's own sequence number, so which *socket
/// frame* the bridge thread happened to read is irrelevant to it.
///
/// The reply carries exactly one thing the host consumes — the plugin's
/// MIDI-out — and collecting a chunk drains it (`take_replies`). So MIDI is the
/// only observable that can tell a paired reply from a mispaired one, which
/// makes it the only honest thing to assert. Each block's reply is stamped with
/// a distinct first data word, so an off-by-one pairing names itself.
#[test]
fn a_timed_out_reply_is_drained_rather_than_paired_with_a_later_block() {
    let _lock = exclusive();
    // One stall, far past `MAX_PROCESS_TIMEOUT`, then full speed.
    let (bridge, _bridge_thread, _server) =
        bridge_with_first_block_stalled(std::time::Duration::from_millis(120));
    let mut batcher = Batcher::new(CHANNELS, CHANNELS, SampleFormat::Float32, BATCH_SIZE);

    let mut input = Planar::new();
    let mut output = Planar::new();
    let mut midi_out = MidiEventVec::new();
    let blocks = 6;
    // Which block each drained MIDI event was stamped by, per driving block.
    let mut stamps: Vec<Vec<u32>> = Vec::with_capacity(blocks);

    for block in 0..blocks {
        for ch in 0..CHANNELS {
            for i in 0..BATCH_SIZE {
                input.set_scalar(ch, i, ramp_sample(block, ch, i));
            }
        }
        output.clear();
        batcher.process(
            &bridge,
            BATCH_SIZE,
            &input.ins(),
            &mut output.outs(),
            &mut midi_out,
        );
        stamps.push(midi_out.iter().map(|e| e.data[0]).collect());
        wait_for_reply(&bridge, block as u64 + 1, WAIT_BUDGET);
    }

    assert!(
        !bridge.is_crashed(),
        "the stalled first block killed the bridge, so nothing below is a \
         statement about pairing"
    );

    // **The deterministic gate.** Everything else the drain does is invisible by
    // construction — the audio it recovers is stale and rejected by the slab's
    // sequence check, and the MIDI it forwards is indistinguishable from MIDI
    // that arrived on time — so a test that only inspects those observes the
    // drain's absence statistically, if at all. An earlier version of this test
    // asserted on stamps alone and passed 12 times out of 12 with `settle`
    // stubbed out: it was claiming coverage it did not have.
    //
    // `settled_replies` counts what the drain actually took off the socket, so
    // stubbing `settle` makes this fail every run rather than some of them.
    //
    // It is bounded by `abandoned_replies`, not pinned to 1. Block 1's 120 ms
    // stall is past `MAX_PROCESS_TIMEOUT`, so at least that one reply is
    // abandoned on any machine; but every later block has the same few-ms
    // budget, and on a loaded machine (macOS CI, three cores, the whole
    // workspace running beside it) one of them missing it is a property of the
    // scheduler, not of the drain. An exact `== 1` read 2 or 3 there. What the
    // drain owes is what was abandoned, so that is the bound.
    //
    // Mutations: stub `settle` to `0` → `settled == 0` → the second assert fails
    // every run. Over-count the drain (`note_settled(taken * 2)`) → `settled`
    // exceeds `abandoned` → the third fails (every run on an unloaded machine,
    // where block 1 is the only one abandoned).
    // `settled` first: its `Acquire` makes every `abandoned` bump that owed
    // those replies visible to the read after it.
    let settled = bridge.settled_replies();
    let abandoned = bridge.abandoned_replies();
    assert!(
        abandoned >= 1,
        "block 1 stalled 120 ms against a budget of at most {:?}, yet no reply \
         was abandoned — the rig did not stage the timeout this test is about",
        std::time::Duration::from_millis(50),
    );
    assert!(
        settled >= 1,
        "the drain took no owed replies although {abandoned} were abandoned. \
         `Owed::settle` never ran or never collected anything, and the abandoned \
         frame is still queued — which the stamps below cannot reliably detect."
    );
    assert!(
        settled <= abandoned,
        "the drain took {settled} replies off the socket but only {abandoned} \
         were ever abandoned: the drain's count no longer matches what it owes."
    );

    // The server stamps block `seq`'s reply with `data[0] == seq` (not its
    // frame offset, which a late reply's collection moves to 0). Every
    // stamp the host ever drains must therefore be a sequence it actually
    // submitted, and no sequence may arrive twice — a reply left undrained
    // shows up as the same stamp reappearing behind a later block.
    let mut seen: Vec<u32> = stamps.iter().flatten().copied().collect();
    let before = seen.len();
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(
        seen.len(),
        before,
        "the same reply was drained twice: stamps {stamps:?}. A reply the bridge \
         abandoned was left on the stream and re-read behind a later block."
    );
    for &stamp in &seen {
        assert!(
            stamp >= 1 && stamp as usize <= blocks,
            "drained a reply stamped {stamp}, which is not one of the {blocks} \
             blocks this test submitted: {stamps:?}"
        );
    }

    // The abandoned block's reply must have reached the host, not merely been
    // consumed off the socket: `settle` forwards it, and dropping it silently
    // loses the plugin's MIDI for any block whose reply ran late.
    assert!(
        seen.contains(&1),
        "block 1's reply — the one whose budget expired — was drained off the \
         socket but never forwarded to the host: stamps {stamps:?}. Its audio \
         is moot, but its MIDI-out is the plugin's real output and the socket \
         is its only path."
    );
}

/// A doubling server that stalls only its **first** reply, then runs at full
/// speed — a plugin that hitches once (a page fault, a scheduler delay) rather
/// than one that is permanently slow.
///
/// A duration is the right shape here, unlike in the backlog test: this only
/// needs the first reply to miss its budget, and 120 ms against a ~2 ms budget
/// is not a race on any machine.
fn bridge_with_first_block_stalled(
    stall: std::time::Duration,
) -> (Arc<PluginBridge>, BridgeThread, std::thread::JoinHandle<()>) {
    stamped_server(stall, true)
}

/// The doubling server of [`bridge_with_server_stall`], with two additions the
/// timeout tests need.
///
/// `first_only` stalls just the opening block rather than every one — a plugin
/// that hitches once versus one that is permanently behind, which are different
/// properties and need different rigs.
///
/// Every reply's MIDI-out carries one event stamped with the sequence it
/// answers. The reply is the only thing that crosses the socket which the host
/// consumes at all — audio travels through the slab and is gated on the slot's
/// own sequence — so this stamp is the only way a test can observe *which block*
/// an arriving frame belongs to. `frame_offset` is a label here and nothing
/// interprets it as a position.
fn stamped_server(
    stall: std::time::Duration,
    first_only: bool,
) -> (Arc<PluginBridge>, BridgeThread, std::thread::JoinHandle<()>) {
    use interprocess::local_socket::{traits::Listener as _, ListenerOptions};

    let path = unique_socket_path("stamped");
    let _ = std::fs::remove_file(&path);
    let name = crate::util::transport::control::socket_name(&path).unwrap();
    let listener = ListenerOptions::new().name(name).create_sync().unwrap();

    let layout = stereo_layout();
    let shm_name = unique_shm_name("stamped");
    let host_slab = Arc::new(AudioSlab::create(shm_name.clone(), layout.clone()).unwrap());
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
        .name("mock-stamped-server".to_string())
        .spawn(move || {
            struct Cleanup(std::path::PathBuf);
            impl Drop for Cleanup {
                fn drop(&mut self) {
                    let _ = std::fs::remove_file(&self.0);
                }
            }
            let _cleanup = Cleanup(path_cleanup);
            let mut scratch = vec![0.0f32; BATCH_SIZE];
            let mut stalled_once = false;
            loop {
                let msg = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    recv_host_msg(&server_stream)
                })) {
                    Ok(Some(msg)) => msg,
                    Ok(None) | Err(_) => break,
                };
                let reply = match msg {
                    HostMessage::ProcessAudio(data) => {
                        if !stall.is_zero() && !(first_only && stalled_once) {
                            stalled_once = true;
                            std::thread::sleep(stall);
                        }
                        let n = data.num_samples;
                        let seq = data.seq;
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
                            server_slab.publish_output(seq);
                        }
                        let midi_out = vec![crate::protocol::IpcMidiEvent {
                            frame_offset: 0,
                            data: [seq as u32, 0, 0, 0],
                        }];
                        BridgeMessage::AudioProcessed {
                            latency_us: 0,
                            seq,
                            midi_out,
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

    std::thread::sleep(std::time::Duration::from_millis(50));

    (bridge, bridge_thread, server_thread)
}

/// A host whose every chunk carries `midi` MIDI events, recording what the
/// batcher reports lost: chunks not sent ([`Chunks::unsent`]) and the MIDI
/// of chunks the bridge refused ([`Chunks::refused`]).
///
/// [`Chunks::unsent`]: super::batcher::Chunks::unsent
/// [`Chunks::refused`]: super::batcher::Chunks::refused
#[derive(Default)]
struct LossProbe {
    midi: usize,
    unsent: usize,
    refused: Vec<usize>,
    out: MidiEventVec,
}

impl super::batcher::Chunks for LossProbe {
    fn begin(&mut self, _at: usize, _chunk: usize) {}
    fn take(&mut self, _from: usize, _n: usize, _at: usize) {}

    fn payload(&mut self, _frames: usize) -> BlockPayload {
        let note = crate::protocol::MidiEvent::note_on(
            tutti_midi_types::MidiGroup::FIRST,
            tutti_midi_types::MidiChannel::FIRST,
            60,
            0x8000,
        );
        BlockPayload {
            midi: std::iter::repeat_n(note, self.midi).collect(),
            ..BlockPayload::default()
        }
    }

    fn unsent(&mut self) {
        self.unsent += 1;
    }

    fn refused(&mut self, midi: usize) {
        self.refused.push(midi);
    }

    fn midi_out(&mut self, events: &MidiEventVec) {
        self.out.clone_from(events);
    }

    fn emit(&mut self, _frame: usize, _event: crate::protocol::MidiEvent) {}
}

/// **A chunk whose audio cannot be staged reports its MIDI lost**, so the
/// plugin node counts it and cuts the SysEx open in it. A batcher wider
/// than the slab fails to stage its third channel on every chunk.
///
/// Mutation (run): the `host.unsent()` call removed from `submit` → nothing
/// reported → fails.
#[test]
fn an_unstaged_chunk_reports_its_midi_lost() {
    let _lock = exclusive();
    let (bridge, _thread, _server) = bridge_with_doubling_server();
    let mut batcher = Batcher::new(3, CHANNELS, SampleFormat::Float32, BATCH_SIZE);
    let silence = vec![0.0f32; BATCH_SIZE];
    let mut out = Planar::new();
    let mut host = LossProbe {
        midi: 4,
        ..LossProbe::default()
    };
    for _ in 0..3 {
        batcher.process(
            &bridge,
            BATCH_SIZE,
            &[&silence, &silence, &silence],
            &mut out.outs(),
            &mut host,
        );
    }
    assert_eq!(host.unsent, 3);
    assert!(host.refused.is_empty());
}

/// **A chunk the bridge refuses reports its MIDI lost, with how many events
/// it carried.** A bridge that never connected is crashed, and refuses
/// every chunk.
///
/// Mutation (run): the `host.refused(midi)` call removed from `submit` →
/// nothing reported → fails.
#[test]
fn a_refused_chunk_reports_its_midi_lost() {
    let _lock = exclusive();
    let path = unique_socket_path("refused");
    let _ = std::fs::remove_file(&path);
    let slab = Arc::new(AudioSlab::create(unique_shm_name("refused"), stereo_layout()).unwrap());
    let (bridge, _thread) =
        PluginBridge::new(path, slab, std::path::PathBuf::from("test.vst3"), 48_000.0).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !bridge.is_crashed() {
        assert!(std::time::Instant::now() < deadline, "never crashed");
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    let mut batcher = Batcher::new(CHANNELS, CHANNELS, SampleFormat::Float32, BATCH_SIZE);
    let input = Planar::new();
    let mut out = Planar::new();
    let mut host = LossProbe {
        midi: 7,
        ..LossProbe::default()
    };
    for _ in 0..2 {
        batcher.process(
            &bridge,
            BATCH_SIZE,
            &input.ins(),
            &mut out.outs(),
            &mut host,
        );
    }
    assert_eq!(host.refused, vec![7, 7]);
    assert_eq!(host.unsent, 0);
}

/// **A block the bridge thread skips as stale never lets the SysEx open in
/// it through with a gap.** The mock server holds its `Ready` until four
/// blocks are queued, so the bridge thread finds the first two stale
/// (`RING_SLOTS` = 2) and skips them: their MIDI is lost, counted, and the
/// rest of the message open in them (in block 3) is not sent either. The
/// message after it, and a note, arrive.
///
/// Mutation (run): the bridge's `retain` through its intake removed → the
/// cut message's tail reaches the server → fails. Mutation (run): the stale
/// branch's `owed.intake.lost()` removed, or its loop admitting the stale
/// payload's events → the same → fails. Mutation (run): the stale payload's
/// MIDI not counted → the count is short by six → fails.
#[test]
fn a_stale_block_never_lets_a_sysex_through_with_a_gap() {
    use tutti_midi_types::{MidiChannel, MidiGroup};

    let _lock = exclusive();
    let cut = sysex_fragments(60); // 10 packets
    let next = sysex_fragments(12); // 2 packets
    let note = crate::protocol::MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 1);
    let (three, four, dropped) = two_stale_blocks([
        cut[..3].to_vec(),
        cut[3..6].to_vec(),
        cut[6..]
            .iter()
            .chain(&next)
            .copied()
            .chain([note])
            .collect(),
        vec![note],
    ]);
    let mut expected_three = next.clone();
    expected_three.push(note);
    assert_eq!(three, expected_three, "the cut message's tail is not sent");
    assert_eq!(four, vec![note]);
    assert_eq!(
        dropped,
        (3 + 3 + 4) as u64,
        "blocks 1 and 2, and the cut message's tail"
    );
}

/// **A stale block holding only a message's `START` cuts that message.**
/// The bridge must record the `START` as open before it marks the block's
/// open messages cut, or the message's `END` in the next block sent passes.
///
/// Mutation (run): the stale branch calling `owed.intake.lost()` before
/// admitting the payload's events → the `END` is sent → fails.
#[test]
fn a_stale_block_holding_only_a_start_cuts_its_message() {
    use tutti_midi_types::{MidiChannel, MidiGroup};

    let _lock = exclusive();
    let lost = sysex_fragments(12); // a START and an END
    let other = sysex_fragments(20);
    let note = crate::protocol::MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 1);
    // The `START` in the last stale block: an earlier one's `lost` would
    // cut it anyway.
    let (three, four, dropped) = two_stale_blocks([
        vec![note],
        vec![lost[0]],
        [lost[1]].into_iter().chain(other.iter().copied()).collect(),
        vec![note],
    ]);
    assert_eq!(three, other, "the lost START's END is not sent");
    assert_eq!(four, vec![note]);
    assert_eq!(dropped, 3, "blocks 1 and 2, and the END");
}

/// `len` payload bytes as SysEx7 packets on group 1.
fn sysex_fragments(len: usize) -> Vec<crate::protocol::MidiEvent> {
    let payload: Vec<u8> = (0..len).map(|i| (i % 0x7F) as u8).collect();
    let mut out = Vec::new();
    crate::protocol::MidiEvent::sysex7_fragments(
        tutti_midi_types::MidiGroup::FIRST,
        &payload,
        &mut out,
    );
    out
}

/// Submits four blocks of MIDI before the mock server says `Ready`, so the
/// bridge thread finds blocks 1 and 2 stale (`RING_SLOTS` = 2) and skips
/// them. Returns the MIDI the server got in blocks 3 and 4, and the
/// bridge's count of the MIDI it did not send.
fn two_stale_blocks(
    blocks: [Vec<crate::protocol::MidiEvent>; 4],
) -> (
    Vec<crate::protocol::MidiEvent>,
    Vec<crate::protocol::MidiEvent>,
    u64,
) {
    use interprocess::local_socket::{traits::Listener as _, ListenerOptions};

    assert_eq!(RING_SLOTS, 2, "blocks 1 and 2 of 4 are stale");
    let path = unique_socket_path("stale-midi");
    let _ = std::fs::remove_file(&path);
    let name = crate::util::transport::control::socket_name(&path).unwrap();
    let listener = ListenerOptions::new().name(name).create_sync().unwrap();
    let slab = Arc::new(AudioSlab::create(unique_shm_name("stale-midi"), stereo_layout()).unwrap());
    let (bridge, thread) = PluginBridge::new(
        path.clone(),
        slab,
        std::path::PathBuf::from("test.vst3"),
        48_000.0,
    )
    .unwrap();
    let server_stream = listener.accept().unwrap();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let (got_tx, got) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        released.recv().unwrap();
        send_bridge_msg(
            &server_stream,
            &BridgeMessage::Ready {
                protocol_version: PROTOCOL_VERSION,
            },
        );
        while let Some(msg) = recv_host_msg(&server_stream) {
            if let HostMessage::ProcessAudio(data) = msg {
                let midi: Vec<crate::protocol::MidiEvent> = data
                    .midi_events
                    .iter()
                    .map(crate::protocol::IpcMidiEvent::to_midi_event)
                    .collect();
                let _ = got_tx.send((data.seq, midi));
                send_bridge_msg(
                    &server_stream,
                    &BridgeMessage::AudioProcessed {
                        latency_us: 0,
                        seq: data.seq,
                        midi_out: Default::default(),
                    },
                );
            }
        }
        let _ = std::fs::remove_file(&path);
    });

    for (i, midi) in blocks.iter().enumerate() {
        let submitted = bridge.submit(
            i as u64 + 1,
            BATCH_SIZE,
            midi.iter().copied().collect(),
            &mut crate::protocol::ParameterChanges::new(),
            Default::default(),
            Default::default(),
            Default::default(),
        );
        assert!(submitted.block);
    }
    release.send(()).unwrap();
    let wait = std::time::Duration::from_secs(5);
    let (seq, three) = got.recv_timeout(wait).expect("block 3 sent");
    assert_eq!(seq, 3);
    let (seq, four) = got.recv_timeout(wait).expect("block 4 sent");
    assert_eq!(seq, 4);
    let dropped = bridge
        .dropped_midi_cell()
        .load(std::sync::atomic::Ordering::Relaxed);
    drop(thread);
    drop(bridge);
    server.join().unwrap();
    (three, four, dropped)
}
